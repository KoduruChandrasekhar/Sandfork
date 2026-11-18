//! The most important test in the layer store: random file operations applied
//! through real overlay mounts and, in parallel, to a plain directory that
//! acts as the reference model.
//!
//! After every batch of operations the merged overlay view must equal the
//! model, and so must: the stack rebuilt from incremental (delta) seals, the
//! stack with a full seal, the stack with the deltas squashed into one layer,
//! and the userspace flatten of the whole stack.
//!
//! The test enters a user and mount namespace itself, so it has no test
//! harness (the harness is multithreaded, and `unshare(CLONE_NEWUSER)` fails
//! in a multithreaded process). `PROPTEST_CASES` sets the case count.

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use sandfork_layers::tree::{self, compare_trees, Owner};
use sandfork_layers::{Store, StoreCfg, XattrNs};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PATHS: [&str; 10] = [
    "a", "b", "c", "d1", "d1/a", "d1/b", "d1/d2", "d1/d2/a", "d3", "d3/a",
];
const MODES: [u32; 6] = [0o644, 0o600, 0o755, 0o700, 0o444, 0o555];
const TARGETS: [&str; 4] = ["a", "d1", "missing", "d3/a"];

#[derive(Debug, Clone)]
enum Op {
    Write(usize, Vec<u8>),
    Append(usize, Vec<u8>),
    Truncate(usize, u64),
    Delete(usize),
    Rename(usize, usize),
    Mkdir(usize),
    Rmdir(usize),
    RmTree(usize),
    Chmod(usize, usize),
    Symlink(usize, usize),
    Hardlink(usize, usize),
}

fn op() -> impl Strategy<Value = Op> {
    let p = 0..PATHS.len();
    prop_oneof![
        3 => (p.clone(), vec(any::<u8>(), 0..64)).prop_map(|(p, d)| Op::Write(p, d)),
        2 => (p.clone(), vec(any::<u8>(), 1..16)).prop_map(|(p, d)| Op::Append(p, d)),
        1 => (p.clone(), 0u64..40).prop_map(|(p, n)| Op::Truncate(p, n)),
        2 => p.clone().prop_map(Op::Delete),
        2 => (p.clone(), p.clone()).prop_map(|(a, b)| Op::Rename(a, b)),
        2 => p.clone().prop_map(Op::Mkdir),
        1 => p.clone().prop_map(Op::Rmdir),
        1 => p.clone().prop_map(Op::RmTree),
        1 => (p.clone(), 0..MODES.len()).prop_map(|(p, m)| Op::Chmod(p, m)),
        1 => (p.clone(), 0..TARGETS.len()).prop_map(|(p, t)| Op::Symlink(p, t)),
        1 => (p.clone(), p).prop_map(|(a, b)| Op::Hardlink(a, b)),
    ]
}

type Stage = (Vec<Op>, Vec<Op>);

fn apply(root: &Path, op: &Op) -> io::Result<()> {
    use std::io::Write;
    let path = |i: &usize| root.join(PATHS[*i]);
    let other = |m: &str| io::Error::new(io::ErrorKind::Other, m.to_string());
    match op {
        Op::Write(p, data) => std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path(p))?
            .write_all(data),
        Op::Append(p, data) => std::fs::OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path(p))?
            .write_all(data),
        Op::Truncate(p, n) => std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path(p))?
            .set_len(*n),
        Op::Delete(p) => std::fs::remove_file(path(p)),
        Op::Rename(a, b) => std::fs::rename(path(a), path(b)),
        Op::Mkdir(p) => std::fs::create_dir(path(p)),
        Op::Rmdir(p) => std::fs::remove_dir(path(p)),
        Op::RmTree(p) => {
            if std::fs::symlink_metadata(path(p))?.is_dir() {
                std::fs::remove_dir_all(path(p))
            } else {
                Err(other("not a directory"))
            }
        }
        Op::Chmod(p, m) => {
            if std::fs::symlink_metadata(path(p))?.file_type().is_symlink() {
                return Err(other("symlink"));
            }
            std::fs::set_permissions(path(p), std::fs::Permissions::from_mode(MODES[*m]))
        }
        Op::Symlink(p, t) => std::os::unix::fs::symlink(TARGETS[*t], path(p)),
        Op::Hardlink(a, b) => {
            if !std::fs::symlink_metadata(path(a))?.file_type().is_file() {
                return Err(other("not a regular file"));
            }
            std::fs::hard_link(path(a), path(b))
        }
    }
}

