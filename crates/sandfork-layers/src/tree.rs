//! Directory trees in overlayfs upper format: walking, copying, merging.
//!
//! A layer is a directory tree as overlayfs would write it into an upper
//! directory: a deleted path is a *whiteout* (a character device with device
//! number 0/0) and a directory that replaces, rather than merges with, the
//! one below carries an *opaque* extended attribute.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::Metadata;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Which extended-attribute namespace overlayfs uses for its markers.
/// Rootless overlayfs (`userxattr`) uses `user.overlay.*`; overlayfs mounted
/// by real root uses `trusted.overlay.*`. A layer written one way and mounted
/// the other has its opaque directories silently ignored, so a store records
/// one namespace and refuses the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrNs {
    User,
    Trusted,
}

impl XattrNs {
    pub fn as_str(&self) -> &'static str {
        match self {
            XattrNs::User => "user",
            XattrNs::Trusted => "trusted",
        }
    }
    pub fn parse(s: &str) -> Option<XattrNs> {
        match s {
            "user" => Some(XattrNs::User),
            "trusted" => Some(XattrNs::Trusted),
            _ => None,
        }
    }
    pub fn overlay_prefix(&self) -> &'static str {
        match self {
            XattrNs::User => "user.overlay.",
            XattrNs::Trusted => "trusted.overlay.",
        }
    }
    pub fn opaque(&self) -> &'static str {
        match self {
            XattrNs::User => "user.overlay.opaque",
            XattrNs::Trusted => "trusted.overlay.opaque",
        }
    }
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

pub fn is_whiteout(m: &Metadata) -> bool {
    m.file_type().is_char_device() && m.rdev() == 0
}

/// Create a whiteout: a character device with device number 0/0. Linux 5.8
/// and later let an unprivileged process create exactly that device.
pub fn make_whiteout(path: &Path) -> io::Result<()> {
    let c = cpath(path)?;
    // SAFETY: `c` is a live NUL-terminated string.
    if unsafe { libc::mknod(c.as_ptr(), libc::S_IFCHR, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn get_xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    let c = cpath(path).ok()?;
    let n = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: `buf` is writable for `buf.len()` bytes; both strings are live.
        let r =
            unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if r >= 0 {
            buf.truncate(r as usize);
            return Some(buf);
        }
        if io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) && buf.len() < (1 << 20)
        {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return None;
    }
}

pub fn set_xattr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let c = cpath(path)?;
    let n = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in xattr name"))?;
    // SAFETY: pointers and length describe live buffers.
    if unsafe {
        libc::lsetxattr(
            c.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn remove_xattr(path: &Path, name: &str) {
    if let (Ok(c), Ok(n)) = (cpath(path), CString::new(name)) {
        // SAFETY: both strings are live.
        unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) };
    }
}

pub fn list_xattrs(path: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(c) = cpath(path) else { return vec![] };
    let mut buf = vec![0u8; 1024];
    let len = loop {
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        let r = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if r >= 0 {
            break r as usize;
        }
        if io::Error::last_os_error().raw_os_error() == Some(libc::ERANGE) && buf.len() < (1 << 20)
        {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return vec![];
    };
    let mut out = Vec::new();
    for name in buf[..len].split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = String::from_utf8_lossy(name).into_owned();
        if let Some(v) = get_xattr(path, &name) {
            out.push((name, v));
        }
    }
    out.sort();
    out
}

pub fn is_opaque(path: &Path, ns: XattrNs) -> bool {
    get_xattr(path, ns.opaque()).as_deref() == Some(b"y")
}

pub fn set_opaque(path: &Path, ns: XattrNs) -> io::Result<()> {
    set_xattr(path, ns.opaque(), b"y")
}

/// Every entry under `root`, as (relative path, metadata), sorted by path so
/// a parent always precedes its children.
pub fn walk(root: &Path) -> io::Result<Vec<(String, Metadata)>> {
    fn rec(dir: &Path, rel: &str, out: &mut Vec<(String, Metadata)>) -> io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name();
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF-8 file name under {}", dir.display()),
                )
            })?;
            let r = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            let m = std::fs::symlink_metadata(e.path())?;
            let is_dir = m.is_dir();
            out.push((r.clone(), m));
            if is_dir {
                rec(&e.path(), &r, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    rec(root, "", &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

pub fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

/// Remove a file, symlink, whiteout or whole directory tree.
pub fn remove_all(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // A directory without owner write or search permission blocks
                // its own removal for an unprivileged caller: open it up.
                for (rel, m) in walk(path).unwrap_or_default() {
                    if m.is_dir() {
                        let _ = std::fs::set_permissions(
                            path.join(rel),
                            std::fs::Permissions::from_mode(0o700),
                        );
                    }
                }
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                std::fs::remove_dir_all(path)
            }
        },
        Ok(_) => std::fs::remove_file(path),
    }
}

