# Branching rollouts: prefix sharing on both sides

Code: `orchestrator._branched_group`, `Fork` in the API.

## The shape

A group of G leaves is grown as G/b trunks, each run to turn k, snapshotted,
and forked into b branches. The comparison is always at equal G: a flat
group of G independent rollouts against a tree with G leaves.

* **Environment side.** At turn k the trunk sandbox is sealed (an
  incremental layer, `layers.md`) and forked b ways. What is saved is
  re-running k turns of commands b − 1 times: the builds, the test runs.
* **Model side.** Every branch's context starts with the trunk's token IDs,
  identical because nothing is ever re-tokenized, so a prefix cache serves
  the shared prefill. `ScriptedGenerator` keeps a block-hash prefix cache
  (16-token blocks, each hash chained on the previous block's) to count
  this. Against TokenMill the hit rate comes from its own metrics.

Tasks whose state lives in a process (data-analysis) cannot be branched in
this build: that needs process checkpoints. The daemon refuses with
`branch_needs_process_state` rather than silently dropping the kernel's
state.

## Is a tree still a valid GRPO group?

Each leaf is still a sample from the policy: the trunk was sampled from it,
and so was each continuation. But leaves that share a trunk are correlated,
so the group mean is a noisier baseline and the group behaves as if it were
smaller than G. Measuring that correlation needs a model and is open (E6's
learning comparison).

What branching could add, as a labelled experiment: siblings share
everything up to turn k, so their reward differences come from choices after
it. That allows a step-level advantage at the branch point, applied to the
tokens after the branch. It is not implemented.

## Tested

`test_branching_shares_the_trunk` checks a 2×2 tree end to end: two trunks,
each with two branches. Branches share their trunk token for token, run in
separate sandboxes, and are all verified, and the generator served part of
their prefill from the prefix cache.

## Measured

E6 in `benchmarks.md` covers flat G = 8 against 2×4 and 4×2 trees on a
py-heavy task whose first two turns are expensive, measuring command
seconds and prefill tokens per leaf.
