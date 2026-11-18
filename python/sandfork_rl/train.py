"""The minimal GRPO training loop.

    python -m sandfork_rl.train --tasks tasks/py-bugfix --steps 200 -P 8 -G 8 \\
        --model Qwen/Qwen2.5-0.5B-Instruct --lora-rank 32 --out runs/bugfix

    # CPU smoke run with a tiny random model, no download:
    python -m sandfork_rl.train --tasks tasks/math-tool --tiny --steps 3 -P 2 -G 4

Synchronous by default. ``--async-staleness k`` switches to asynchronous mode:
the generator keeps producing groups with the newest weights it has, the
trainer takes completed groups from a bounded buffer, and samples whose oldest
token is more than ``k`` policy versions old are dropped.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import sys
import time
from typing import List, Optional

from . import orchestrator as orch
from .generate import ByteTokenizer, HFGenerator, HFTokenizer, HTTPGenerator, ScriptedGenerator
from .grpo import LossConfig
from .rollout import RolloutConfig
from .tasks import find_tasks
from .trace import Trace


def tiny_model(vocab_size: int, seed: int = 0):
    """A small random GPT-2, built locally: for CPU tests and smoke runs."""
    import torch
    from transformers import GPT2Config, GPT2LMHeadModel

    torch.manual_seed(seed)
    cfg = GPT2Config(vocab_size=vocab_size, n_positions=4096, n_embd=64, n_layer=2, n_head=2, bos_token_id=256, eos_token_id=257, resid_pdrop=0.0, embd_pdrop=0.0, attn_pdrop=0.0)
    return GPT2LMHeadModel(cfg)


def build(args):
    """(tokenizer, generator, trainer or None)."""
    trainer = None
    if args.generator == "scripted":
        tok = ByteTokenizer()
        return tok, ScriptedGenerator(tok, json.load(open(args.script)) if args.script else ['<tool>{"name":"submit","args":{}}</tool>']), None
    if args.tiny:
        tok = ByteTokenizer()
        model = tiny_model(tok.vocab_size, args.seed)
    else:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer

        dtype = torch.float16 if args.device.startswith("cuda") else torch.float32
        tok = HFTokenizer(AutoTokenizer.from_pretrained(args.model))
        model = AutoModelForCausalLM.from_pretrained(args.model, torch_dtype=dtype)
        if args.lora_rank:
            for p in model.parameters():
                p.requires_grad_(False)
    if args.train:
        from .trainer import TrainConfig, Trainer

        tc = TrainConfig(lr=args.lr, lora_rank=args.lora_rank, loss=LossConfig(eps_low=args.eps_low, eps_high=args.eps_high, kl_coef=args.kl), optimizer_steps=args.optimizer_steps, activation_checkpointing=args.activation_checkpointing)
        trainer = Trainer(model, tc, device=args.train_device or args.device)
        model = trainer.model
    if args.generator == "hf":
        gen = HFGenerator(model, tok, device=args.train_device or args.device)
    else:
        gen = HTTPGenerator(args.generator_url, api=args.generator, model=args.model)
    return tok, gen, trainer


async def pick(client, args, n: int, step: int) -> List[str]:
    picks = await client.pick_tasks(n, group_size=args.group_size, strategy=args.strategy, family=args.family, seed=args.seed * 100003 + step, new_epoch=True)
    return [p.task_dir for p in picks]


async def run_sync(client, args, tok, gen, trainer, train_dirs, eval_dirs) -> None:
    cfg = make_step_config(args)
    for step in range(args.steps):
        dirs = await pick(client, args, args.prompts_per_step, step)
        res = await orch.run_step(client, dirs, tok, gen, cfg, step=step, trainer=trainer)
        extra = {"policy_version": gen.policy_version}
        if eval_dirs and args.eval_every and step % args.eval_every == 0:
            extra.update(await evaluate(client, args, tok, gen, eval_dirs, step))
        orch.write_step(args.out, res, extra)
        print(json.dumps({"step": step, **{k: round(v, 4) for k, v in res.metrics.items() if isinstance(v, float)}, **extra}), flush=True)


async def run_async(client, args, tok, gen, trainer, train_dirs, eval_dirs) -> None:
    """Producers keep generating groups; the trainer consumes from a bounded
    buffer and drops samples staler than ``k`` versions."""
    cfg = make_step_config(args)
    cfg.prompts_per_step = 1
    buf: asyncio.Queue = asyncio.Queue(maxsize=max(2, args.prompts_per_step * 2))
    stop = asyncio.Event()

    async def producer(pid: int) -> None:
        i = 0
        while not stop.is_set():
            dirs = await pick(client, args, 1, pid * 1_000_000 + i)
            res = await orch.run_step(client, dirs, tok, gen, cfg, step=i, trainer=None, trace=Trace())
            await buf.put(res)
            i += 1

    producers = [asyncio.create_task(producer(p)) for p in range(args.prompts_per_step)]
    try:
        for step in range(args.steps):
            batch, stale = [], 0
            t0 = time.monotonic()
            while len(batch) < args.prompts_per_step:
                res = await buf.get()
                version = trainer.version if trainer else 0
                kept = [t for t in res.trajectories if t.oldest_policy < 0 or version - t.oldest_policy <= args.async_staleness]
                stale += len(res.trajectories) - len(kept)
                res.trajectories = kept
                batch.append(res)
            groups = []
            for r in batch:
                by = {}
                for t in r.trajectories:
                    by.setdefault(t.group_id, []).append(t)
                groups.extend(by.values())
            m = orch.step_metrics(groups, cfg)
            m["wait_seconds"] = time.monotonic() - t0
            m["stale_dropped"] = float(stale)
            if trainer is not None:
                trajs, advs = orch.build_batch(groups, cfg.advantage)
                if trajs:
                    m.update(trainer.step(trajs, advs))
                    gen.policy_version = trainer.version
            os.makedirs(args.out, exist_ok=True)
            with open(os.path.join(args.out, "metrics.jsonl"), "a") as f:
                f.write(json.dumps({"step": step, "mode": "async", **m}) + "\n")
            print(json.dumps({"step": step, "mode": "async", **{k: round(v, 4) for k, v in m.items()}}), flush=True)
    finally:
        stop.set()
        for p in producers:
            p.cancel()


async def evaluate(client, args, tok, gen, eval_dirs, step: int) -> dict:
    """Pass@1 on the uniformly sampled held-out set: the arbiter against
    curriculum overfitting and reward hacking."""
    cfg = make_step_config(args)
    cfg.group_size = 1
    rng = random.Random(step)
    dirs = rng.sample(eval_dirs, min(len(eval_dirs), args.eval_size))
    res = await orch.run_step(client, dirs, tok, gen, cfg, step=step, trainer=None, trace=Trace())
    valid = [t.reward for t in res.trajectories if t.reward is not None]
    return {"eval_pass_at_1": sum(1 for r in valid if r >= 1.0) / max(len(valid), 1), "eval_reward": sum(valid) / max(len(valid), 1), "eval_n": len(valid)}


def make_step_config(args) -> orch.StepConfig:
    return orch.StepConfig(
        prompts_per_step=args.prompts_per_step,
        group_size=args.group_size,
        mode=args.mode,
        advantage=args.advantage,
        strategy=args.strategy,
        family=args.family,
        branch_factor=args.branch,
        branch_turn=args.branch_turn,
        rollout=RolloutConfig(max_new_tokens=args.max_new_tokens, max_context=args.max_context, temperature=args.temperature, checkpoint_every=args.checkpoint_every),
    )


def parse(argv: Optional[List[str]] = None):
    ap = argparse.ArgumentParser(description="Minimal GRPO loop on Sandfork")
    ap.add_argument("--socket", default=None)
    ap.add_argument("--tasks", required=True, help="a task directory root (all tasks under it are used)")
    ap.add_argument("--family", default="")
    ap.add_argument("--steps", type=int, default=10)
    ap.add_argument("-P", "--prompts-per-step", type=int, default=4)
    ap.add_argument("-G", "--group-size", type=int, default=8)
    ap.add_argument("--mode", default="auto", help="fork mode: auto, f1, f2, f3")
    ap.add_argument("--strategy", default="band", help="band (difficulty-band scheduling) or uniform")
    ap.add_argument("--advantage", default="dr_grpo", choices=["dr_grpo", "grpo", "rloo"])
    ap.add_argument("--branch", type=int, default=0, help="branch factor b (0: flat groups)")
    ap.add_argument("--branch-turn", type=int, default=2)
    ap.add_argument("--model", default="Qwen/Qwen2.5-0.5B-Instruct")
    ap.add_argument("--tiny", action="store_true", help="a tiny random model and byte tokenizer, built locally")
    ap.add_argument("--generator", default="hf", choices=["hf", "tokenmill", "vllm", "scripted"])
    ap.add_argument("--generator-url", default="http://127.0.0.1:8000")
    ap.add_argument("--script", default="", help="scripted generator: JSON list of action texts")
    ap.add_argument("--no-train", dest="train", action="store_false")
    ap.add_argument("--device", default="cpu")
    ap.add_argument("--train-device", default="")
    ap.add_argument("--lr", type=float, default=1e-5)
    ap.add_argument("--lora-rank", type=int, default=0)
    ap.add_argument("--eps-low", type=float, default=0.2)
    ap.add_argument("--eps-high", type=float, default=0.28)
    ap.add_argument("--kl", type=float, default=0.0)
    ap.add_argument("--optimizer-steps", type=int, default=1)
    ap.add_argument("--activation-checkpointing", action="store_true")
    ap.add_argument("--max-new-tokens", type=int, default=256)
    ap.add_argument("--max-context", type=int, default=4096)
    ap.add_argument("--temperature", type=float, default=1.0)
    ap.add_argument("--checkpoint-every", type=int, default=2)
    ap.add_argument("--async-staleness", type=int, default=-1, help="k >= 0 enables asynchronous mode")
    ap.add_argument("--eval-fraction", type=float, default=0.1)
    ap.add_argument("--eval-every", type=int, default=10)
    ap.add_argument("--eval-size", type=int, default=16)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", default="runs/latest")
    return ap.parse_args(argv)


async def amain(args) -> None:
    import sandfork

    all_dirs = [args.tasks] if os.path.exists(os.path.join(args.tasks, "task.toml")) else find_tasks(args.tasks, args.family)
    if not all_dirs:
        sys.exit(f"no tasks under {args.tasks}")
    rng = random.Random(args.seed)
    shuffled = list(all_dirs)
    rng.shuffle(shuffled)
    n_eval = int(len(shuffled) * args.eval_fraction) if len(shuffled) >= 10 else 0
    eval_dirs, train_dirs = shuffled[:n_eval], shuffled[n_eval:]
    tok, gen, trainer = build(args)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "config.json"), "w") as f:
        json.dump({k: v for k, v in vars(args).items()}, f, indent=2)
    async with sandfork.connect(args.socket) as client:
        await client.register_tasks(train_dirs)
        if args.async_staleness >= 0:
            await run_async(client, args, tok, gen, trainer, train_dirs, eval_dirs)
        else:
            await run_sync(client, args, tok, gen, trainer, train_dirs, eval_dirs)


def main(argv: Optional[List[str]] = None) -> None:
    asyncio.run(amain(parse(argv)))


if __name__ == "__main__":
    main()