/// How ownership is set on copied entries.
pub enum Owner<'a> {
    /// Keep the source's owner (needs privilege over those IDs).
    Preserve,
    /// Leave whatever the copying process creates.
    Skip,
    /// Decide per relative path.
    Map(&'a dyn Fn(&str) -> (u32, u32)),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CopyStats {
    pub files: u64,
    pub bytes: u64,
    pub reflinked: u64,
    pub skipped: u64,
}

/// Copy file contents: a reflink when the filesystem supports it, otherwise
/// extent by extent so sparse files stay sparse.
pub fn copy_file(src: &Path, dst: &Path, stats: &mut CopyStats) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let s = std::fs::File::open(src)?;
    let d = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    let size = s.metadata()?.len();
    stats.files += 1;
    stats.bytes += size;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    // SAFETY: FICLONE takes the source descriptor as its argument.
    if size > 0 && unsafe { libc::ioctl(d.as_raw_fd(), FICLONE as _, s.as_raw_fd()) } == 0 {
        stats.reflinked += 1;
        return Ok(());
    }
    d.set_len(size)?;
    let mut off: i64 = 0;
    while (off as u64) < size {
        // SAFETY: lseek with SEEK_DATA/SEEK_HOLE on a descriptor we own.
        let data = unsafe { libc::lseek(s.as_raw_fd(), off, libc::SEEK_DATA) };
        let (start, end) = if data < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENXIO) => break, // only a hole remains
                _ => (off, size as i64),    // SEEK_DATA unsupported: copy the rest
            }
        } else {
            // SAFETY: as above.
            let hole = unsafe { libc::lseek(s.as_raw_fd(), data, libc::SEEK_HOLE) };
            (data, if hole < 0 { size as i64 } else { hole })
        };
        let mut pos_in = start;
        let mut pos_out = start;
        while pos_in < end {
            // SAFETY: both offsets are valid i64 out-parameters.
            let n = unsafe {
                libc::copy_file_range(
                    s.as_raw_fd(),
                    &mut pos_in,
                    d.as_raw_fd(),
                    &mut pos_out,
                    (end - pos_in) as usize,
                    0,
                )
            };
            if n > 0 {
                continue;
            }
            if n == 0 {
                break;
            }
            // Cross-filesystem or unsupported: fall back to read and write.
            use std::os::unix::fs::FileExt;
            let mut buf = vec![0u8; 1 << 16];
            while pos_in < end {
                let want = ((end - pos_in) as usize).min(buf.len());
                let got = s.read_at(&mut buf[..want], pos_in as u64)?;
                if got == 0 {
                    break;
                }
                d.write_all_at(&buf[..got], pos_in as u64)?;
                pos_in += got as i64;
            }
            break;
        }
        off = end;
    }
    Ok(())
}

fn set_mtime(path: &Path, m: &Metadata) {
    if let Ok(c) = cpath(path) {
        let ts = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: m.mtime() as _,
                tv_nsec: m.mtime_nsec() as _,
            },
        ];
        // SAFETY: `ts` is a two-element array as utimensat requires.
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                ts.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
    }
}

fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = cpath(path)?;
    // SAFETY: `c` is a live NUL-terminated string.
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Copies entries while preserving whiteouts, extended attributes, hard links
/// and sparse files.
pub struct Copier<'a> {
    pub owner: Owner<'a>,
    pub stats: CopyStats,
    links: HashMap<(u64, u64), PathBuf>,
    /// Extended attributes that are never copied (for example an opaque
    /// marker the caller wants to decide itself).
    pub skip_xattr: Option<&'static str>,
}

impl<'a> Copier<'a> {
    pub fn new(owner: Owner<'a>) -> Self {
        Copier {
            owner,
            stats: CopyStats::default(),
            links: HashMap::new(),
            skip_xattr: None,
        }
    }

    /// Apply owner, mode, extended attributes and mtime from `m` to `dst`.
    pub fn apply_attrs(&self, src: &Path, dst: &Path, rel: &str, m: &Metadata) -> io::Result<()> {
        match &self.owner {
            Owner::Preserve => chown(dst, m.uid(), m.gid())?,
            Owner::Map(f) => {
                let (u, g) = f(rel);
                chown(dst, u, g)?;
            }
            Owner::Skip => {}
        }
        if !m.file_type().is_symlink() && !is_whiteout(m) {
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(m.mode() & 0o7777))?;
        }
        for (name, value) in list_xattrs(src) {
            if Some(name.as_str()) == self.skip_xattr
                || name.starts_with("security.")
                || name.starts_with("system.")
            {
                continue;
            }
            // A namespace this process may not write (trusted.* when rootless)
            // is skipped rather than failing the whole copy.
            let _ = set_xattr(dst, &name, &value);
        }
        set_mtime(dst, m);
        Ok(())
    }

    /// Copy one entry (not its children). `dst` must not exist, except that
    /// an existing directory is reused.
    pub fn copy_entry(
        &mut self,
        src: &Path,
        dst: &Path,
        rel: &str,
        m: &Metadata,
    ) -> io::Result<()> {
        let ft = m.file_type();
        if ft.is_dir() {
            match std::fs::create_dir(dst) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        } else if ft.is_file() {
            if m.nlink() > 1 {
                if let Some(first) = self.links.get(&(m.dev(), m.ino())) {
                    return std::fs::hard_link(first, dst);
                }
                self.links.insert((m.dev(), m.ino()), dst.to_path_buf());
            }
            copy_file(src, dst, &mut self.stats)?;
        } else if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        } else if is_whiteout(m) {
            make_whiteout(dst)?;
        } else {
            // Sockets, FIFOs and real devices never belong in a layer.
            self.stats.skipped += 1;
            return Ok(());
        }
        self.apply_attrs(src, dst, rel, m)
    }

    /// Copy the whole tree under `src` into `dst` (created if missing).
    /// Names in `exclude` are left out at any depth.
    pub fn copy_tree(&mut self, src: &Path, dst: &Path, exclude: &[String]) -> io::Result<()> {
        std::fs::create_dir_all(dst)?;
        let entries = walk(src)?;
        let skip = |rel: &str| {
            exclude.iter().any(|x| {
                rel == x
                    || rel.starts_with(&format!("{x}/"))
                    || rel.ends_with(&format!("/{x}"))
                    || rel.contains(&format!("/{x}/"))
            })
        };
        for (rel, m) in &entries {
            if skip(rel) {
                continue;
            }
            self.copy_entry(&src.join(rel), &dst.join(rel), rel, m)?;
        }
        // Creating children changed directory mtimes: restore them, deepest first.
        for (rel, m) in entries.iter().rev() {
            if m.is_dir() && !skip(rel) {
                set_mtime(&dst.join(rel), m);
            }
        }
        Ok(())
    }
}

