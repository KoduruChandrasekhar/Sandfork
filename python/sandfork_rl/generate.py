"""Tokenizers and generators.

A generator takes token IDs and returns token IDs plus the per-token
log-probability of the policy that produced them, and the version of that
policy. Implementations:

* ``ScriptedGenerator``: replays fixed tool-call sequences. It replaces the
  model in every non-GPU test and in the trace-driven load generator, so
  failures are reproducible.
* ``HFGenerator``: a local ``transformers`` model, in process. Small runs and
  CPU tests; shares weights with the trainer, so weight sync is free.
* ``HTTPGenerator``: TokenMill's token-in/token-out endpoint, or vLLM's
  ``/v1/completions`` with ``return_tokens_as_token_ids``, as the reference
  generator. The contract is in ``docs/integration.md``.
"""

from __future__ import annotations

import asyncio
import json
import math
import time
import urllib.request
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Protocol, Sequence


@dataclass
class GenOutput:
    token_ids: List[int]
    logprobs: List[float]
    policy_version: int
    finish_reason: str = "stop"
    #: Prompt tokens served from the prefix cache, when the generator reports it.
    cached_prompt_tokens: int = 0
    prompt_tokens: int = 0
    #: Mean token entropy from top-k log-probabilities, when available.
    entropy: float = 0.0
    seconds: float = 0.0


class Tokenizer(Protocol):
    eos_id: int

    def encode(self, text: str) -> List[int]: ...

    def decode(self, ids: Sequence[int]) -> str: ...

    def chat_prompt(self, system: str, user: str) -> List[int]: ...


class ByteTokenizer:
    """Bytes as tokens, plus a few specials. Deterministic and dependency-free;
    used by tests and by the scripted generator. ``encode(decode(x)) == x``
    holds for every sequence of byte tokens."""

    BOS, EOS, PAD = 256, 257, 258
    vocab_size = 259
    eos_id = EOS

    def encode(self, text: str) -> List[int]:
        return list(text.encode("utf-8"))

    def decode(self, ids: Sequence[int]) -> str:
        return bytes(i for i in ids if i < 256).decode("utf-8", errors="replace")

    def chat_prompt(self, system: str, user: str) -> List[int]:
        return [self.BOS] + self.encode(f"<system>{system}</system>\n<user>{user}</user>\n<assistant>")


class HFTokenizer:
    """A ``transformers`` tokenizer. The chat template is applied exactly once,
    to the prompt; everything after it is appended as IDs."""

    def __init__(self, tok):
        self.tok = tok
        self.eos_id = tok.eos_token_id if tok.eos_token_id is not None else 0

    def encode(self, text: str) -> List[int]:
        return self.tok.encode(text, add_special_tokens=False)

    def decode(self, ids: Sequence[int]) -> str:
        return self.tok.decode(list(ids), skip_special_tokens=False)

    def chat_prompt(self, system: str, user: str) -> List[int]:
        if getattr(self.tok, "chat_template", None):
            msgs = [{"role": "system", "content": system}, {"role": "user", "content": user}]
            return list(self.tok.apply_chat_template(msgs, add_generation_prompt=True, tokenize=True))
        return self.encode(f"{system}\n\n{user}\n\n")


class Generator(Protocol):
    policy_version: int

    async def generate(self, prompt_ids: List[int], *, max_new_tokens: int, stop: Sequence[str], temperature: float = 1.0) -> GenOutput: ...


def _stop_at(text_of, ids: List[int], stop: Sequence[str]) -> int:
    """Length of the shortest prefix of ``ids`` whose text contains a stop
    string, stop string included; ``len(ids)`` if none appears."""
    for n in range(1, len(ids) + 1):
        t = text_of(ids[:n])
        if any(s in t for s in stop):
            return n
    return len(ids)


