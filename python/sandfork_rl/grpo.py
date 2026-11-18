"""Group-relative advantages and the policy-gradient loss.

Rewards come from the verifier. A rollout with an infrastructure error has no
reward and is dropped; the group's advantages are computed over the rest, and
the whole group is dropped if fewer than two valid rollouts remain.

Advantage estimators (``mode``):

* ``dr_grpo`` (default): subtract the group mean, do not divide by the
  standard deviation. Dividing up-weights groups with little variance, which
  over-weights tasks that are nearly always passed or nearly always failed.
* ``grpo``: the original, mean-subtracted and divided by the standard deviation.
* ``rloo``: leave-one-out, subtract the mean of the *other* members.

Every model token of rollout i gets the same advantage A_i.

The loss, for every model token t of rollout i, with the behaviour
log-probability recorded at generation::

    rho = exp(log pi_theta(y) - log pi_gen(y))
    L   = -(1 / sum_i |y_i|) * sum_i sum_t min(rho A_i, clip(rho, 1 - eps_low, 1 + eps_high) A_i)

Token-level aggregation (DAPO): divide by the total number of model tokens in
the batch, not per sequence. Asymmetric clipping, ``eps_low = 0.2`` and
``eps_high = 0.28``. No KL penalty by default.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Dict, List, Optional, Sequence


def advantages(rewards: Sequence[Optional[float]], mode: str = "dr_grpo", eps: float = 1e-6) -> Optional[List[Optional[float]]]:
    """Advantages for one group. ``None`` rewards (infrastructure errors) get
    ``None``. Returns ``None`` for the whole group if fewer than two rewards
    are valid."""
    valid = [r for r in rewards if r is not None]
    if len(valid) < 2:
        return None
    n = len(valid)
    mean = sum(valid) / n
    out: List[Optional[float]] = []
    if mode == "dr_grpo":
        out = [None if r is None else r - mean for r in rewards]
    elif mode == "grpo":
        std = math.sqrt(sum((r - mean) ** 2 for r in valid) / (n - 1))
        out = [None if r is None else (r - mean) / (std + eps) for r in rewards]
    elif mode == "rloo":
        total = sum(valid)
        out = [None if r is None else r - (total - r) / (n - 1) for r in rewards]
    else:
        raise ValueError(f"unknown advantage mode {mode!r}")
    return out


def wasted_probability(p: float, g: int) -> float:
    """Chance that a group of ``g`` binary rewards at pass rate ``p`` is all
    equal, and so teaches nothing."""
    return p**g + (1 - p) ** g


@dataclass
class LossConfig:
    eps_low: float = 0.2
    eps_high: float = 0.28
    kl_coef: float = 0.0
    #: Truncated importance sampling against the training-inference mismatch:
    #: weight each token by min(pi_train_old / pi_gen, cap). 0 disables it.
    tis_cap: float = 0.0


def policy_loss(logp_new, logp_gen, adv, mask, cfg: LossConfig, logp_ref=None, logp_old=None):
    """The clipped, token-level policy-gradient loss.

    All tensors are ``[batch, seq]`` and aligned to the *predicted* token:
    ``logp_new[b, t]`` is the new policy's log-probability of token ``t``.
    ``mask`` is 1 on model-generated tokens only, so prompt and observation
    tokens contribute exactly zero gradient. Returns ``(loss, metrics)``.
    """
    import torch

    mask = mask.to(logp_new.dtype)
    n_tokens = mask.sum().clamp(min=1.0)
    # The ratio is taken against the policy that generated the tokens. In
    # synchronous training with one optimizer step per batch it is exactly 1
    # in exact arithmetic; any deviation is training-inference mismatch.
    log_ratio = (logp_new - logp_gen) * mask
    ratio = torch.exp(log_ratio)
    a = adv.unsqueeze(-1) if adv.dim() == 1 else adv
    unclipped = ratio * a
    clipped = torch.clamp(ratio, 1.0 - cfg.eps_low, 1.0 + cfg.eps_high) * a
    per_token = -torch.minimum(unclipped, clipped)
    if cfg.tis_cap > 0 and logp_old is not None:
        w = torch.exp((logp_old - logp_gen) * mask).clamp(max=cfg.tis_cap).detach()
        per_token = per_token * w
    if cfg.kl_coef > 0 and logp_ref is not None:
        # The k3 estimator of KL(pi_theta || pi_ref): non-negative, unbiased.
        d = (logp_ref - logp_new) * mask
        per_token = per_token + cfg.kl_coef * (torch.exp(d) - d - 1.0)
    loss = (per_token * mask).sum() / n_tokens
    with torch.no_grad():
        diff = (logp_new - logp_gen).abs() * mask
        clip_frac = (((ratio < 1 - cfg.eps_low) | (ratio > 1 + cfg.eps_high)).to(mask.dtype) * mask).sum() / n_tokens
        metrics = {
            "loss": float(loss),
            "tokens": float(n_tokens),
            "mismatch_mean": float(diff.sum() / n_tokens),
            "mismatch_max": float(diff.max()) if diff.numel() else 0.0,
            "ratio_off_frac": float((((ratio - 1).abs() > 0.2).to(mask.dtype) * mask).sum() / n_tokens),
            "clip_frac": float(clip_frac),
        }
    return loss, metrics


def token_logprobs(model, input_ids, attention_mask=None):
    """Log-probability of every token given its prefix, shape ``[B, T]``;
    position 0 (no prefix) is 0."""
    import torch

    out = model(input_ids=input_ids, attention_mask=attention_mask)
    logits = out.logits if hasattr(out, "logits") else out
    logp = torch.log_softmax(logits[:, :-1].float(), dim=-1)
    picked = logp.gather(-1, input_ids[:, 1:].unsqueeze(-1)).squeeze(-1)
    return torch.cat([torch.zeros_like(picked[:, :1]), picked], dim=1)


def summarize_groups(groups: Sequence[Sequence[Optional[float]]]) -> Dict[str, float]:
    """Group-level statistics for one step: how many groups had reward
    variance (and so taught something)."""
    useful = dropped = 0
    var_sum = 0.0
    for g in groups:
        valid = [r for r in g if r is not None]
        if len(valid) < 2:
            dropped += 1
            continue
        m = sum(valid) / len(valid)
        v = sum((r - m) ** 2 for r in valid) / len(valid)
        var_sum += v
        useful += v > 0
    n = max(len(groups), 1)
    return {"groups": float(len(groups)), "useful_groups": float(useful), "useful_frac": useful / n, "dropped_groups": float(dropped), "reward_var_mean": var_sum / n}