/// Merge the overlay-format layer at `layer` on top of the overlay-format
/// tree at `dst`, so that `dst` alone is equivalent to stacking both.
///
/// Whiteouts and opaque markers are kept, because layers below `dst` may
/// still hold the paths they hide. A directory that replaces a whiteout or a
/// non-directory becomes opaque for the same reason.
pub fn merge_layer(dst: &Path, layer: &Path, ns: XattrNs, owner: Owner) -> io::Result<CopyStats> {
    let mut cp = Copier::new(owner);
    cp.skip_xattr = Some(ns.opaque());
    for (rel, m) in walk(layer)? {
        let src = layer.join(&rel);
        let d = dst.join(&rel);
        let existing = std::fs::symlink_metadata(&d).ok();
        if is_whiteout(&m) {
            remove_all(&d)?;
            make_whiteout(&d)?;
        } else if m.is_dir() {
            let opaque = is_opaque(&src, ns);
            match existing {
                Some(e) if e.is_dir() => {
                    if opaque {
                        remove_all(&d)?;
                        cp.copy_entry(&src, &d, &rel, &m)?;
                        set_opaque(&d, ns)?;
                    } else {
                        cp.apply_attrs(&src, &d, &rel, &m)?;
                    }
                }
                Some(_) => {
                    remove_all(&d)?;
                    cp.copy_entry(&src, &d, &rel, &m)?;
                    set_opaque(&d, ns)?;
                }
                None => {
                    cp.copy_entry(&src, &d, &rel, &m)?;
                    if opaque {
                        set_opaque(&d, ns)?;
                    }
                }
            }
        } else {
            remove_all(&d)?;
            cp.copy_entry(&src, &d, &rel, &m)?;
        }
    }
    Ok(cp.stats)
}

/// Turn an overlay-format tree that has nothing below it into a plain tree:
/// drop whiteouts and opaque markers.
pub fn strip_overlay_markers(root: &Path, ns: XattrNs) -> io::Result<()> {
    for (rel, m) in walk(root)? {
        let p = root.join(&rel);
        if is_whiteout(&m) {
            let _ = std::fs::remove_file(&p);
        } else if m.is_dir() {
            remove_xattr(&p, ns.opaque());
        }
    }
    Ok(())
}

fn same_content(a: &Path, b: &Path, ma: &Metadata, mb: &Metadata) -> io::Result<bool> {
    if ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(hash_file(a)? == hash_file(b)?)
}

/// Compare two plain trees. Returns one line per difference; empty means
/// equal. Ownership and timestamps are not compared.
pub fn compare_trees(a: &Path, b: &Path) -> io::Result<Vec<String>> {
    let wa = walk(a)?;
    let wb = walk(b)?;
    let mb: HashMap<&str, &Metadata> = wb.iter().map(|(p, m)| (p.as_str(), m)).collect();
    let ma: HashMap<&str, &Metadata> = wa.iter().map(|(p, m)| (p.as_str(), m)).collect();
    let mut out = Vec::new();
    for (p, m) in &wa {
        let Some(o) = mb.get(p.as_str()) else {
            out.push(format!("only in left: {p}"));
            continue;
        };
        let (ta, tb) = (m.file_type(), o.file_type());
        if ta.is_dir() != tb.is_dir()
            || ta.is_file() != tb.is_file()
            || ta.is_symlink() != tb.is_symlink()
        {
            out.push(format!("type differs: {p}"));
            continue;
        }
        if !ta.is_symlink() && (m.mode() & 0o7777) != (o.mode() & 0o7777) {
            out.push(format!(
                "mode differs: {p} ({:o} vs {:o})",
                m.mode() & 0o7777,
                o.mode() & 0o7777
            ));
        }
        if ta.is_file() && !same_content(&a.join(p), &b.join(p), m, o)? {
            out.push(format!("content differs: {p}"));
        }
        if ta.is_symlink() && std::fs::read_link(a.join(p))? != std::fs::read_link(b.join(p))? {
            out.push(format!("link target differs: {p}"));
        }
    }
    for (p, _) in &wb {
        if !ma.contains_key(p.as_str()) {
            out.push(format!("only in right: {p}"));
        }
    }
    Ok(out)
}

