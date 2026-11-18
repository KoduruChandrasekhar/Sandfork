# 0006: Groups as the unit of admission and accounting

**Context.** A GRPO group finishes only when its slowest rollout does, and a partial group is useless. Two half-admitted groups can each hold what the other needs.

**Decision.** `ForkGroup(G)` is gang-admitted: all G or none, FIFO within a priority, head of line only, with a deadline. A failure to start any member tears the group down. Verification and restores have priority and a reserved share of the pool, so work in flight can always finish.

**Evidence.** `invariants_hold_under_random_load`; `verification_can_always_start_when_new_groups_fill_their_share` (a deadlock found while validating tasks).
