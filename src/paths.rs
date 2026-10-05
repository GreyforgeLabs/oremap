//! Path spelling helpers that match `pathlib.PurePosixPath` and
//! `os.path.expanduser` so report paths, roots, and warnings keep the exact
//! text earlier releases produced.

use std::fs;
use std::path::{Path, PathBuf};

/// Error raised when `~` or `~user` cannot be expanded (pathlib raises
/// `RuntimeError("Could not determine home directory.")`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeError;

impl std::fmt::Display for HomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Could not determine home directory.")
    }
}

/// `str(PurePosixPath(text))`: collapse duplicate separators and `.` parts,
/// drop trailing separators, keep a leading `//`, and spell empty as `.`.
pub fn normalize(text: &str) -> String {
    let root = if text.starts_with("//") && !text.starts_with("///") {
        "//"
    } else if text.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = text
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = format!("{root}{}", parts.join("/"));
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Join a normalized directory spelling with one child name the way
/// `Path(dir) / name` renders it.
pub fn join(dir: &str, name: &str) -> String {
    if dir == "." {
        normalize(name)
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn passwd_home(matches: impl Fn(&[&str]) -> bool) -> Option<String> {
    let content = fs::read_to_string("/etc/passwd").ok()?;
    content.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 7 && matches(&fields) {
            Some(fields[5].to_string())
        } else {
            None
        }
    })
}

/// `os.path.expanduser` applied to one `~` or `~name` prefix. Returns `None`
/// when the user database has no answer (Python returns the input unchanged).
fn expand_tilde(prefix: &str) -> Option<String> {
    let home = if prefix == "~" {
        match std::env::var_os("HOME") {
            Some(value) => value.to_string_lossy().into_owned(),
            None => {
                let uid = rustix::process::getuid().as_raw().to_string();
                passwd_home(|fields| fields[2] == uid)?
            }
        }
    } else {
        let name = &prefix[1..];
        passwd_home(|fields| fields[0] == name)?
    };
    let trimmed = home.trim_end_matches('/');
    Some(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// `str(Path(text).expanduser())` for an already normalized spelling.
pub fn expanduser(normalized: &str) -> Result<String, HomeError> {
    if !normalized.starts_with('~') {
        return Ok(normalized.to_string());
    }
    let (first, rest) = match normalized.split_once('/') {
        Some((first, rest)) => (first, Some(rest)),
        None => (normalized, None),
    };
    let home = expand_tilde(first).ok_or(HomeError)?;
    if home.starts_with('~') {
        return Err(HomeError);
    }
    Ok(join_home(&home, rest))
}

/// Append the part after `~` to an expanded home. pathlib parses the home on
/// its own first, so a home of `/` gives `/rest`, not the `//rest` root that
/// plain string joining would produce.
fn join_home(home: &str, rest: Option<&str>) -> String {
    let home = normalize(home);
    match rest {
        Some(rest) if home.ends_with('/') => normalize(&format!("{home}{rest}")),
        Some(rest) => normalize(&format!("{home}/{rest}")),
        None => home,
    }
}

/// `str(Path.home())`.
pub fn home() -> String {
    match expand_tilde("~") {
        Some(home) => normalize(&home),
        None => "~".to_string(),
    }
}

/// Current working directory as `str(Path.cwd())`.
pub fn cwd() -> String {
    std::env::current_dir()
        .map(|path| normalize(&path.to_string_lossy()))
        .unwrap_or_else(|_| ".".to_string())
}

/// Display text for a filesystem path.
pub fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Convert a display spelling back to a filesystem path.
pub fn to_path(text: &str) -> PathBuf {
    PathBuf::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_pathlib() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize("./"), ".");
        assert_eq!(normalize("//a//b/./c/"), "//a/b/c");
        assert_eq!(normalize("///a"), "/a");
        assert_eq!(normalize("a/../b"), "a/../b");
        assert_eq!(normalize("a/./b/."), "a/b");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("~/x/"), "~/x");
    }

    #[test]
    fn join_matches_pathlib() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("u", "a.service"), "u/a.service");
        assert_eq!(join(".", "a.service"), "a.service");
    }

    #[test]
    fn expanduser_leaves_plain_paths() {
        assert_eq!(expanduser("/srv/x"), Ok("/srv/x".to_string()));
        assert_eq!(expanduser("rel"), Ok("rel".to_string()));
        assert_eq!(
            expanduser("~definitely-missing-user-oremap/x"),
            Err(HomeError)
        );
    }

    #[test]
    fn root_home_joins_like_pathlib() {
        assert_eq!(join_home("/", Some("bin")), "/bin");
        assert_eq!(join_home("/", None), "/");
        assert_eq!(join_home("//srv", Some("x")), "//srv/x");
        assert_eq!(join_home("/home/u", Some("a/b")), "/home/u/a/b");
        assert_eq!(join_home(".", Some("bin")), "bin");
    }
}