/// Write into `out` the overlay-format layer that turns plain tree `base`
/// into plain tree `cur`: changed and new entries, plus whiteouts for
/// deleted paths. This is what an overlay upper directory would hold, and it
/// lets the plain-copy backend seal and diff like the overlay backend.
pub fn tree_delta(base: &Path, cur: &Path, out: &Path, owner: Owner) -> io::Result<CopyStats> {
    std::fs::create_dir_all(out)?;
    let wc = walk(cur)?;
    let wb = walk(base)?;
    let mc: HashMap<&str, &Metadata> = wc.iter().map(|(p, m)| (p.as_str(), m)).collect();
    let mut cp = Copier::new(owner);
    let ensure_parents = |cp: &mut Copier, rel: &str| -> io::Result<()> {
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            let d = out.join(&acc);
            if std::fs::symlink_metadata(&d).is_err() {
                let m = std::fs::symlink_metadata(cur.join(&acc))?;
                cp.copy_entry(&cur.join(&acc), &d, &acc, &m)?;
            }
        }
        Ok(())
    };
    for (rel, m) in &wc {
        let b = std::fs::symlink_metadata(base.join(rel)).ok();
        let ft = m.file_type();
        let changed = match &b {
            None => true,
            Some(o) => {
                let ot = o.file_type();
                if ft.is_dir() != ot.is_dir()
                    || ft.is_file() != ot.is_file()
                    || ft.is_symlink() != ot.is_symlink()
                {
                    true
                } else if ft.is_symlink() {
                    std::fs::read_link(cur.join(rel))? != std::fs::read_link(base.join(rel))?
                } else if (m.mode() & 0o7777) != (o.mode() & 0o7777) {
                    true
                } else {
                    ft.is_file() && !same_content(&cur.join(rel), &base.join(rel), m, o)?
                }
            }
        };
        if changed {
            ensure_parents(&mut cp, rel)?;
            let d = out.join(rel);
            if std::fs::symlink_metadata(&d).is_err() {
                cp.copy_entry(&cur.join(rel), &d, rel, m)?;
            }
        }
    }
    for (rel, _) in &wb {
        if mc.contains_key(rel.as_str()) {
            continue;
        }
        // A deleted directory's whiteout already covers everything under it.
        let parent = rel.rsplit_once('/').map(|(p, _)| p);
        if parent.is_some_and(|p| !mc.get(p).is_some_and(|m| m.is_dir())) {
            continue;
        }
        ensure_parents(&mut cp, rel)?;
        make_whiteout(&out.join(rel))?;
    }
    Ok(cp.stats)
}

/// Look `rel` up through a stack of lower layers (top first), honouring
/// whiteouts and opaque directories. Returns the path of the entry that the
/// merged view would show.
pub fn lower_lookup(
    lowers_top_first: &[PathBuf],
    rel: &str,
    ns: XattrNs,
) -> Option<(PathBuf, Metadata)> {
    let comps: Vec<&OsStr> = Path::new(rel).iter().collect();
    'layers: for layer in lowers_top_first {
        let mut cur = layer.clone();
        let mut opaque_above = false;
        for (i, comp) in comps.iter().enumerate() {
            cur.push(comp);
            match std::fs::symlink_metadata(&cur) {
                Err(_) => {
                    if opaque_above {
                        return None;
                    }
                    continue 'layers;
                }
                Ok(m) if is_whiteout(&m) => return None,
                Ok(m) => {
                    if i + 1 == comps.len() {
                        return Some((cur, m));
                    }
                    if !m.is_dir() {
                        return None;
                    }
                    if is_opaque(&cur, ns) {
                        opaque_above = true;
                    }
                }
            }
        }
    }
    None
}
