//! Path globs for `allowed_paths`: `*` and `?` match within one component,
//! `**` matches any number of components (including none).

/// Match a `/`-separated relative path against a glob.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segs(&pat, &segs)
}

fn match_segs(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => {
            // `**` swallows zero or more whole components.
            (0..=segs.len()).any(|i| match_segs(rest, &segs[i..]))
        }
        Some((p, rest)) => match segs.split_first() {
            Some((s, srest)) => {
                match_component(p.as_bytes(), s.as_bytes()) && match_segs(rest, srest)
            }
            None => false,
        },
    }
}

fn match_component(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| match_component(rest, &s[i..])),
        Some((b'?', rest)) => !s.is_empty() && match_component(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && match_component(rest, &s[1..]),
    }
}

/// True if any pattern matches.
pub fn any_match(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star() {
        assert!(glob_match("src/**", "src/a.py"));
        assert!(glob_match("src/**", "src/pkg/deep/a.py"));
        assert!(glob_match("src/**", "src"));
        assert!(!glob_match("src/**", "tests/a.py"));
        assert!(!glob_match("src/**", "srcx/a.py"));
        assert!(glob_match("**/*.py", "a.py"));
        assert!(glob_match("**/*.py", "x/y/a.py"));
        assert!(!glob_match("**/*.py", "x/y/a.txt"));
    }

    #[test]
    fn single_component() {
        assert!(glob_match("src/*.py", "src/a.py"));
        assert!(!glob_match("src/*.py", "src/pkg/a.py"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("answer.txt", "answer.txt"));
        assert!(!glob_match("answer.txt", "answer.txt.bak"));
    }

    #[test]
    fn exploit_paths_do_not_match_src_glob() {
        let allowed = vec!["src/**".to_string()];
        for p in [
            "conftest.py",
            "pytest.ini",
            "sitecustomize.py",
            "visible/test_x.py",
            "evil.pth",
            "setup.cfg",
        ] {
            assert!(!any_match(&allowed, p), "{p} must be rejected");
        }
    }
}
