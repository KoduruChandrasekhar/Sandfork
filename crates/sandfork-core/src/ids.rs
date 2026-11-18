//! Identifiers. Random ids come from `getrandom(2)`; layer ids are content hashes.

/// Fill `buf` from the kernel's random pool.
pub fn random_bytes(buf: &mut [u8]) {
    let mut off = 0;
    while off < buf.len() {
        // SAFETY: the pointer and length describe the unfilled tail of `buf`.
        let n = unsafe { libc::getrandom(buf[off..].as_mut_ptr().cast(), buf.len() - off, 0) };
        if n > 0 {
            off += n as usize;
        } else {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                // getrandom cannot fail on any kernel Sandfork supports once the
                // pool is initialised; fall back to the clock rather than abort.
                let t = crate::monotonic_ns().to_le_bytes();
                for (i, b) in buf[off..].iter_mut().enumerate() {
                    *b = t[i % 8] ^ (i as u8).wrapping_mul(31);
                }
                return;
            }
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// A new random id such as `sbx-3f9a1c0b77e2`.
pub fn new_id(prefix: &str) -> String {
    let mut b = [0u8; 6];
    random_bytes(&mut b);
    format!("{prefix}-{}", hex(&b))
}

pub fn sandbox_id() -> String {
    new_id("sbx")
}
pub fn group_id() -> String {
    new_id("grp")
}
pub fn snapshot_id() -> String {
    new_id("snap")
}
pub fn checkpoint_id() -> String {
    new_id("ckpt")
}

/// True if `s` is safe to use as a single path component.
pub fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_safe() {
        let a = sandbox_id();
        let b = sandbox_id();
        assert_ne!(a, b);
        assert!(a.starts_with("sbx-") && a.len() == 16);
        assert!(is_safe_component(&a));
        assert!(!is_safe_component("../x"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("a/b"));
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0x00, 0xff, 0x1a]), "00ff1a");
    }
}
