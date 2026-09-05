//! Lexical path normalization shared by every place that makes an access
//! decision on a path string.
//!
//! Both `PathGrant::covers` and `Capability::is_path_allowed` are prefix
//! checks. A prefix check on the raw string is only meaningful once `.` and
//! `..` segments are gone: `/srv/data/../../etc/shadow` starts with
//! `/srv/data/` and would otherwise be granted. This module resolves those
//! segments without touching the filesystem, so it works the same for a path
//! that does not exist yet (a file about to be created) and for a remote
//! filesystem Ferrite cannot inspect.
//!
//! Symlinks are deliberately not handled here: that needs the real
//! filesystem and is done by the side that owns it (see `ferrous::server`).

/// Normalizes `path` lexically:
///
/// * backslashes become forward slashes
/// * repeated separators collapse
/// * `.` segments are dropped
/// * `..` segments pop the previous segment, and are clamped at the root so a
///   path can never climb above `/` (or above a Windows drive root)
/// * a trailing slash is removed, except for the root itself
///
/// A Windows drive prefix (`C:`) is kept as the first segment so that
/// `C:/a/../b` becomes `C:/b`, and a leading `/` is preserved so that
/// `/C:/a` (Ferrite's virtual-root convention) stays distinguishable from
/// `C:/a`.
pub fn normalize_path(path: &str) -> String {
    let s = path.trim().replace('\\', "/");
    let absolute = s.starts_with('/');

    let mut out: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                // Never pop a drive prefix, and never climb above the root.
                match out.last() {
                    Some(last) if !is_drive(last) => {
                        out.pop();
                    }
                    _ => {}
                }
            }
            other => out.push(other),
        }
    }

    let body = out.join("/");
    if absolute {
        format!("/{}", body)
    } else if body.is_empty() {
        "/".to_string()
    } else {
        body
    }
}

fn is_drive(seg: &str) -> bool {
    let b = seg.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Whether `path` is `root` itself or lies beneath it, comparing normalized
/// forms and requiring a `/` boundary so `/foo` does not cover `/foobar`.
/// A root of `/` covers everything.
pub fn is_within(root: &str, path: &str) -> bool {
    let root = normalize_path(root);
    if root == "/" {
        return true;
    }
    let path = normalize_path(path);
    path == root || path.starts_with(&format!("{}/", root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_clean_paths_alone() {
        assert_eq!(normalize_path("/srv/data"), "/srv/data");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("C:/Users"), "C:/Users");
    }

    #[test]
    fn strips_dot_and_trailing_separators() {
        assert_eq!(normalize_path("/srv/./data/"), "/srv/data");
        assert_eq!(normalize_path("/srv//data"), "/srv/data");
        assert_eq!(normalize_path("/srv\\data\\"), "/srv/data");
    }

    #[test]
    fn resolves_parent_segments() {
        assert_eq!(normalize_path("/srv/data/../other"), "/srv/other");
        assert_eq!(normalize_path("/srv/data/sub/../../x"), "/srv/x");
    }

    #[test]
    fn clamps_at_root() {
        assert_eq!(normalize_path("/srv/data/../../../../etc/shadow"), "/etc/shadow");
        assert_eq!(normalize_path("/.."), "/");
        assert_eq!(normalize_path(".."), "/");
    }

    #[test]
    fn keeps_windows_drive_prefixes() {
        assert_eq!(normalize_path("C:\\srv\\..\\..\\Windows"), "C:/Windows");
        assert_eq!(normalize_path("/C:/srv/../x"), "/C:/x");
        assert_eq!(normalize_path("C:"), "C:");
    }

    #[test]
    fn is_within_requires_a_segment_boundary() {
        assert!(is_within("/srv/data", "/srv/data"));
        assert!(is_within("/srv/data", "/srv/data/x/y"));
        assert!(!is_within("/srv/data", "/srv/database"));
        assert!(is_within("/", "/anything"));
    }

    #[test]
    fn is_within_sees_through_traversal() {
        assert!(!is_within("/srv/data", "/srv/data/../../etc/shadow"));
        assert!(!is_within("/srv/data", "/srv/data/..\\..\\etc/shadow"));
        assert!(is_within("/srv/data", "/srv/data/sub/../still_inside"));
    }
}
