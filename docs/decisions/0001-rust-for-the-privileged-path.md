# 0001: Rust for everything that touches the kernel

**Context.** The spawner, the shim and the zygote library run between `fork` and `exec` or call `unshare(CLONE_NEWUSER)`, which fails with `EINVAL` in a multithreaded process. The daemon parses requests while holding privileges in its user namespace.

**Decision.** Rust throughout the kernel-facing path. The daemon is async (tokio and tonic). The spawner and shim are single-threaded binaries with no async runtime. The process model keeps anything multithreaded from ever forking a sandbox: the daemon enters its namespaces and starts the spawner *before* the tokio runtime exists, and the spawner asserts one thread at start-up.

**Alternatives.** Go cannot fork without exec and is multithreaded from the start; runc needs a C constructor (`nsexec`) to work around it. Python is used only to prototype. C++20 was viable; Rust buys memory safety in code that parses untrusted frames.

**Evidence.** `sandfork-ns` and the spawner check thread counts; the zygote's hygiene check rejects a second thread (`the_test_harness_itself_fails_the_thread_check`).
