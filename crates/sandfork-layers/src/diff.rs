//! What a sandbox changed: a change list from its writable layer, plus
//! unified text diffs. Three things use it: verification by transplant, the
//! trajectory store, and `sf diff`.

use crate::tree::{self, XattrNs};
use serde::{Deserialize, Serialize};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub is_dir: bool,
    /// A directory that replaced the one below instead of merging with it.
    pub opaque: bool,
    pub size: u64,
}

/// Walk a writable layer and classify each entry against the lower layers.
///
/// A file present in the layer is added or modified (the lower layers tell
/// which), a whiteout is a deletion, an opaque directory is a replacement.
/// Directories that merely contain changes are not reported.
pub fn changes(lowers_top_first: &[PathBuf], upper: &Path, ns: XattrNs) -> io::Result<Vec<Change>> {
    let mut out = Vec::new();
    let mut opaque_dirs: Vec<String> = Vec::new();
    for (rel, m) in tree::walk(upper)? {
        let p = upper.join(&rel);
        let under_opaque = opaque_dirs
            .iter()
            .any(|d| rel.starts_with(d.as_str()) && rel.as_bytes().get(d.len()) == Some(&b'/'));
        let lower = tree::lower_lookup(lowers_top_first, &rel, ns);
        if tree::is_whiteout(&m) {
            if let Some((_, lm)) = lower {
                out.push(Change {
                    path: rel,
                    kind: ChangeKind::Deleted,
                    is_dir: lm.is_dir(),
                    opaque: false,
                    size: 0,
                });
            }
            continue;
        }
        if m.is_dir() {
            let opaque = tree::is_opaque(&p, ns);
            match &lower {
                Some((_, lm)) if lm.is_dir() => {
                    if opaque {
                        opaque_dirs.push(rel.clone());
                        out.push(Change {
                            path: rel,
                            kind: ChangeKind::Modified,
                            is_dir: true,
                            opaque: true,
                            size: 0,
                        });
                    }
                }
                _ => out.push(Change {
                    path: rel,
                    kind: ChangeKind::Added,
                    is_dir: true,
                    opaque: false,
                    size: 0,
                }),
            }
            continue;
        }
        let ft = m.file_type();
        if !ft.is_file() && !ft.is_symlink() {
            continue;
        }
        let kind = match &lower {
            None => ChangeKind::Added,
            Some((lp, lm)) => {
                let same_type = lm.file_type().is_file() == ft.is_file()
                    && lm.file_type().is_symlink() == ft.is_symlink();
                let unchanged = same_type
                    && if ft.is_symlink() {
                        std::fs::read_link(&p).ok() == std::fs::read_link(lp).ok()
                    } else {
                        (lm.mode() & 0o7777) == (m.mode() & 0o7777)
                            && lm.len() == m.len()
                            && tree::hash_file(lp)? == tree::hash_file(&p)?
                    };
                // A copy-up with no real change is not a change, unless an
                // opaque parent means the lower copy is no longer visible.
                if unchanged && !under_opaque {
                    continue;
                }
                ChangeKind::Modified
            }
        };
        out.push(Change {
            path: rel,
            kind,
            is_dir: false,
            opaque: false,
            size: m.len(),
        });
    }
    Ok(out)
}

/// Largest file for which a text diff is produced.
pub const MAX_TEXT_DIFF: u64 = 1 << 20;