/// Overlayfs without `index=on` breaks a hard link when it copies one of its
/// names up. Layers below a fresh writable layer therefore behave as if every
/// link were a separate file; the model is made to agree at each stage start.
fn break_hardlinks(root: &Path) -> io::Result<()> {
    for (rel, m) in tree::walk(root)? {
        if m.file_type().is_file() && m.nlink() > 1 {
            let p = root.join(&rel);
            let tmp = root.join(format!("{rel}.unlink-tmp"));
            std::fs::copy(&p, &tmp)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m.mode() & 0o7777))?;
            std::fs::rename(&tmp, &p)?;
        }
    }
    Ok(())
}

struct Mounted(PathBuf);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = sandfork_ns::sys::umount_detach(&self.0.to_string_lossy());
    }
}

fn mount_overlay(
    store: &Store,
    layers_bottom_first: &[String],
    scratch: &Path,
    at: &Path,
) -> Result<Mounted, TestCaseError> {
    let fail = |e: String| TestCaseError::fail(e);
    for d in ["upper", "work"] {
        std::fs::create_dir_all(scratch.join(d)).map_err(|e| fail(e.to_string()))?;
    }
    std::fs::create_dir_all(at).map_err(|e| fail(e.to_string()))?;
    let lowers: Vec<String> = layers_bottom_first
        .iter()
        .rev()
        .map(|l| store.layer_root(l).to_string_lossy().into_owned())
        .collect();
    let opts = format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        lowers.join(":"),
        scratch.join("upper").display(),
        scratch.join("work").display()
    );
    sandfork_ns::sys::mount(
        Some("overlay"),
        &at.to_string_lossy(),
        Some("overlay"),
        0,
        Some(&opts),
    )
    .map_err(|e| fail(e.to_string()))?;
    Ok(Mounted(at.to_path_buf()))
}

fn expect_equal(what: &str, got: &Path, model: &Path) -> Result<(), TestCaseError> {
    let d = compare_trees(got, model)
        .map_err(|e| TestCaseError::fail(format!("{what}: compare failed: {e}")))?;
    if !d.is_empty() {
        return Err(TestCaseError::fail(format!(
            "{what}: differs from the model: {d:?}"
        )));
    }
    Ok(())
}

/// Mount `layers` with a throwaway writable layer and compare with the model.
fn check_stack(
    what: &str,
    store: &Store,
    layers: &[String],
    case: &Path,
    model: &Path,
    n: &mut u32,
) -> Result<(), TestCaseError> {
    *n += 1;
    let scratch = case.join(format!("chk{n}"));
    let at = scratch.join("mnt");
    let m = mount_overlay(store, layers, &scratch, &at)?;
    let r = expect_equal(what, &at, model);
    drop(m);
    r
}