class ScriptedGenerator:
    """Replays a fixed list of actions, one per turn, per rollout.

    ``script`` maps a rollout key to its list of action texts; the key comes
    from ``key_of(prompt_ids)`` (by default the whole prompt prefix), so
    branched rollouts that share a prefix replay the same trunk. Log-
    probabilities are synthetic but deterministic. ``seconds_per_token``
    simulates generation time for load tests.
    """

    def __init__(self, tokenizer: Tokenizer, actions: Sequence[str], *, seconds_per_token: float = 0.0, prefill_seconds_per_token: float = 0.0):
        self.tok = tokenizer
        self.actions = list(actions)
        self.policy_version = 0
        self.seconds_per_token = seconds_per_token
        self.prefill_seconds_per_token = prefill_seconds_per_token
        self.returned: Dict[int, List[List[int]]] = {}
        #: Prefix cache for the branching experiment: hashes of full blocks.
        self.block = 16
        self._blocks: Dict[int, int] = {}
        self.prefill_total = 0
        self.prefill_cached = 0

    def _prefix_hits(self, ids: List[int]) -> int:
        h, hit, cached = 0, 0, True
        for i in range(0, len(ids) - len(ids) % self.block, self.block):
            h = hash((h, tuple(ids[i : i + self.block])))
            if cached and h in self._blocks:
                hit += self.block
            else:
                cached = False
                self._blocks[h] = 1
        return hit

    async def generate(self, prompt_ids: List[int], *, max_new_tokens: int, stop: Sequence[str], temperature: float = 1.0, turn: int = 0) -> GenOutput:
        t0 = time.monotonic()
        text = self.actions[turn] if turn < len(self.actions) else self.actions[-1]
        ids = self.tok.encode(text)[:max_new_tokens]
        hits = self._prefix_hits(prompt_ids)
        self.prefill_total += len(prompt_ids)
        self.prefill_cached += hits
        delay = self.seconds_per_token * len(ids) + self.prefill_seconds_per_token * (len(prompt_ids) - hits)
        if delay > 0:
            await asyncio.sleep(delay)
        lp = [-0.1 - 0.001 * (i % 7) for i in range(len(ids))]
        return GenOutput(list(ids), lp, self.policy_version, "stop", hits, len(prompt_ids), 0.0, time.monotonic() - t0)


class HFGenerator:
    """Sample from a local ``transformers`` causal LM, token in, token out.

    Generation stops at the first stop string, which is included, so the
    sequence trained on ends exactly where the model's did. Log-probabilities
    are recomputed from the sampling distribution (temperature applied),
    which is the behaviour policy.
    """

    def __init__(self, model, tokenizer: Tokenizer, *, device: str = "cpu", top_k_entropy: int = 20):
        self.model = model
        self.tok = tokenizer
        self.device = device
        self.policy_version = 0
        self.top_k_entropy = top_k_entropy
        self._lock = asyncio.Lock()

    def _generate_sync(self, prompt_ids: List[int], max_new_tokens: int, stop: Sequence[str], temperature: float) -> GenOutput:
        import torch

        t0 = time.monotonic()
        model = self.model
        was_training = model.training
        model.eval()
        ids = torch.tensor([prompt_ids], device=self.device)
        out_ids: List[int] = []
        out_lp: List[float] = []
        entropies: List[float] = []
        past = None
        finish = "length"
        with torch.no_grad():
            inp = ids
            for _ in range(max_new_tokens):
                res = model(input_ids=inp, past_key_values=past, use_cache=True)
                past = res.past_key_values
                logits = res.logits[0, -1].float() / max(temperature, 1e-5)
                logp = torch.log_softmax(logits, dim=-1)
                tok = int(torch.multinomial(logp.exp(), 1).item())
                out_ids.append(tok)
                out_lp.append(float(logp[tok]))
                top = torch.topk(logp, min(self.top_k_entropy, logp.numel())).values
                p = top.exp() / top.exp().sum()
                entropies.append(float(-(p * p.log()).sum()))
                if tok == self.tok.eos_id:
                    finish = "eos"
                    break
                tail = self.tok.decode(out_ids[-64:])
                if any(s in tail for s in stop):
                    finish = "stop"
                    break
                inp = torch.tensor([[tok]], device=self.device)
        if was_training:
            model.train()
        ent = sum(entropies) / len(entropies) if entropies else 0.0
        return GenOutput(out_ids, out_lp, self.policy_version, finish, 0, len(prompt_ids), ent, time.monotonic() - t0)

    async def generate(self, prompt_ids: List[int], *, max_new_tokens: int, stop: Sequence[str], temperature: float = 1.0, turn: int = 0) -> GenOutput:
        async with self._lock:  # one model, one forward pass at a time
            return await asyncio.to_thread(self._generate_sync, prompt_ids, max_new_tokens, stop, temperature)


