//! Where monolithd finds its versioned configuration.
//!
//! `$MONOLITHD_CONFIG_DIR`, when set and non-empty, names the directory; otherwise it is
//! the `config/` directory of the source tree the binary was built from, so a binary
//! built elsewhere or a moved checkout still works when the variable is set. The units
//! that read configuration set it explicitly, and each logs which directory it uses.

use std::ffi::OsString;
use std::path::PathBuf;

pub const CONFIG_ENV: &str = "MONOLITHD_CONFIG_DIR";

pub fn config_dir() -> PathBuf {
    resolve(std::env::var_os(CONFIG_ENV)).0
}

/// A one-line account of which configuration directory is in use and why.
pub fn describe() -> String {
    let (directory, from_environment) = resolve(std::env::var_os(CONFIG_ENV));
    let source = if from_environment { CONFIG_ENV } else { "compiled in" };
    format!("config {} ({source})", directory.display())
}

fn resolve(value: Option<OsString>) -> (PathBuf, bool) {
    match value {
        Some(value) if !value.is_empty() => (PathBuf::from(value), true),
        _ => (PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_overrides_the_compiled_in_directory() {
        assert_eq!(resolve(Some("/srv/monolithd/config".into())), (PathBuf::from("/srv/monolithd/config"), true));
    }

    #[test]
    fn unset_or_empty_falls_back_to_the_source_tree() {
        let compiled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config");
        assert_eq!(resolve(None), (compiled.clone(), false));
        assert_eq!(resolve(Some(OsString::new())), (compiled, false));
    }
}