fn run_case(work: &Path, stages: &[Stage], case_no: u64) -> Result<(), TestCaseError> {
    let fail = |e: String| TestCaseError::fail(e);
    let case = work.join(format!("c{case_no}"));
    let _ = tree::remove_all(&case);
    let model = case.join("model");
    std::fs::create_dir_all(model.join("d1/d2")).map_err(|e| fail(e.to_string()))?;
    for (p, data) in [("a", "alpha"), ("d1/a", "one"), ("d1/d2/a", "two")] {
        std::fs::write(model.join(p), data).map_err(|e| fail(e.to_string()))?;
    }
    let store = Store::open(StoreCfg {
        root: case.join("store"),
        xattr_ns: XattrNs::User,
        id_base: None,
    })
    .map_err(|e| fail(e.to_string()))?;
    let base = store
        .import_tree(&model, "", &[], Owner::Skip)
        .map_err(|e| fail(e.to_string()))?;
    let mut layers = vec![base.id];
    let mut n = 0u32;

    for (si, (batch_a, batch_b)) in stages.iter().enumerate() {
        break_hardlinks(&model).map_err(|e| fail(e.to_string()))?;
        let scratch = case.join(format!("stage{si}"));
        let mnt = scratch.join("mnt");
        let main = mount_overlay(&store, &layers, &scratch, &mnt)?;
        let upper = scratch.join("upper");
        let mut prev = None;
        let mut deltas: Vec<String> = Vec::new();

        for (bi, batch) in [batch_a, batch_b].into_iter().enumerate() {
            for op in batch {
                // The overlay may refuse what a plain directory allows (for
                // example renaming a directory that lives in a lower layer).
                // What it accepts, the model must accept too.
                if apply(&mnt, op).is_ok() {
                    apply(&model, op).map_err(|e| {
                        fail(format!(
                            "stage {si}: overlay accepted {op:?} but the model refused it: {e}"
                        ))
                    })?;
                }
            }
            expect_equal(
                &format!("stage {si} batch {bi}: live overlay"),
                &mnt,
                &model,
            )?;

            let (delta, cur) = store
                .seal_delta(&upper, prev.as_ref())
                .map_err(|e| fail(e.to_string()))?;
            if let Some(d) = delta {
                deltas.push(d.id);
            }
            prev = Some(cur);
            let stacked: Vec<String> = layers.iter().chain(&deltas).cloned().collect();
            check_stack(
                &format!("stage {si} batch {bi}: incremental seals"),
                &store,
                &stacked,
                &case,
                &model,
                &mut n,
            )?;
        }

        let full = store.seal(&upper).map_err(|e| fail(e.to_string()))?;
        let with_full: Vec<String> = layers.iter().cloned().chain([full.id]).collect();
        check_stack(
            &format!("stage {si}: full seal"),
            &store,
            &with_full,
            &case,
            &model,
            &mut n,
        )?;

        let stacked: Vec<String> = layers.iter().chain(&deltas).cloned().collect();
        let flat = case.join(format!("flat{si}"));
        store
            .flatten_into(&stacked, &flat)
            .map_err(|e| fail(e.to_string()))?;
        expect_equal(&format!("stage {si}: userspace flatten"), &flat, &model)?;

        let mut squashed = None;
        if !deltas.is_empty() {
            let sq = store.squash(&deltas).map_err(|e| fail(e.to_string()))?;
            let with_sq: Vec<String> = layers.iter().cloned().chain([sq.id.clone()]).collect();
            check_stack(
                &format!("stage {si}: squashed deltas"),
                &store,
                &with_sq,
                &case,
                &model,
                &mut n,
            )?;
            squashed = Some(sq.id);
        }
        drop(main);
        // Alternate between continuing on the delta chain and on its squash,
        // so later stages build on both representations.
        match squashed {
            Some(sq) if si % 2 == 1 => layers.push(sq),
            _ => layers.extend(deltas),
        }
    }
    let _ = tree::remove_all(&case);
    Ok(())
}

fn main() {
    if let Err(e) = sandfork_ns::outer::enter_outer_userns() {
        if std::env::var("SANDFORK_REQUIRE_NS").is_ok() {
            eprintln!("overlay_model: cannot enter a user namespace: {e}");
            std::process::exit(1);
        }
        println!("overlay_model: SKIPPED (no user namespaces here: {e})");
        return;
    }
    let cases: u32 = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48);
    let work = std::env::var("SANDFORK_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("sf-overlay-model-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("create work dir");

    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    });
    let case_no = std::cell::Cell::new(0u64);
    let result = runner.run(&vec((vec(op(), 0..8), vec(op(), 0..8)), 1..4), |stages| {
        case_no.set(case_no.get() + 1);
        run_case(&work, &stages, case_no.get())
    });
    let _ = tree::remove_all(&work);
    match result {
        Ok(()) => println!("overlay_model: {cases} cases passed"),
        Err(e) => {
            eprintln!("overlay_model: FAILED\n{e}");
            std::process::exit(1);
        }
    }
}
