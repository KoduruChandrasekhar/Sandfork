# 0003: A denylist seccomp policy first

**Context.** An allowlist is tighter but breaks more programs, and the task families run arbitrary toolchains (pytest, pip, compilers, shell utilities).

**Decision.** Allow by default and deny a documented list: mounts and namespaces; `clone` with namespace bits; `clone3` (with `ENOSYS`, so glibc falls back to `clone`); process introspection; high-risk kernel interfaces; host administration; legacy interfaces. Foreign architectures are killed. The policy is data, and every entry has a unit test.

**Alternatives.** A learned per-family allowlist (stretch goal S3).
