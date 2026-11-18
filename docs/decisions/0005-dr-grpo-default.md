# 0005: Dr. GRPO advantages by default

**Context.** Original GRPO divides the mean-subtracted reward by the group's standard deviation, which up-weights groups with little variance: tasks that are nearly always passed or nearly always failed.

**Decision.** Subtract the group mean only (Liu et al., Understanding R1-Zero-Like Training). Original GRPO and leave-one-out are options. Token-level loss aggregation and asymmetric clipping (0.2 and 0.28) follow DAPO. No KL penalty by default.
