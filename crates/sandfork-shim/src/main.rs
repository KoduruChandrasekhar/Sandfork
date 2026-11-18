//! The shim binary. The spawner execs it as PID 1 of a new sandbox with the
//! control channel on descriptor 3.

use sandfork_shim::{run, ShimCfg};

fn main() {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let level = env("SANDFORK_LEVEL");
    let cfg = ShimCfg {
        namespaced: level != "l0",
        root: if level == "l0" {
            env("SANDFORK_ROOT")
        } else {
            "/".to_string()
        },
        agent_uid: env("SANDFORK_AGENT_UID")
            .parse()
            .unwrap_or(sandfork_core::AGENT_UID),
        agent_gid: env("SANDFORK_AGENT_GID")
            .parse()
            .unwrap_or(sandfork_core::AGENT_GID),
    };
    run(3, cfg)
}
