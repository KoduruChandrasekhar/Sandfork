//! Layer-store behaviour that needs no mounts: deduplication, the
//! extended-attribute namespace rule, hand-built whiteouts and opaque
//! directories, squashing, lookups, diffs and garbage collection.

use sandfork_layers::tree::{self, compare_trees, Owner};
use sandfork_layers::{ChangeKind, Store, StoreCfg, XattrNs};
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sf-store-{name}-{}", std::process::id()));
    let _ = tree::remove_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn store(dir: &Path) -> Store {
    Store::open(StoreCfg {
        root: dir.join("store"),
        xattr_ns: XattrNs::User,
        id_base: None,
    })
    .unwrap()
}

fn write(root: &Path, rel: &str, data: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// Whiteouts need Linux 5.8 for an unprivileged caller, and opaque markers
/// need user xattrs on the temp filesystem. Tests that depend on them skip
/// where either is missing rather than report a false failure.
fn overlay_markers_work(dir: &Path) -> bool {
    let probe = dir.join("probe");
    std::fs::create_dir_all(&probe).unwrap();
    let ok = tree::make_whiteout(&probe.join("w")).is_ok()
        && tree::set_opaque(&probe, XattrNs::User).is_ok();
    let _ = tree::remove_all(&probe);
    if !ok {
        eprintln!("skipping: no whiteouts or user xattrs in {}", dir.display());
    }
    ok
}

#[test]
fn identical_trees_deduplicate() {
    let d = tmp("dedup");
    let s = store(&d);
    for name in ["one", "two"] {
        write(&d.join(name), "src/a.py", "x = 1\n");
        write(&d.join(name), "src/b.py", "y = 2\n");
    }
    let a = s.import_tree(&d.join("one"), "", &[], Owner::Skip).unwrap();
    // A different mtime must not change the layer ID.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&d.join("two"), "src/a.py", "x = 1\n");
    let b = s.import_tree(&d.join("two"), "", &[], Owner::Skip).unwrap();
    assert_eq!(a.id, b.id);
    assert!(!a.deduped && b.deduped);
    let st = s.stats().unwrap();
    assert_eq!((st.layers, st.dedup_hits), (1, 1));
    write(&d.join("two"), "src/a.py", "x = 2\n");
    assert_ne!(
        s.import_tree(&d.join("two"), "", &[], Owner::Skip)
            .unwrap()
            .id,
        a.id
    );
    assert!(s.verify_layer(&a.id).unwrap());
}

#[test]
fn import_honours_prefix_and_exclude() {
    let d = tmp("prefix");
    let s = store(&d);
    write(&d.join("fx"), "src/a.py", "a");
    write(&d.join("fx"), "tests/test_a.py", "hidden");
    write(&d.join("fx"), "src/__pycache__/a.pyc", "junk");
    let l = s
        .import_tree(
            &d.join("fx"),
            "workspace",
            &["tests".into(), "__pycache__".into()],
            Owner::Skip,
        )
        .unwrap();
    let root = s.layer_root(&l.id);
    assert!(root.join("workspace/src/a.py").exists());
    assert!(
        !root.join("workspace/tests").exists(),
        "hidden tests must never enter a rollout layer"
    );
    assert!(!root.join("workspace/src/__pycache__").exists());
}

#[test]
fn the_other_xattr_namespace_is_refused() {
    let d = tmp("xattr");
    drop(store(&d));
    let err = Store::open(StoreCfg {
        root: d.join("store"),
        xattr_ns: XattrNs::Trusted,
        id_base: None,
    })
    .err()
    .expect("must refuse");
    assert_eq!(err.code, "xattr_ns");
    assert!(
        err.message.contains("opaque directories"),
        "{}",
        err.message
    );
    // The original namespace still opens.
    drop(store(&d));
}

#[test]
fn squash_and_flatten_honour_whiteouts_and_opaque_dirs() {
    let d = tmp("squash");
    if !overlay_markers_work(&d) {
        return;
    }
    let s = store(&d);
    // base: keep, gone, dir/{old1,old2}, sub/x
    let base = d.join("base");
    write(&base, "keep", "k");
    write(&base, "gone", "g");
    write(&base, "dir/old1", "1");
    write(&base, "dir/old2", "2");
    write(&base, "sub/x", "x");
    let l0 = s.import_tree(&base, "", &[], Owner::Skip).unwrap();

    // layer 1: delete `gone`, replace `dir` wholesale, modify sub/x
    let u1 = d.join("u1");
    std::fs::create_dir_all(&u1).unwrap();
    tree::make_whiteout(&u1.join("gone")).unwrap();
    write(&u1, "dir/new", "n");
    tree::set_opaque(&u1.join("dir"), XattrNs::User).unwrap();
    write(&u1, "sub/x", "x2");
    let l1 = s.seal(&u1).unwrap();

    // layer 2: bring `gone` back as a directory, delete dir/new
    let u2 = d.join("u2");
    write(&u2, "gone/inside", "i");
    std::fs::create_dir_all(u2.join("dir")).unwrap();
    tree::make_whiteout(&u2.join("dir/new")).unwrap();
    let l2 = s.seal(&u2).unwrap();

    let expect = d.join("expect");
    write(&expect, "keep", "k");
    write(&expect, "gone/inside", "i");
    std::fs::create_dir_all(expect.join("dir")).unwrap();
    write(&expect, "sub/x", "x2");

    let flat = d.join("flat");
    s.flatten_into(&[l0.id.clone(), l1.id.clone(), l2.id.clone()], &flat)
        .unwrap();
    assert_eq!(compare_trees(&flat, &expect).unwrap(), Vec::<String>::new());

    // Squashing the top run must give the same merged view, and must keep
    // the markers that hide paths in the layer below.
    let sq = s.squash(&[l1.id.clone(), l2.id.clone()]).unwrap();
    let flat2 = d.join("flat2");
    s.flatten_into(&[l0.id.clone(), sq.id.clone()], &flat2)
        .unwrap();
    assert_eq!(
        compare_trees(&flat2, &expect).unwrap(),
        Vec::<String>::new()
    );
    let sq_root = s.layer_root(&sq.id);
    assert!(
        tree::is_opaque(&sq_root.join("dir"), XattrNs::User),
        "dir must stay opaque after squashing"
    );
    assert!(
        tree::is_opaque(&sq_root.join("gone"), XattrNs::User),
        "a directory that replaced a whiteout must become opaque"
    );
}

#[test]
fn lookup_and_changes_see_through_the_stack() {
    let d = tmp("changes");
    if !overlay_markers_work(&d) {
        return;
    }
    let s = store(&d);
    let base = d.join("base");
    write(&base, "src/a.py", "a = 1\n");
    write(&base, "src/b.py", "b = 1\n");
    write(&base, "tests/t.py", "t\n");
    let l0 = s.import_tree(&base, "", &[], Owner::Skip).unwrap();
    let lowers = vec![s.layer_root(&l0.id)];

    let up = d.join("upper");
    write(&up, "src/a.py", "a = 2\n"); // modified
    write(&up, "src/b.py", "b = 1\n"); // copied up, unchanged
    write(&up, "src/c.py", "c = 1\n"); // added
    std::fs::create_dir_all(up.join("tests")).unwrap();
    tree::make_whiteout(&up.join("tests/t.py")).unwrap(); // deleted
    tree::make_whiteout(&up.join("never-existed")).unwrap(); // deletes nothing

    let ch = sandfork_layers::diff::changes(&lowers, &up, XattrNs::User).unwrap();
    let got: Vec<(&str, ChangeKind)> = ch.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    assert_eq!(
        got,
        vec![
            ("src/a.py", ChangeKind::Modified),
            ("src/c.py", ChangeKind::Added),
            ("tests/t.py", ChangeKind::Deleted)
        ]
    );

    assert!(tree::lower_lookup(&lowers, "src/a.py", XattrNs::User).is_some());
    assert!(tree::lower_lookup(&lowers, "src/zzz.py", XattrNs::User).is_none());
    // With the upper on top of the stack, the whiteout hides the lower file.
    let stack = vec![up.clone(), s.layer_root(&l0.id)];
    assert!(tree::lower_lookup(&stack, "tests/t.py", XattrNs::User).is_none());
    assert!(tree::lower_lookup(&stack, "src/b.py", XattrNs::User).is_some());
}

#[test]
fn incremental_seal_writes_only_what_changed() {
    let d = tmp("delta");
    if !overlay_markers_work(&d) {
        return;
    }
    let s = store(&d);
    let up = d.join("upper");
    write(&up, "a", "1");
    write(&up, "dir/b", "2");
    let (l1, m1) = s.seal_delta(&up, None).unwrap();
    let l1 = l1.unwrap();

    // Nothing changed: no layer.
    let (none, m1b) = s.seal_delta(&up, Some(&m1)).unwrap();
    assert!(none.is_none());
    assert_eq!(m1, m1b);

    write(&up, "dir/b", "22");
    std::fs::remove_file(up.join("a")).unwrap();
    write(&up, "c", "3");
    let (l2, _m2) = s.seal_delta(&up, Some(&m1)).unwrap();
    let l2 = l2.unwrap();
    let root = s.layer_root(&l2.id);
    assert!(
        tree::is_whiteout(&std::fs::symlink_metadata(root.join("a")).unwrap()),
        "a path present at the last seal and absent now needs a whiteout"
    );
    assert_eq!(std::fs::read_to_string(root.join("dir/b")).unwrap(), "22");
    assert_eq!(std::fs::read_to_string(root.join("c")).unwrap(), "3");

    let flat = d.join("flat");
    s.flatten_into(&[l1.id, l2.id], &flat).unwrap();
    let expect = d.join("expect");
    write(&expect, "dir/b", "22");
    write(&expect, "c", "3");
    assert_eq!(compare_trees(&flat, &expect).unwrap(), Vec::<String>::new());
}

#[test]
fn gc_keeps_what_is_referenced() {
    let d = tmp("gc");
    let s = store(&d);
    let mut ids = Vec::new();
    for i in 0..4 {
        write(&d.join(format!("t{i}")), "f", &format!("content {i}"));
        ids.push(
            s.import_tree(&d.join(format!("t{i}")), "", &[], Owner::Skip)
                .unwrap()
                .id,
        );
    }
    let pinned = s
        .new_snapshot("task-a", "", "prepared", vec![ids[0].clone()], "key-a")
        .unwrap();
    s.pin(&pinned.id, 1).unwrap();
    let in_use = s
        .new_snapshot("task-b", "", "prepared", vec![ids[1].clone()], "key-b")
        .unwrap();
    s.register_sandbox("sbx-live", &in_use.id, &in_use.layers)
        .unwrap();
    let idle = s
        .new_snapshot("task-c", "", "prepared", vec![ids[2].clone()], "key-c")
        .unwrap();
    // ids[3] is referenced by nothing.

    // Inside the grace period nothing is collected.
    let r = s.gc(3600, None).unwrap();
    assert_eq!((r.layers_removed, r.snapshots_removed), (0, 0));

    let r = s.gc(0, None).unwrap();
    assert_eq!(
        r.snapshots_removed, 1,
        "only the idle, unpinned snapshot goes"
    );
    assert_eq!(r.layers_removed, 2, "its layer and the unreferenced one");
    assert!(s.layer_root(&ids[0]).exists() && s.layer_root(&ids[1]).exists());
    assert!(!s.layer_root(&ids[2]).exists() && !s.layer_root(&ids[3]).exists());
    assert!(s.get_snapshot(&idle.id).is_err());
    assert!(s.find_snapshot_by_key("key-c").unwrap().is_none());
    assert!(s.find_snapshot_by_key("key-a").unwrap().is_some());

    // Releasing the sandbox and unpinning makes both collectable; a zero
    // budget then evicts everything.
    s.unregister_sandbox("sbx-live").unwrap();
    s.pin(&pinned.id, -1).unwrap();
    let r = s.gc(3600, Some(0)).unwrap();
    assert_eq!(r.bytes_total, 0);
    assert_eq!(s.stats().unwrap().layers, 0);
}
