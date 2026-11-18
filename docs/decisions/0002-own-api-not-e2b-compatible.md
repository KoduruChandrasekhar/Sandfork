# 0002: Our own API, not a vendor-compatible one

**Context.** Existing sandbox APIs are designed around one sandbox. GRPO's unit of work is a group, admitted as a gang and released together.

**Decision.** One gRPC service, `sandfork.v1`, with `ForkGroup` as the central call, idempotent mutating calls, three error classes and leases. A small Gym-style adapter covers frameworks that expect `reset`/`step`.

**Alternatives.** Imitating a vendor API would force per-sandbox semantics and no gang admission.
