//! Failure injection at named points.
//!
//! `SANDFORK_FAILPOINTS=name[,name...]` makes each named point fail with an
//! infrastructure error the first time it is reached, and succeed afterwards.
//! Every injection point has a test asserting the documented recovery: the
//! error is `INFRA`, reconciliation cleans up, and an idempotent retry
//! succeeds.

use crate::{Result, SfError};
use std::collections::HashSet;
use std::sync::Mutex;

static FIRED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Fail once at `name` if it is listed in `SANDFORK_FAILPOINTS`.
pub fn point(name: &str) -> Result<()> {
    let Ok(spec) = std::env::var("SANDFORK_FAILPOINTS") else {
        return Ok(());
    };
    if !spec.split(',').any(|p| p.trim() == name) {
        return Ok(());
    }
    let mut fired = FIRED.lock().unwrap_or_else(|p| p.into_inner());
    if fired
        .get_or_insert_with(HashSet::new)
        .insert(name.to_string())
    {
        return Err(SfError::infra(
            "failpoint",
            format!("injected failure at {name}"),
        ));
    }
    Ok(())
}

/// The injection points that exist, for documentation and tests.
pub const POINTS: [&str; 6] = [
    "seal_before_copy",
    "seal_after_copy",
    "spawn",
    "admit_then_fork",
    "criu_dump",
    "verify_exec",
];
