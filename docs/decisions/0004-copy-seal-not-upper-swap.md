# 0004: Seal by copying the writable layer, never by swapping it

**Context.** Snapshotting a running sandbox needs its changes as an immutable layer.

**Decision.** Copy the upper directory into the store under a cgroup freeze, and keep the sandbox running on its own upper. Incremental seals copy only what changed since the last seal.

**Alternatives.** Swapping the upper directory of a live overlay is not supported: the mounted root cannot have its upper replaced underneath it. Remounting would break every open file and working directory in the sandbox.

**Evidence.** The overlay-against-model property test checks full seals, incremental seals and squashes against a plain-directory model.
