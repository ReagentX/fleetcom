//! Pure path helpers shared by the client (the `@` picker, dir labels) and the
//! supervisor (resolving a session recipe's stored dirs). No filesystem access:
//! `resolve`/`lexical_clean` are lexical, so a path need not exist to be tidied
//! and `/tmp` never becomes `/private/tmp`.

use std::path::{Component, Path, PathBuf};

/// Shorten a path for display: `$HOME` collapses to `~`. Everything else stays
/// absolute, so two directories never render as the same label.
pub fn abbreviate(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = s.strip_prefix(&home)
    {
        if rest.is_empty() {
            return "~".to_string();
        } else if rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

/// Expand a leading `~` (alone or `~/…`) to `$HOME`.
pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{rest}");
    }
    s.to_string()
}

/// Collapse `.` and `..` lexically (no filesystem access, no symlink
/// resolution): `/a/b/../c` → `/a/c`. Kept lexical rather than
/// `fs::canonicalize` so `/tmp` stays `/tmp` (not `/private/tmp`) and the path
/// need not exist yet. This only tidies what the user typed.
pub fn lexical_clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}       // `/..` stays `/`
                _ => out.push(Component::ParentDir), // leading `..` in a relative path
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Turn a typed path fragment into a fully-qualified, lexically-clean absolute
/// path: `~`/relative are resolved against `base`, then `lexical_clean`
/// collapses `.`/`..` and trailing slashes so a stored cwd reads as `~/a/c`,
/// never `~/a/b/../c` or `~/test//`. `base` is the invocation dir for the `@`
/// picker and the daemon cwd for session load. Both absolute, so a recipe's
/// stored `~/a` or `/tmp` never actually needs it.
pub fn resolve(base: &Path, s: &str) -> PathBuf {
    let expanded = expand_tilde(s);
    let p = if expanded.is_empty() {
        base.to_path_buf()
    } else if Path::new(&expanded).is_absolute() {
        PathBuf::from(expanded)
    } else {
        base.join(expanded)
    };
    lexical_clean(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_normalizes_trailing_slash() {
        let base = Path::new("/base");
        assert_eq!(resolve(base, "/tmp/"), PathBuf::from("/tmp"));
        assert_eq!(resolve(base, "/tmp"), PathBuf::from("/tmp"));
    }

    #[test]
    fn resolve_collapses_dotdot() {
        let base = Path::new("/base");
        assert_eq!(resolve(base, "/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve(base, "/a/b/../../c"), PathBuf::from("/c"));
        assert_eq!(resolve(base, "/../x"), PathBuf::from("/x")); // can't climb past root
        assert_eq!(
            resolve(base, "/Users/x/Code/Rust/fleetcom/../imessage-exporter"),
            PathBuf::from("/Users/x/Code/Rust/imessage-exporter")
        );
        // relative input resolves against base, then collapses
        assert_eq!(resolve(base, "sub/.."), PathBuf::from("/base"));
    }
}
