//! Layer manifests and content addressing.
//!
//! A manifest lists every path in a layer with its type, mode, owner, size,
//! content hash, symlink target and extended attributes. The layer ID is the
//! BLAKE3 hash of the canonical serialisation. Modification times are stored
//! but kept out of the hash, so identical preparation results deduplicate.

use crate::tree::{self, XattrNs};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Whiteout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    /// Directories only: replaces, rather than merges with, the layers below.
    pub opaque: bool,
    pub mode: u32,
    /// Owner as a sandbox-side ID, so a store means the same thing whichever
    /// user namespace the daemon runs in.
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub hash: [u8; 32],
    pub target: String,
    pub xattrs: Vec<(String, Vec<u8>)>,
    /// For the second and later names of a hard-linked file: the first name.
    pub hardlink: String,
    /// Stored, never hashed.
    pub mtime_ns: i64,
}

impl Entry {
    /// Equality on everything the layer ID covers.
    pub fn same_as(&self, o: &Entry) -> bool {
        self.kind == o.kind
            && self.opaque == o.opaque
            && self.mode == o.mode
            && self.uid == o.uid
            && self.gid == o.gid
            && self.size == o.size
            && self.hash == o.hash
            && self.target == o.target
            && self.xattrs == o.xattrs
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Sorted by path.
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.path.as_str().cmp(path))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.hardlink.is_empty())
            .map(|e| e.size)
            .sum()
    }

    pub fn files(&self) -> u64 {
        self.entries.iter().filter(|e| e.kind == Kind::File).count() as u64
    }

    /// The layer ID: BLAKE3 over a canonical, mtime-free serialisation.
    pub fn id(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"sandfork-layer-v1\0");
        for e in &self.entries {
            h.update(e.path.as_bytes());
            h.update(&[0, e.kind as u8, e.opaque as u8]);
            h.update(&e.mode.to_le_bytes());
            h.update(&e.uid.to_le_bytes());
            h.update(&e.gid.to_le_bytes());
            h.update(&e.size.to_le_bytes());
            h.update(&e.hash);
            h.update(e.target.as_bytes());
            h.update(&[0]);
            h.update(e.hardlink.as_bytes());
            h.update(&[0]);
            h.update(&(e.xattrs.len() as u32).to_le_bytes());
            for (k, v) in &e.xattrs {
                h.update(k.as_bytes());
                h.update(&[0]);
                h.update(&(v.len() as u32).to_le_bytes());
                h.update(v);
            }
        }
        h.finalize().to_hex().to_string()
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        postcard::to_allocvec(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Manifest> {
        postcard::from_bytes(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// Scan an overlay-format tree into a manifest. `id_base` is the daemon-side
/// ID of sandbox UID 0; `None` records no ownership (L0).
pub fn scan(root: &Path, ns: XattrNs, id_base: Option<u32>) -> io::Result<Manifest> {
    let mut entries = Vec::new();
    let mut links: HashMap<(u64, u64), String> = HashMap::new();
    let prefix = ns.overlay_prefix();
    for (rel, m) in tree::walk(root)? {
        let p = root.join(&rel);
        let ft = m.file_type();
        let kind = if tree::is_whiteout(&m) {
            Kind::Whiteout
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_file() {
            Kind::File
        } else {
            continue; // sockets, FIFOs and devices never enter a layer
        };
        let mut hardlink = String::new();
        if kind == Kind::File && m.nlink() > 1 {
            match links.get(&(m.dev(), m.ino())) {
                Some(first) => hardlink = first.clone(),
                None => {
                    links.insert((m.dev(), m.ino()), rel.clone());
                }
            }
        }
        let (uid, gid) = match id_base {
            Some(b) => (m.uid().saturating_sub(b), m.gid().saturating_sub(b)),
            None => (0, 0),
        };
        let xattrs: Vec<(String, Vec<u8>)> = if kind == Kind::Symlink || kind == Kind::Whiteout {
            vec![]
        } else {
            tree::list_xattrs(&p)
                .into_iter()
                .filter(|(k, _)| {
                    !k.starts_with(prefix)
                        && !k.starts_with("security.")
                        && !k.starts_with("system.")
                })
                .collect()
        };
        entries.push(Entry {
            kind,
            opaque: kind == Kind::Dir && tree::is_opaque(&p, ns),
            mode: if kind == Kind::Symlink || kind == Kind::Whiteout {
                0
            } else {
                m.mode() & 0o7777
            },
            uid,
            gid,
            size: if kind == Kind::File { m.len() } else { 0 },
            hash: if kind == Kind::File {
                tree::hash_file(&p)?
            } else {
                [0; 32]
            },
            target: if kind == Kind::Symlink {
                std::fs::read_link(&p)?.to_string_lossy().into_owned()
            } else {
                String::new()
            },
            xattrs,
            hardlink,
            mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            path: rel,
        });
    }
    Ok(Manifest { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, kind: Kind) -> Entry {
        Entry {
            path: path.into(),
            kind,
            opaque: false,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: 0,
            hash: [0; 32],
            target: String::new(),
            xattrs: vec![],
            hardlink: String::new(),
            mtime_ns: 0,
        }
    }

    #[test]
    fn id_ignores_mtime_but_not_content() {
        let mut a = Manifest {
            entries: vec![e("a", Kind::File)],
        };
        let id = a.id();
        a.entries[0].mtime_ns = 12345;
        assert_eq!(a.id(), id, "mtime is not part of the layer ID");
        a.entries[0].hash[0] = 1;
        assert_ne!(a.id(), id);
        a.entries[0].hash[0] = 0;
        a.entries[0].mode = 0o755;
        assert_ne!(a.id(), id);
    }

    #[test]
    fn opaque_and_whiteout_change_the_id() {
        let plain = Manifest {
            entries: vec![e("d", Kind::Dir)],
        };
        let mut opaque = plain.clone();
        opaque.entries[0].opaque = true;
        assert_ne!(plain.id(), opaque.id());
        let wh = Manifest {
            entries: vec![e("d", Kind::Whiteout)],
        };
        assert_ne!(plain.id(), wh.id());
    }

    #[test]
    fn roundtrip_and_lookup() {
        let m = Manifest {
            entries: vec![
                e("a", Kind::Dir),
                e("a/b", Kind::File),
                e("c", Kind::Symlink),
            ],
        };
        let back = Manifest::decode(&m.encode().unwrap()).unwrap();
        assert_eq!(m, back);
        assert!(back.get("a/b").is_some());
        assert!(back.get("a/x").is_none());
    }
}