fn read_text(path: &Path) -> Option<String> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.len() > MAX_TEXT_DIFF {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A unified diff between the lower and upper versions of one file, with the
/// number of lines added and removed. `None` for binary or oversized files.
pub fn text_diff(
    lower: Option<&Path>,
    upper: Option<&Path>,
    path: &str,
) -> Option<(String, usize, usize)> {
    let a = match lower {
        Some(p) => read_text(p)?,
        None => String::new(),
    };
    let b = match upper {
        Some(p) => read_text(p)?,
        None => String::new(),
    };
    Some(unified_diff(&a, &b, path))
}

/// Shortest edit script by Myers' algorithm. `None` if it would need more
/// than `max_d` edits (the caller falls back to replace-everything).
fn myers(a: &[&str], b: &[&str], max_d: usize) -> Option<Vec<(u8, usize)>> {
    let (n, m) = (a.len(), b.len());
    let max = n + m;
    if max == 0 {
        return Some(vec![]);
    }
    let off = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = false;
    'outer: for d in 0..=max as isize {
        if d as usize > max_d {
            return None;
        }
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + off) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while (x as usize) < n && (y as usize) < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                found = true;
                break 'outer;
            }
            k += 2;
        }
    }
    if !found {
        return None;
    }
    let (mut x, mut y) = (n as isize, m as isize);
    let mut ops = Vec::new();
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && v[(k - 1 + off) as usize] < v[(k + 1 + off) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(prev_k + off) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push((b' ', (x - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push((b'+', (y - 1) as usize));
            } else {
                ops.push((b'-', (x - 1) as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

/// Unified diff with three lines of context. Returns (text, added, removed).
pub fn unified_diff(old: &str, new: &str, path: &str) -> (String, usize, usize) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Trim the common prefix and suffix: an agent's edit touches few lines.
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let middle = if am.len() + bm.len() <= 4000 {
        myers(am, bm, 2000)
    } else {
        None
    }
    .unwrap_or_else(|| {
        (0..am.len())
            .map(|i| (b'-', i))
            .chain((0..bm.len()).map(|j| (b'+', j)))
            .collect()
    });

    // Rebuild the full edit script as (tag, old line index, new line index).
    let mut ops: Vec<(u8, usize, usize)> = Vec::with_capacity(a.len() + bm.len());
    let (mut i, mut j) = (0usize, 0usize);
    for _ in 0..pre {
        ops.push((b' ', i, j));
        i += 1;
        j += 1;
    }
    for (tag, _) in middle {
        ops.push((tag, i, j));
        match tag {
            b' ' => {
                i += 1;
                j += 1;
            }
            b'-' => i += 1,
            _ => j += 1,
        }
    }
    for _ in 0..suf {
        ops.push((b' ', i, j));
        i += 1;
        j += 1;
    }
    let added = ops.iter().filter(|o| o.0 == b'+').count();
    let removed = ops.iter().filter(|o| o.0 == b'-').count();
    if added == 0 && removed == 0 {
        return (String::new(), 0, 0);
    }

    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    const CTX: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != b' ')
        .map(|(i, _)| i)
        .collect();
    let mut idx = 0;
    while idx < changed.len() {
        let start = changed[idx].saturating_sub(CTX);
        let mut end = changed[idx];
        while idx + 1 < changed.len() && changed[idx + 1] <= end + 2 * CTX + 1 {
            idx += 1;
            end = changed[idx];
        }
        let stop = (end + CTX + 1).min(ops.len());
        let hunk = &ops[start..stop];
        let old_count = hunk.iter().filter(|o| o.0 != b'+').count();
        let new_count = hunk.iter().filter(|o| o.0 != b'-').count();
        let old_start = if old_count == 0 {
            hunk[0].1
        } else {
            hunk[0].1 + 1
        };
        let new_start = if new_count == 0 {
            hunk[0].2
        } else {
            hunk[0].2 + 1
        };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for (tag, oi, nj) in hunk {
            let line = if *tag == b'+' { b[*nj] } else { a[*oi] };
            out.push(*tag as char);
            out.push_str(line);
            out.push('\n');
        }
        idx += 1;
    }
    (out, added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply a unified diff produced by `unified_diff` back onto `old`.
    fn apply(old: &str, diff: &str) -> String {
        let a: Vec<&str> = old.lines().collect();
        let mut out: Vec<String> = Vec::new();
        let mut pos = 0usize;
        for line in diff.lines().skip(2) {
            if let Some(h) = line.strip_prefix("@@ -") {
                let start: usize = h.split(',').next().unwrap().parse().unwrap();
                let count: usize = h
                    .split(',')
                    .nth(1)
                    .unwrap()
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                let begin = if count == 0 { start } else { start - 1 };
                while pos < begin {
                    out.push(a[pos].to_string());
                    pos += 1;
                }
            } else if let Some(l) = line.strip_prefix('+') {
                out.push(l.to_string());
            } else if line.starts_with('-') {
                pos += 1;
            } else if let Some(l) = line.strip_prefix(' ') {
                out.push(l.to_string());
                pos += 1;
            }
        }
        while pos < a.len() {
            out.push(a[pos].to_string());
            pos += 1;
        }
        out.join("\n")
    }

    #[test]
    fn single_line_edit() {
        let old = "def f(a, b):\n    return a < b\n\nx = 1\n";
        let new = "def f(a, b):\n    return a <= b\n\nx = 1\n";
        let (d, added, removed) = unified_diff(old, new, "src/f.py");
        assert_eq!((added, removed), (1, 1));
        assert!(d.contains("-    return a < b\n+    return a <= b\n"), "{d}");
        assert!(
            d.starts_with("--- a/src/f.py\n+++ b/src/f.py\n@@ -1,4 +1,4 @@\n"),
            "{d}"
        );
    }

    #[test]
    fn identical_files_have_no_diff() {
        assert_eq!(unified_diff("a\nb\n", "a\nb\n", "x"), (String::new(), 0, 0));
    }

    #[test]
    fn diffs_apply_back() {
        let cases = [
            ("", "a\nb\n"),
            ("a\nb\n", ""),
            (
                "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n",
                "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nK\nl\nm\n",
            ),
            ("1\n2\n3\n", "0\n1\n2\n3\n"),
            ("x\ny\nz\n", "z\ny\nx\n"),
        ];
        for (old, new) in cases {
            let (d, _, _) = unified_diff(old, new, "f");
            let want: Vec<&str> = new.lines().collect();
            assert_eq!(
                apply(old, &d),
                want.join("\n"),
                "old={old:?} new={new:?}\n{d}"
            );
        }
    }
}
