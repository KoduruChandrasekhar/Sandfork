# The rollout orchestrator and the minimal GRPO loop

Code: `python/sandfork_rl/`. The RL loop is the test harness, not the
product. It drives Sandfork the way a real trainer would.

## One synchronous step (`orchestrator.run_step`)

1. The scheduler picks P tasks (`PickTasks`).
2. `ForkGroup(task, G)` for each.
3. All P × G rollouts run concurrently with asyncio.
4. Every final state is verified.
5. Advantages are computed within each group.
6. The trainer takes one optimizer step (or `--optimizer-steps`).
7. The generator's policy version is bumped. With the in-process `HFGenerator`
   the trainer and generator share one model, so weight sync is free.
8. Groups are released and the step's trace and metrics are written.

`--async-staleness k` switches to asynchronous mode. Producers keep
generating groups with the newest weights, the trainer takes completed groups
from a bounded buffer, and samples whose oldest token is more than k
versions old are dropped.

## Token in, token out

Never decode the conversation and tokenize it again. Byte-pair encoding of
the same text can produce different IDs than the model generated, most often
where generated text meets an appended observation, and training on them is
silently off-policy. A `Trajectory` is built only by appending IDs:

* the prompt's, with the chat template applied exactly once;
* each generated span exactly as the generator returned it, with the stop
  string included;
* each observation's.

Every generated span is recorded, and `assert_token_exact` checks them ID
for ID against what the generator returned. `run_step` runs that check on
every trajectory by default.

## Loss masks

Mask is 1 on generated tokens, 0 on prompt and observation tokens.
Observations condition every later action, so they stay in the input, but
the policy did not choose them, so they carry no gradient.

`test_prompt_and_observation_tokens_get_exactly_zero_gradient` puts a
gradient hook on the input embeddings and checks two things: no position at
or after the last model token receives gradient, and changing an
observation position's log-probability leaves the loss unchanged.
`test_masked_positions_contribute_nothing_to_parameter_gradients` checks
that an all-zero mask gives zero gradient on every parameter.

## Advantages (`grpo.advantages`)

* `dr_grpo` (the default): subtract the group mean, no division by the
  standard deviation (decision 0005).
* `grpo`: divide by the sample standard deviation.
* `rloo`: subtract the mean of the other members.

Rollouts with an infrastructure error have no reward and are dropped; the
mean is over the rest, and a group with fewer than two valid rollouts is
dropped. Tested on fixed vectors.

## The loss (`grpo.policy_loss`)

    rho = exp(log pi_theta(y) − log pi_gen(y))
    L   = −(1 / Σ|y_i|) Σ_i Σ_t min(rho A_i, clip(rho, 1 − 0.2, 1 + 0.28) A_i)

* **Token-level aggregation** over the whole batch. Micro-batches are
  weighted by their token counts so gradient accumulation gives exactly the
  batch mean.
* **Asymmetric clipping**, DAPO's clip-higher.
* **Optional:** a KL penalty (k3 estimator, against a reference model) and
  truncated importance sampling capped at 2.

## The mismatch metric

Every step logs the mean and maximum of |log π_train − log π_gen| over model
tokens, and the fraction of tokens with |ρ − 1| > 0.2. In synchronous
training with one optimizer step, ρ would be exactly 1 in exact arithmetic,
so any deviation is training-inference mismatch.

This metric found a real bug in development: models built in `train()` mode
had dropout on, so recomputed log-probabilities differed from the
generator's with identical weights. The trainer now sets every dropout
probability to zero (`disable_dropout`), and
`test_ratio_is_one_and_mismatch_zero_when_recomputed_exactly` guards it.

## The trainer (`trainer.py`)

AdamW with gradients clipped at 1.0. Optional LoRA through PEFT, with
adapter weights in fp32 and fp16 autocast with dynamic loss scaling on CUDA.
Batches are packed by length. Activation checkpointing is optional.

`test_a_step_moves_the_policy_toward_positive_advantage` checks the
direction of an update.

## Generators (`generate.py`)

| Generator | Use |
|---|---|
| `ScriptedGenerator` | replays fixed tool calls; every non-GPU test and benchmark; has a block-hash prefix cache to count prefill reuse |
| `HFGenerator` | a local `transformers` model, in process, sampling token by token with log-probabilities and top-k entropy |
| `HTTPGenerator` | TokenMill's token-in/token-out endpoint, or vLLM's `/v1/completions` with `return_tokens_as_token_ids` (`integration.md`) |

## The trace

`trace.Trace` writes Chrome trace events, one track per rollout (queue,
fork, generation and command per turn, checkpoint, restore, verify) plus the
trainer and weight sync. Open `runs/<name>/trace-stepNNNN.json` in Perfetto.

## Status

The loop runs end to end against the daemon with scripted agents
(`tests/test_daemon.py`), and its trainer is tested on CPU with a tiny
random GPT-2. **No GPU run has been made.** These are open:

* the math-tool control run (reward rising over 100 steps);
* the py-bugfix run;
* the F1 control;
* the async-mode comparison;
* the 1,000-rollout token-exactness run with a real model.

Running them:

    python -m sandfork_rl.train --tasks tasks/math-tool --steps 100 -P 8 -G 8 \
        --model Qwen/Qwen2.5-0.5B-Instruct --lora-rank 32 --device cuda --out runs/math
