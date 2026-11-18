"""A deliberately small trainer: one policy, LoRA or full fine-tuning,
token-level clipped policy gradient.

* Base weights frozen in fp16 on GPU (fp32 on CPU). LoRA adapters in fp32,
  computation in fp16 with dynamic loss scaling where CUDA is available.
* AdamW, gradients clipped at 1.0.
* Batches packed by length, gradient accumulation across the step's rollouts.
* The mismatch between the generator's recorded log-probabilities and the
  trainer's recomputed ones is logged every step: it grows before training
  collapses (Qi et al.).
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Dict, List, Optional, Sequence

from .grpo import LossConfig, policy_loss, token_logprobs
from .trajectory import Trajectory


@dataclass
class TrainConfig:
    lr: float = 1e-5
    weight_decay: float = 0.0
    grad_clip: float = 1.0
    #: Tokens per micro-batch (packed by length).
    micro_batch_tokens: int = 8192
    optimizer_steps: int = 1
    loss: LossConfig = field(default_factory=LossConfig)
    lora_rank: int = 0
    lora_alpha: int = 32
    lora_targets: Sequence[str] = ("q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj", "c_attn", "c_proj", "c_fc")
    activation_checkpointing: bool = False


def apply_lora(model, cfg: TrainConfig):
    from peft import LoraConfig, get_peft_model

    names = {n.split(".")[-1] for n, _ in model.named_modules()}
    targets = [t for t in cfg.lora_targets if t in names]
    if not targets:
        raise ValueError(f"none of the LoRA targets {list(cfg.lora_targets)} exist in this model")
    lc = LoraConfig(r=cfg.lora_rank, lora_alpha=cfg.lora_alpha, target_modules=targets, lora_dropout=0.0, bias="none", task_type="CAUSAL_LM")
    model = get_peft_model(model, lc)
    for n, p in model.named_parameters():
        if p.requires_grad:
            p.data = p.data.float()
    return model


def disable_dropout(model) -> int:
    """Set every dropout probability to zero. With dropout on, the trainer's
    recomputed log-probabilities differ from the generator's even with
    identical weights, which shows up as training-inference mismatch and
    biases the importance ratio. Returns the number of modules changed."""
    import torch

    n = 0
    for m in model.modules():
        if isinstance(m, torch.nn.Dropout) and m.p != 0:
            m.p = 0.0
            n += 1
    cfg = getattr(model, "config", None)
    for attr in ("attention_dropout", "hidden_dropout", "resid_pdrop", "embd_pdrop", "attn_pdrop", "dropout"):
        if cfg is not None and isinstance(getattr(cfg, attr, None), float):
            setattr(cfg, attr, 0.0)
    return n


def pack(trajs: Sequence[Trajectory], max_tokens: int) -> List[List[int]]:
    """Group trajectory indices into micro-batches of similar length."""
    order = sorted(range(len(trajs)), key=lambda i: len(trajs[i]))
    batches: List[List[int]] = []
    cur: List[int] = []
    longest = 0
    for i in order:
        n = len(trajs[i])
        if cur and max(longest, n) * (len(cur) + 1) > max_tokens:
            batches.append(cur)
            cur, longest = [], 0
        cur.append(i)
        longest = max(longest, n)
    if cur:
        batches.append(cur)
    return batches


class Trainer:
    def __init__(self, model, cfg: TrainConfig, *, device: str = "cpu", ref_model=None):
        import torch

        self.cfg = cfg
        self.device = device
        if cfg.lora_rank > 0:
            model = apply_lora(model, cfg)
        disable_dropout(model)
        if cfg.activation_checkpointing and hasattr(model, "gradient_checkpointing_enable"):
            model.gradient_checkpointing_enable()
        self.model = model.to(device)
        self.ref_model = ref_model
        params = [p for p in self.model.parameters() if p.requires_grad]
        self.opt = torch.optim.AdamW(params, lr=cfg.lr, weight_decay=cfg.weight_decay)
        self.scaler = torch.amp.GradScaler("cuda", enabled=device.startswith("cuda"))
        self.version = 0

    def _tensors(self, trajs: Sequence[Trajectory], idx: List[int], advs: Sequence[float]):
        import torch

        T = max(len(trajs[i]) for i in idx)
        ids = torch.zeros(len(idx), T, dtype=torch.long)
        mask = torch.zeros(len(idx), T)
        attn = torch.zeros(len(idx), T, dtype=torch.long)
        lgen = torch.zeros(len(idx), T)
        adv = torch.zeros(len(idx))
        for row, i in enumerate(idx):
            t = trajs[i]
            n = len(t)
            ids[row, :n] = torch.tensor(t.ids)
            mask[row, :n] = torch.tensor(t.mask, dtype=torch.float)
            attn[row, :n] = 1
            lgen[row, :n] = torch.tensor(t.logp)
            adv[row] = advs[i]
        # The first token has no prefix and is never a model token.
        mask[:, 0] = 0
        d = self.device
        return ids.to(d), attn.to(d), mask.to(d), lgen.to(d), adv.to(d)

    def step(self, trajs: Sequence[Trajectory], advs: Sequence[float]) -> Dict[str, float]:
        """One training step on a batch of trajectories with their advantages.
        Gradients accumulate over micro-batches, weighted so the loss is the
        token-level mean over the *whole* batch."""
        import torch

        t0 = time.monotonic()
        self.model.train()
        total_tokens = sum(t.model_tokens for t in trajs) or 1
        metrics: Dict[str, float] = {"mismatch_mean": 0.0, "mismatch_max": 0.0, "ratio_off_frac": 0.0, "clip_frac": 0.0, "loss": 0.0}
        timing = {"forward": 0.0, "backward": 0.0, "optimizer": 0.0}
        for _ in range(self.cfg.optimizer_steps):
            self.opt.zero_grad(set_to_none=True)
            for idx in pack(trajs, self.cfg.micro_batch_tokens):
                ids, attn, mask, lgen, adv = self._tensors(trajs, idx, advs)
                f0 = time.monotonic()
                with torch.autocast(device_type="cuda", dtype=torch.float16, enabled=self.device.startswith("cuda")):
                    lnew = token_logprobs(self.model, ids, attn)
                    lref = None
                    if self.cfg.loss.kl_coef > 0 and self.ref_model is not None:
                        with torch.no_grad():
                            lref = token_logprobs(self.ref_model, ids, attn)
                    loss, m = policy_loss(lnew, lgen, adv, mask, self.cfg.loss, logp_ref=lref)
                # Rescale so micro-batches sum to the batch's token-level mean.
                weight = m["tokens"] / total_tokens
                timing["forward"] += time.monotonic() - f0
                b0 = time.monotonic()
                self.scaler.scale(loss * weight).backward()
                timing["backward"] += time.monotonic() - b0
                for k in ("mismatch_mean", "ratio_off_frac", "clip_frac", "loss"):
                    metrics[k] += m[k] * weight
                metrics["mismatch_max"] = max(metrics["mismatch_max"], m["mismatch_max"])
            o0 = time.monotonic()
            self.scaler.unscale_(self.opt)
            gnorm = torch.nn.utils.clip_grad_norm_([p for p in self.model.parameters() if p.requires_grad], self.cfg.grad_clip)
            self.scaler.step(self.opt)
            self.scaler.update()
            timing["optimizer"] += time.monotonic() - o0
            metrics["grad_norm"] = float(gnorm)
        self.version += 1
        metrics.update({f"time_{k}": v for k, v in timing.items()})
        metrics["time_train"] = time.monotonic() - t0
        metrics["model_tokens"] = float(total_tokens)
        metrics["policy_version"] = float(self.version)
        return metrics

    def adapter_state(self) -> Dict[str, "object"]:
        """The LoRA A and B matrices: what an adapter push sends."""
        return {n: p.detach().to("cpu").clone() for n, p in self.model.named_parameters() if p.requires_grad}
