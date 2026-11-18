# Integration with the other projects

Each integration is optional; none of the core depends on them. None has
been exercised in this build: the other repositories are not here.

## TokenMill (needed for the GPU run)

`HTTPGenerator(api="tokenmill")` expects this contract:

```
POST /v1/generate
{"prompt_token_ids": [...], "max_new_tokens": 512, "stop": ["</tool>"],
 "include_stop": true, "logprobs": true, "temperature": 1.0}
-> {"token_ids": [...], "logprobs": [...], "policy_version": 7,
    "finish_reason": "stop", "cached_prompt_tokens": 1024}

POST /v1/weights
{"adapter_path": "/path/to/adapter", "version": 8}
```

What TokenMill must provide:

* **Token input with per-token log-probabilities.** The orchestrator never
  sends text.
* **The stop string included** in the returned tokens, so the trained
  sequence ends where the model's did.
* **Prefix caching:** hash each full KV block by the previous block's hash
  and its own token IDs, reuse matches on prefill, reference-count shared
  blocks, and evict unreferenced blocks LRU. Report `cached_prompt_tokens`.
* **Weight reload between requests,** with the version echoed in every
  response, so every generated span records the policy that produced it.

The vLLM reference path (`api="vllm"`) uses `/v1/completions` with the
prompt as token IDs, `logprobs=1` and `return_tokens_as_token_ids`.

Open: the contract test against a pinned TokenMill version in CI.

## ShardForge (optional)

Would replace the single-GPU LoRA trainer for full fine-tuning of a 1.5B
model across both T4s, with its fp16 loss scaler and exact-resume
checkpointing. Not attempted.

## Durable-Agent-Runtime (patterns)

Leases with fencing tokens are implemented: `RenewLease` refuses a stale
token. The event-sourced rollout log is not; the trajectory and trace files
are the audit trail for now.

## HNSW-Vector-DB (optional)

A trajectory index for deduplicating near-identical solutions and "show me
rollouts like this one". Not attempted.

## The demo script

`demo/full-stack.sh` is not written. Its GPU-free equivalent is
`make test-python`, which starts a daemon, runs scripted rollouts, branches,
recovers from a killed sandbox and verifies every result.
