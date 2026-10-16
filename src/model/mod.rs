//! Plugin-owned state: groups (path-keyed), sidebar fold state, and the popup tree.
pub mod groups;
pub mod sidebar;
pub mod tree;

use std::io;
use std::path::{Path, PathBuf};

pub const PLUGIN_ID: &str = "space-kit";

/// `$HOME` (empty when unset, so paths stay relative instead of panicking).
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `$HERDR_PLUGIN_STATE_DIR`, else `$XDG_STATE_HOME/herdr/plugins/space-kit`.
#[cfg(not(test))]
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state"));
    state.join("herdr/plugins").join(PLUGIN_ID)
}

/// Tests never write to the real state dir.
#[cfg(test)]
pub fn state_dir() -> PathBuf {
    std::env::temp_dir().join(format!("space-kit-state-{}", std::process::id()))
}

/// Atomic write through a pid-named temp file in the same directory. Creates the directory.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// realpath: resolve symlinks of the longest existing ancestor, keep the rest as written.
pub fn realpath(path: &str) -> String {
    let mut existing = PathBuf::from(path);
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let joined = rest.iter().rev().fold(resolved, |acc: PathBuf, part| acc.join(part));
            return joined.to_string_lossy().into_owned();
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realpath_resolves_the_existing_ancestor_of_a_missing_path() {
        let tmp = realpath("/tmp");
        assert_eq!(realpath("/tmp/space-kit-missing/x"), format!("{tmp}/space-kit-missing/x"));
    }

    #[test]
    fn realpath_keeps_a_relative_path_it_cannot_resolve() {
        assert_eq!(realpath("space-kit-relative-missing"), "space-kit-relative-missing");
    }
}
