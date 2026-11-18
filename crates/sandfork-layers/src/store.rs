//! The content-addressed layer store.
//!
//! ```text
//! <root>/layers/<id[..20]>/root/        the tree, read-only once sealed
//! <root>/layers/<id[..20]>/manifest.bin
//! <root>/rollouts/<sandbox-id>/         per-sandbox tmpfs: upper/ work/ root/
//! <root>/tmp/<random>/root/             staging for seals and imports
//! <root>/store.sqlite                   layers, snapshots, sandboxes, mounts
//! ```
//!
//! Layer directories are named by the first 20 hex digits of the layer ID so
//! that many lower layers fit in one overlay option string.

use crate::manifest::{scan, Kind, Manifest};
use crate::tree::{self, Copier, Owner, XattrNs};
use rusqlite::{params, Connection, OptionalExtension};
use sandfork_core::{ids, unix_time, Ctx, Result, SfError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct StoreCfg {
    pub root: PathBuf,
    pub xattr_ns: XattrNs,
    /// Daemon-side ID of sandbox UID 0; `None` when ownership is not tracked (L0).
    pub id_base: Option<u32>,
}

/// An ordered list of layers, bottom to top, plus metadata. Snapshots form a
/// tree with shared prefixes: a base, a task's prepared state on top of it, a
/// checkpoint at turn 3 of one rollout on top of that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub task: String,
    pub parent: String,
    /// `base`, `prepared`, `turn` or `final`.
    pub kind: String,
    pub layers: Vec<String>,
    /// Idempotency key (a spec hash); empty for none.
    pub key: String,
    pub created: u64,
    pub last_used: u64,
    pub pins: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerInfo {
    pub id: String,
    pub bytes: u64,
    pub files: u64,
    /// The identical layer was already in the store.
    pub deduped: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GcReport {
    pub snapshots_removed: u64,
    pub layers_removed: u64,
    pub orphans_removed: u64,
    pub bytes_freed: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoreStats {
    pub layers: u64,
    pub bytes: u64,
    pub snapshots: u64,
    pub dedup_hits: u64,
    pub sandboxes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub id: String,
    pub sandbox: String,
    pub snapshot: String,
    /// CRIU image directory; empty for a filesystem-only checkpoint.
    pub images: String,
    pub turn: u32,
    pub created: u64,
}

pub struct Store {
    pub cfg: StoreCfg,
    db: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS layers(id TEXT PRIMARY KEY, bytes INTEGER NOT NULL, files INTEGER NOT NULL, created INTEGER NOT NULL, hits INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS snapshots(id TEXT PRIMARY KEY, task TEXT NOT NULL, parent TEXT NOT NULL, kind TEXT NOT NULL, layers TEXT NOT NULL, key TEXT NOT NULL, created INTEGER NOT NULL, last_used INTEGER NOT NULL, pins INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS snapshots_key ON snapshots(key);
CREATE TABLE IF NOT EXISTS sandboxes(id TEXT PRIMARY KEY, snapshot TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS mounts(sandbox TEXT NOT NULL, layer TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS mounts_sandbox ON mounts(sandbox);
CREATE TABLE IF NOT EXISTS checkpoints(id TEXT PRIMARY KEY, sandbox TEXT NOT NULL, snapshot TEXT NOT NULL, images TEXT NOT NULL, turn INTEGER NOT NULL, created INTEGER NOT NULL);
";

fn short(id: &str) -> &str {
    &id[..id.len().min(20)]
}

impl Store {
    /// Open or create a store. Refuses a store created with the other
    /// extended-attribute namespace.
    pub fn open(cfg: StoreCfg) -> Result<Store> {
        for d in ["layers", "rollouts", "tmp", "images", "flat"] {
            std::fs::create_dir_all(cfg.root.join(d))
                .ctx_with(|| format!("create {}/{d}", cfg.root.display()))?;
        }
        let db = Connection::open(cfg.root.join("store.sqlite")).ctx("open store.sqlite")?;
        db.busy_timeout(std::time::Duration::from_secs(10))
            .ctx("busy_timeout")?;
        let _: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .ctx("journal_mode")?;
        db.execute_batch(SCHEMA).ctx("create schema")?;
        let existing: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key='xattr_ns'", [], |r| {
                r.get(0)
            })
            .optional()
            .ctx("read meta")?;
        match existing {
            Some(ns) if ns != cfg.xattr_ns.as_str() => {
                return Err(SfError::client(
                    "xattr_ns",
                    format!(
                        "store at {} was created with {ns}.overlay.* markers; refusing to use it with {}.overlay.*: opaque directories would be silently ignored and deleted directories would reappear",
                        cfg.root.display(),
                        cfg.xattr_ns.as_str()
                    ),
                ));
            }
            Some(_) => {}
            None => {
                db.execute(
                    "INSERT INTO meta(key, value) VALUES('xattr_ns', ?1)",
                    params![cfg.xattr_ns.as_str()],
                )
                .ctx("write meta")?;
            }
        }
        Ok(Store {
            cfg,
            db: Mutex::new(db),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    pub fn layer_dir(&self, id: &str) -> PathBuf {
        self.cfg.root.join("layers").join(short(id))
    }

    pub fn layer_root(&self, id: &str) -> PathBuf {
        self.layer_dir(id).join("root")
    }

    /// Path of a layer's tree relative to the store root.
    pub fn layer_rel(&self, id: &str) -> String {
        format!("layers/{}/root", short(id))
    }

    pub fn rollout_dir(&self, sandbox: &str) -> PathBuf {
        self.cfg.root.join("rollouts").join(sandbox)
    }

    fn owner(&self) -> Owner<'static> {
        if self.cfg.id_base.is_some() {
            Owner::Preserve
        } else {
            Owner::Skip
        }
    }

    /// A fresh staging directory containing an empty `root/`.
    pub fn new_staging(&self) -> Result<PathBuf> {
        let dir = self.cfg.root.join("tmp").join(ids::new_id("stage"));
        std::fs::create_dir_all(dir.join("root")).ctx("create staging")?;
        Ok(dir)
    }

    /// Hash a staged tree and move it into the store. If the identical layer
    /// already exists the staging copy is dropped: that is the deduplication.
    pub fn finalize(&self, staging: &Path) -> Result<LayerInfo> {
        use std::os::unix::fs::PermissionsExt;
        let manifest = scan(&staging.join("root"), self.cfg.xattr_ns, self.cfg.id_base)
            .ctx("scan staged layer")?;
        let id = manifest.id();
        let (bytes, files) = (manifest.bytes(), manifest.files());
        let dst = self.layer_dir(&id);
        let mut deduped = dst.join("manifest.bin").exists();
        if !deduped {
            std::fs::write(
                staging.join("manifest.bin"),
                manifest.encode().ctx("encode manifest")?,
            )
            .ctx("write manifest")?;
            for p in [staging.to_path_buf(), staging.join("root")] {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            }
            let _ = tree::remove_all(&dst); // a half-written directory from a crash
            match std::fs::rename(staging, &dst) {
                Ok(()) => {}
                Err(_) if dst.join("manifest.bin").exists() => deduped = true, // lost a race to an identical layer
                Err(e) => {
                    return Err(SfError::infra(
                        "layers",
                        format!("move layer into store: {e}"),
                    ))
                }
            }
        }
        if deduped {
            let _ = tree::remove_all(staging);
        }
        let db = self.db();
        db.execute(
            "INSERT OR IGNORE INTO layers(id, bytes, files, created) VALUES(?1, ?2, ?3, ?4)",
            params![id, bytes as i64, files as i64, unix_time() as i64],
        )
        .ctx("insert layer")?;
        if deduped {
            db.execute(
                "UPDATE layers SET hits = hits + 1 WHERE id = ?1",
                params![id],
            )
            .ctx("count dedup hit")?;
        }
        Ok(LayerInfo {
            id,
            bytes,
            files,
            deduped,
        })
    }

    /// Import a plain directory as a layer. With `prefix`, the tree lands
    /// under that path inside the layer (for example `workspace`). Names in
    /// `exclude` are left out at any depth.
    pub fn import_tree(
        &self,
        src: &Path,
        prefix: &str,
        exclude: &[String],
        owner: Owner,
    ) -> Result<LayerInfo> {
        let staging = self.new_staging()?;
        let r = (|| {
            let mut cp = Copier::new(owner);
            let mut dst = staging.join("root");
            let mut rel = String::new();
            for part in prefix.split('/').filter(|p| !p.is_empty()) {
                dst.push(part);
                if !rel.is_empty() {
                    rel.push('/');
                }
                rel.push_str(part);
                std::fs::create_dir_all(&dst)?;
                let m = std::fs::symlink_metadata(src)?;
                cp.apply_attrs(src, &dst, &rel, &m)?;
            }
            // Owner decisions see paths relative to the layer root.
            let pre = rel.clone();
            if let Owner::Map(f) = &cp.owner {
                let mapped = |r: &str| {
                    f(&if pre.is_empty() {
                        r.to_string()
                    } else {
                        format!("{pre}/{r}")
                    })
                };
                let mut inner = Copier::new(Owner::Map(&mapped));
                inner.copy_tree(src, &dst, exclude)
            } else {
                cp.copy_tree(src, &dst, exclude)
            }
        })();
        if let Err(e) = r {
            let _ = tree::remove_all(&staging);
            return Err(SfError::infra(
                "layers",
                format!("import {}: {e}", src.display()),
            ));
        }
        self.finalize(&staging)
    }

    /// Seal a writable layer: copy it into the store as a new layer. The
    /// running sandbox keeps its own writable layer; the snapshot is a copy.
    pub fn seal(&self, upper: &Path) -> Result<LayerInfo> {
        sandfork_core::fail::point("seal_before_copy")?;
        let staging = self.new_staging()?;
        let mut cp = Copier::new(self.owner());
        if let Err(e) = cp.copy_tree(upper, &staging.join("root"), &[]) {
            let _ = tree::remove_all(&staging);
            return Err(SfError::infra(
                "layers",
                format!("seal {}: {e}", upper.display()),
            ));
        }
        if let Err(e) = sandfork_core::fail::point("seal_after_copy") {
            let _ = tree::remove_all(&staging);
            return Err(e);
        }
        self.finalize(&staging)
    }

    /// Incremental seal. `prev` is the manifest of the whole writable layer at
    /// the previous seal. The new layer holds only entries whose content or
    /// metadata changed since then, plus whiteouts for paths that were present
    /// then and are absent now. Stacked on the earlier layers, that delta is a
    /// correct overlay. Returns the new layer (if anything changed) and the
    /// current manifest, to pass as `prev` next time.
    pub fn seal_delta(
        &self,
        upper: &Path,
        prev: Option<&Manifest>,
    ) -> Result<(Option<LayerInfo>, Manifest)> {
        let ns = self.cfg.xattr_ns;
        let cur = scan(upper, ns, self.cfg.id_base).ctx("scan writable layer")?;
        let Some(prev) = prev else {
            if cur.entries.is_empty() {
                return Ok((None, cur));
            }
            return Ok((Some(self.seal(upper)?), cur));
        };

        // Directories that became opaque since the last seal: they hide the
        // earlier delta layers, so their whole subtree must be written again.
        let newly_opaque: Vec<&str> = cur
            .entries
            .iter()
            .filter(|e| {
                e.kind == Kind::Dir
                    && e.opaque
                    && !prev
                        .get(&e.path)
                        .is_some_and(|p| p.kind == Kind::Dir && p.opaque)
            })
            .map(|e| e.path.as_str())
            .collect();
        let under = |dirs: &[&str], path: &str| {
            dirs.iter().any(|d| {
                path.len() > d.len() && path.starts_with(d) && path.as_bytes()[d.len()] == b'/'
            })
        };

        // path -> true if the entry itself is written with its own attributes
        // (false: only a container for something below it).
        let mut include: BTreeMap<String, bool> = BTreeMap::new();
        for e in &cur.entries {
            let changed = !prev.get(&e.path).is_some_and(|p| p.same_as(e));
            if changed || under(&newly_opaque, &e.path) {
                include.insert(e.path.clone(), true);
            }
        }
        let mut whiteouts: Vec<String> = Vec::new();
        'deleted: for p in &prev.entries {
            if cur.get(&p.path).is_some() {
                continue;
            }
            // Covered if an ancestor is itself gone (its whiteout covers
            // this), is no longer a directory, or is newly opaque.
            let mut acc = String::new();
            let parts: Vec<&str> = p.path.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                match cur.get(&acc) {
                    Some(a) if a.kind == Kind::Dir && !newly_opaque.contains(&acc.as_str()) => {}
                    _ => continue 'deleted,
                }
            }
            whiteouts.push(p.path.clone());
        }
        if include.is_empty() && whiteouts.is_empty() {
            return Ok((None, cur));
        }
        for path in include
            .keys()
            .cloned()
            .chain(whiteouts.iter().cloned())
            .collect::<Vec<_>>()
        {
            let mut acc = String::new();
            let parts: Vec<&str> = path.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                include.entry(acc.clone()).or_insert(false);
            }
        }

        let staging = self.new_staging()?;
        let root = staging.join("root");
        let r = (|| -> std::io::Result<()> {
            let mut cp = Copier::new(self.owner());
            // The opaque marker is decided here, not copied: a directory that
            // was already opaque at the last seal must not hide that seal.
            cp.skip_xattr = Some(ns.opaque());
            for path in include.keys() {
                let src = upper.join(path);
                let dst = root.join(path);
                let m = std::fs::symlink_metadata(&src)?;
                cp.copy_entry(&src, &dst, path, &m)?;
                if newly_opaque.contains(&path.as_str()) {
                    tree::set_opaque(&dst, ns)?;
                }
            }
            for path in &whiteouts {
                tree::make_whiteout(&root.join(path))?;
            }
            Ok(())
        })();
        if let Err(e) = r {
            let _ = tree::remove_all(&staging);
            return Err(SfError::infra(
                "layers",
                format!("incremental seal of {}: {e}", upper.display()),
            ));
        }
        Ok((Some(self.finalize(&staging)?), cur))
    }

    pub fn manifest(&self, id: &str) -> Result<Manifest> {
        let bytes = std::fs::read(self.layer_dir(id).join("manifest.bin"))
            .ctx_with(|| format!("manifest of layer {}", short(id)))?;
        Manifest::decode(&bytes).ctx("decode manifest")
    }

    /// True if the layer's tree still matches its manifest. A checkpoint
    /// references its sealed layer by hash; restore asserts this first.
    pub fn verify_layer(&self, id: &str) -> Result<bool> {
        let now =
            scan(&self.layer_root(id), self.cfg.xattr_ns, self.cfg.id_base).ctx("rescan layer")?;
        Ok(now.id() == id)
    }

    /// Squash a run of layers (bottom to top) into one equivalent layer.
    /// Whiteouts and opaque markers survive, because layers below the run may
    /// still hold the paths they hide.
    pub fn squash(&self, layers_bottom_first: &[String]) -> Result<LayerInfo> {
        let staging = self.new_staging()?;
        for id in layers_bottom_first {
            if let Err(e) = tree::merge_layer(
                &staging.join("root"),
                &self.layer_root(id),
                self.cfg.xattr_ns,
                self.owner(),
            ) {
                let _ = tree::remove_all(&staging);
                return Err(SfError::infra(
                    "layers",
                    format!("squash layer {}: {e}", short(id)),
                ));
            }
        }
        self.finalize(&staging)
    }

    /// Materialise a whole layer stack (bottom to top) as a plain tree.
    pub fn flatten_into(&self, layers_bottom_first: &[String], dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst).ctx("create flatten target")?;
        for id in layers_bottom_first {
            tree::merge_layer(dst, &self.layer_root(id), self.cfg.xattr_ns, self.owner())
                .ctx_with(|| format!("flatten layer {}", short(id)))?;
        }
        tree::strip_overlay_markers(dst, self.cfg.xattr_ns).ctx("strip overlay markers")
    }

    // ------------------------------------------------------------ snapshots

    pub fn put_snapshot(&self, s: &Snapshot) -> Result<()> {
        self.db()
            .execute(
                "INSERT OR REPLACE INTO snapshots(id, task, parent, kind, layers, key, created, last_used, pins) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![s.id, s.task, s.parent, s.kind, serde_json::to_string(&s.layers).unwrap_or_default(), s.key, s.created as i64, s.last_used as i64, s.pins],
            )
            .ctx("insert snapshot")?;
        Ok(())
    }

    /// Create and store a snapshot from a list of layers.
    pub fn new_snapshot(
        &self,
        task: &str,
        parent: &str,
        kind: &str,
        layers: Vec<String>,
        key: &str,
    ) -> Result<Snapshot> {
        let now = unix_time();
        let s = Snapshot {
            id: ids::snapshot_id(),
            task: task.into(),
            parent: parent.into(),
            kind: kind.into(),
            layers,
            key: key.into(),
            created: now,
            last_used: now,
            pins: 0,
        };
        self.put_snapshot(&s)?;
        Ok(s)
    }

    fn row_to_snapshot(r: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
        let layers: String = r.get(4)?;
        Ok(Snapshot {
            id: r.get(0)?,
            task: r.get(1)?,
            parent: r.get(2)?,
            kind: r.get(3)?,
            layers: serde_json::from_str(&layers).unwrap_or_default(),
            key: r.get(5)?,
            created: r.get::<_, i64>(6)? as u64,
            last_used: r.get::<_, i64>(7)? as u64,
            pins: r.get(8)?,
        })
    }

    const SNAP_COLS: &'static str = "id, task, parent, kind, layers, key, created, last_used, pins";

    pub fn get_snapshot(&self, id: &str) -> Result<Snapshot> {
        self.db()
            .query_row(
                &format!("SELECT {} FROM snapshots WHERE id = ?1", Self::SNAP_COLS),
                params![id],
                Self::row_to_snapshot,
            )
            .optional()
            .ctx("read snapshot")?
            .ok_or_else(|| SfError::client("not_found", format!("snapshot {id} does not exist")))
    }

    pub fn find_snapshot_by_key(&self, key: &str) -> Result<Option<Snapshot>> {
        if key.is_empty() {
            return Ok(None);
        }
        let s = self
            .db()
            .query_row(
                &format!(
                    "SELECT {} FROM snapshots WHERE key = ?1 ORDER BY created DESC LIMIT 1",
                    Self::SNAP_COLS
                ),
                params![key],
                Self::row_to_snapshot,
            )
            .optional()
            .ctx("read snapshot by key")?;
        // A snapshot whose layers were garbage-collected is not a hit.
        Ok(s.filter(|s| {
            s.layers
                .iter()
                .all(|l| self.layer_dir(l).join("manifest.bin").exists())
        }))
    }

    pub fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
        let db = self.db();
        let mut st = db
            .prepare(&format!(
                "SELECT {} FROM snapshots ORDER BY created",
                Self::SNAP_COLS
            ))
            .ctx("list snapshots")?;
        let rows = st
            .query_map([], Self::row_to_snapshot)
            .ctx("list snapshots")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("list snapshots")
    }

    pub fn pin(&self, id: &str, delta: i32) -> Result<()> {
        self.db()
            .execute(
                "UPDATE snapshots SET pins = MAX(0, pins + ?2) WHERE id = ?1",
                params![id, delta],
            )
            .ctx("pin snapshot")?;
        Ok(())
    }

    /// Pins are held by a running daemon; none survive a restart.
    pub fn clear_pins(&self) -> Result<()> {
        self.db()
            .execute("UPDATE snapshots SET pins = 0", [])
            .ctx("clear pins")?;
        Ok(())
    }

    pub fn touch_snapshot(&self, id: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE snapshots SET last_used = ?2 WHERE id = ?1",
                params![id, unix_time() as i64],
            )
            .ctx("touch snapshot")?;
        Ok(())
    }

    // ------------------------------------------------------------ sandboxes and mounts

    /// Record a live sandbox and the layers it has mounted. Mounts are tracked
    /// here rather than by parsing mountinfo.
    pub fn register_sandbox(&self, id: &str, snapshot: &str, layers: &[String]) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT OR REPLACE INTO sandboxes(id, snapshot, created) VALUES(?1, ?2, ?3)",
            params![id, snapshot, unix_time() as i64],
        )
        .ctx("register sandbox")?;
        for l in layers {
            db.execute(
                "INSERT INTO mounts(sandbox, layer) VALUES(?1, ?2)",
                params![id, l],
            )
            .ctx("register mount")?;
        }
        Ok(())
    }

    pub fn unregister_sandbox(&self, id: &str) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM mounts WHERE sandbox = ?1", params![id])
            .ctx("unregister mounts")?;
        db.execute("DELETE FROM sandboxes WHERE id = ?1", params![id])
            .ctx("unregister sandbox")?;
        Ok(())
    }

    pub fn registered_sandboxes(&self) -> Result<Vec<String>> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT id FROM sandboxes")
            .ctx("list sandboxes")?;
        let rows = st
            .query_map([], |r| r.get::<_, String>(0))
            .ctx("list sandboxes")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("list sandboxes")
    }

    // ------------------------------------------------------------ checkpoints

    pub fn put_checkpoint(&self, c: &CheckpointRow) -> Result<()> {
        self.db()
            .execute("INSERT OR REPLACE INTO checkpoints(id, sandbox, snapshot, images, turn, created) VALUES(?1,?2,?3,?4,?5,?6)", params![c.id, c.sandbox, c.snapshot, c.images, c.turn, c.created as i64])
            .ctx("insert checkpoint")?;
        Ok(())
    }

    pub fn get_checkpoint(&self, id: &str) -> Result<CheckpointRow> {
        self.db()
            .query_row("SELECT id, sandbox, snapshot, images, turn, created FROM checkpoints WHERE id = ?1", params![id], |r| {
                Ok(CheckpointRow { id: r.get(0)?, sandbox: r.get(1)?, snapshot: r.get(2)?, images: r.get(3)?, turn: r.get(4)?, created: r.get::<_, i64>(5)? as u64 })
            })
            .optional()
            .ctx("read checkpoint")?
            .ok_or_else(|| SfError::client("not_found", format!("checkpoint {id} does not exist")))
    }

    /// Drop the checkpoints of a sandbox whose rollout has finished.
    pub fn delete_checkpoints_for(&self, sandbox: &str) -> Result<()> {
        self.db()
            .execute(
                "DELETE FROM checkpoints WHERE sandbox = ?1",
                params![sandbox],
            )
            .ctx("delete checkpoints")?;
        Ok(())
    }

    /// Drop checkpoints older than `max_age_s`. Checkpoints deliberately
    /// outlive a daemon crash so rollouts can resume; this bounds how long.
    pub fn expire_checkpoints(&self, max_age_s: u64) -> Result<u64> {
        let cutoff = unix_time().saturating_sub(max_age_s) as i64;
        let n = self
            .db()
            .execute(
                "DELETE FROM checkpoints WHERE created < ?1",
                params![cutoff],
            )
            .ctx("expire checkpoints")?;
        Ok(n as u64)
    }

    // ------------------------------------------------------------ garbage collection

    pub fn stats(&self) -> Result<StoreStats> {
        let db = self.db();
        let (layers, bytes, hits): (i64, i64, i64) = db
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(bytes),0), COALESCE(SUM(hits),0) FROM layers",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ctx("layer stats")?;
        let snapshots: i64 = db
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .ctx("snapshot stats")?;
        let sandboxes: i64 = db
            .query_row("SELECT COUNT(*) FROM sandboxes", [], |r| r.get(0))
            .ctx("sandbox stats")?;
        Ok(StoreStats {
            layers: layers as u64,
            bytes: bytes as u64,
            snapshots: snapshots as u64,
            dedup_hits: hits as u64,
            sandboxes: sandboxes as u64,
        })
    }

    fn sweep(&self, grace_s: u64, report: &mut GcReport) -> Result<()> {
        let now = unix_time();
        let cutoff = now.saturating_sub(grace_s) as i64;
        let db = self.db();
        // Mark: snapshots that are pinned, in use by a live sandbox, named by
        // a checkpoint, or younger than the grace period.
        report.snapshots_removed += db
            .execute(
                "DELETE FROM snapshots WHERE pins = 0 AND created <= ?1
                 AND id NOT IN (SELECT snapshot FROM sandboxes)
                 AND id NOT IN (SELECT snapshot FROM checkpoints)",
                params![cutoff],
            )
            .ctx("sweep snapshots")? as u64;
        let mut live: HashSet<String> = HashSet::new();
        {
            let mut st = db
                .prepare("SELECT layers FROM snapshots")
                .ctx("mark layers")?;
            for row in st
                .query_map([], |r| r.get::<_, String>(0))
                .ctx("mark layers")?
            {
                let layers: Vec<String> =
                    serde_json::from_str(&row.ctx("mark layers")?).unwrap_or_default();
                live.extend(layers);
            }
            let mut st = db.prepare("SELECT layer FROM mounts").ctx("mark mounts")?;
            for row in st
                .query_map([], |r| r.get::<_, String>(0))
                .ctx("mark mounts")?
            {
                live.insert(row.ctx("mark mounts")?);
            }
        }
        // Sweep: unreferenced layers past the grace period.
        let mut dead: Vec<(String, i64)> = Vec::new();
        {
            let mut st = db
                .prepare("SELECT id, bytes FROM layers WHERE created <= ?1")
                .ctx("sweep layers")?;
            for row in st
                .query_map(params![cutoff], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .ctx("sweep layers")?
            {
                let (id, bytes) = row.ctx("sweep layers")?;
                if !live.contains(&id) {
                    dead.push((id, bytes));
                }
            }
        }
        for (id, bytes) in dead {
            tree::remove_all(&self.layer_dir(&id)).ctx("remove layer")?;
            db.execute("DELETE FROM layers WHERE id = ?1", params![id])
                .ctx("delete layer row")?;
            report.layers_removed += 1;
            report.bytes_freed += bytes as u64;
        }
        Ok(())
    }

    /// Mark and sweep. Unreferenced layers are deleted only after a grace
    /// period and only when no live mount lists them. Under a disk budget,
    /// unpinned prepared snapshots are evicted least-recently-used first.
    pub fn gc(&self, grace_s: u64, budget_bytes: Option<u64>) -> Result<GcReport> {
        let mut report = GcReport::default();
        self.sweep(grace_s, &mut report)?;

        if let Some(budget) = budget_bytes {
            loop {
                if self.stats()?.bytes <= budget {
                    break;
                }
                let victim: Option<String> = self
                    .db()
                    .query_row(
                        "SELECT id FROM snapshots WHERE pins = 0
                         AND id NOT IN (SELECT snapshot FROM sandboxes)
                         AND id NOT IN (SELECT snapshot FROM checkpoints)
                         ORDER BY last_used ASC LIMIT 1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .ctx("pick eviction victim")?;
                let Some(victim) = victim else { break };
                self.db()
                    .execute("DELETE FROM snapshots WHERE id = ?1", params![victim])
                    .ctx("evict snapshot")?;
                report.snapshots_removed += 1;
                self.sweep(0, &mut report)?;
            }
        }

        // Directories no row accounts for: a crash between rename and insert,
        // or abandoned staging.
        let known: BTreeSet<String> = {
            let db = self.db();
            let mut st = db.prepare("SELECT id FROM layers").ctx("list layers")?;
            let rows = st
                .query_map([], |r| r.get::<_, String>(0))
                .ctx("list layers")?;
            rows.filter_map(|r| r.ok())
                .map(|id| short(&id).to_string())
                .collect()
        };
        let old = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= grace_s)
        };
        for (dir, is_orphan) in [("layers", true), ("tmp", false)] {
            let Ok(rd) = std::fs::read_dir(self.cfg.root.join(dir)) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if (!is_orphan || !known.contains(&name)) && old(&e.path()) {
                    tree::remove_all(&e.path()).ctx("remove orphan")?;
                    report.orphans_removed += 1;
                }
            }
        }
        // Flattened copies (plain-copy backend) of snapshots that no longer exist.
        if let Ok(rd) = std::fs::read_dir(self.cfg.root.join("flat")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if self.get_snapshot(name.trim_end_matches(".tmp")).is_err() {
                    tree::remove_all(&e.path()).ctx("remove flattened snapshot")?;
                    report.orphans_removed += 1;
                }
            }
        }
        report.bytes_total = self.stats()?.bytes;
        Ok(report)
    }
}