class HTTPGenerator:
    """A remote generator that accepts and returns token IDs.

    ``api="tokenmill"`` posts to ``{url}/v1/generate``::

        {"prompt_token_ids": [...], "max_new_tokens": 512, "stop": ["</tool>"],
         "include_stop": true, "logprobs": true, "temperature": 1.0}
        -> {"token_ids": [...], "logprobs": [...], "policy_version": 7,
            "finish_reason": "stop", "cached_prompt_tokens": 1024}

    ``api="vllm"`` uses ``{url}/v1/completions`` with the prompt as a list of
    IDs, ``logprobs=1`` and ``return_tokens_as_token_ids``.
    """

    def __init__(self, url: str, *, api: str = "tokenmill", model: str = "", timeout: float = 300.0):
        self.url = url.rstrip("/")
        self.api = api
        self.model = model
        self.timeout = timeout
        self.policy_version = 0

    def _post(self, path: str, body: dict) -> dict:
        req = urllib.request.Request(self.url + path, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=self.timeout) as r:
            return json.loads(r.read())

    def _generate_sync(self, prompt_ids: List[int], max_new_tokens: int, stop: Sequence[str], temperature: float) -> GenOutput:
        t0 = time.monotonic()
        if self.api == "tokenmill":
            r = self._post("/v1/generate", {"prompt_token_ids": prompt_ids, "max_new_tokens": max_new_tokens, "stop": list(stop), "include_stop": True, "logprobs": True, "temperature": temperature})
            self.policy_version = int(r.get("policy_version", self.policy_version))
            return GenOutput(list(r["token_ids"]), [float(x) for x in r["logprobs"]], self.policy_version, r.get("finish_reason", "stop"), int(r.get("cached_prompt_tokens", 0)), len(prompt_ids), 0.0, time.monotonic() - t0)
        r = self._post(
            "/v1/completions",
            {"model": self.model, "prompt": prompt_ids, "max_tokens": max_new_tokens, "stop": list(stop), "include_stop_str_in_output": True, "logprobs": 1, "temperature": temperature, "return_tokens_as_token_ids": True},
        )
        ch = r["choices"][0]
        lps = ch["logprobs"]
        ids = [int(t.split(":", 1)[1]) for t in lps["tokens"]]
        cached = int(r.get("usage", {}).get("prompt_tokens_details", {}) .get("cached_tokens", 0) or 0)
        return GenOutput(ids, [float(x) for x in lps["token_logprobs"]], self.policy_version, ch.get("finish_reason", "stop"), cached, len(prompt_ids), 0.0, time.monotonic() - t0)

    async def generate(self, prompt_ids: List[int], *, max_new_tokens: int, stop: Sequence[str], temperature: float = 1.0, turn: int = 0) -> GenOutput:
        return await asyncio.to_thread(self._generate_sync, prompt_ids, max_new_tokens, stop, temperature)

    def push_adapter(self, path: str, version: int) -> None:
        """Ask the generator to load a LoRA adapter and tag responses with ``version``."""
        self._post("/v1/weights", {"adapter_path": path, "version": version})
        self.policy_version = version


def entropy_from_logprobs(top_logprobs: Sequence[float]) -> float:
    ps = [math.exp(x) for x in top_logprobs]
    z = sum(ps) or 1.0
    return -sum((p / z) * math.log(p / z) for p in ps if p > 0)
