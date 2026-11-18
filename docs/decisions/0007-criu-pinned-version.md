# 0007: CRIU pinned at 4.2.1 or newer, and deferred

**Context.** CRIU 4.2.1 fixes CVE-2026-73860, and CVE-2026-18107 concerns tampered images. The development machine's distribution packages 4.2, and the machine has no passwordless root.

**Decision.** Any CRIU integration must require 4.2.1 or newer, run CRIU only in a separate root helper that alone writes the image directory, and verify credentials after every restore. Process checkpoints are **not implemented in this build**: checkpoints are filesystem-only, and fork mode F4 is reported as unavailable. Tasks that keep state in a process cannot be branched or restored, and the daemon says so rather than silently dropping that state.
