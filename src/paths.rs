//! Where monolithd finds its versioned configuration, first match wins:
//!
//! 1. `$MONOLITHD_CONFIG_DIR`, when set and non-empty. The units set it explicitly.
//! 2. `$XDG_CONFIG_HOME/monolithd` (`~/.config/monolithd` when `XDG_CONFIG_HOME` is unset,
//!    empty or relative), when that directory exists.
//! 3. The `config/` directory of the source tree the binary was built from.
//!
//! Each service logs which directory it uses. Tests skip step 2, so `cargo test` checks
//! the repository's own `config/`, never a user's live copy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const CONFIG_ENV: &str = "MONOLITHD_CONFIG_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Environment,
    User,
    SourceTree,
}

pub fn config_dir() -> PathBuf {
    lookup().0
}

/// A one-line account of which configuration directory is in use and why.
pub fn describe() -> String {
    let (directory, source) = lookup();
    let source = match source {
        Source::Environment => CONFIG_ENV,
        Source::User => "user config directory",
        Source::SourceTree => "compiled in",
    };
    format!("config {} ({source})", directory.display())
}

fn lookup() -> (PathBuf, Source) {
    let user = if cfg!(test) { None } else { user_dir(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME")) };
    resolve(std::env::var_os(CONFIG_ENV), user, Path::is_dir)
}

/// `$XDG_CONFIG_HOME/monolithd`, or `$HOME/.config/monolithd`, per the XDG base directory
/// rules: a relative `XDG_CONFIG_HOME` is ignored.
fn user_dir(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg_config_home.map(PathBuf::from) {
        Some(base) if base.is_absolute() => base,
        _ => PathBuf::from(home.filter(|home| !home.is_empty())?).join(".config"),
    };
    Some(base.join("monolithd"))
}

fn resolve(environment: Option<OsString>, user: Option<PathBuf>, exists: impl Fn(&Path) -> bool) -> (PathBuf, Source) {
    match environment {
        Some(value) if !value.is_empty() => (PathBuf::from(value), Source::Environment),
        _ => match user.filter(|directory| exists(directory)) {
            Some(directory) => (directory, Source::User),
            None => (PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config"), Source::SourceTree),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_tree() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
    }

    #[test]
    fn the_environment_wins_over_everything() {
        let user = Some(PathBuf::from("/home/u/.config/monolithd"));
        assert_eq!(resolve(Some("/srv/monolithd/config".into()), user, |_| true), (PathBuf::from("/srv/monolithd/config"), Source::Environment));
    }

    #[test]
    fn an_existing_user_directory_comes_next() {
        let user = PathBuf::from("/home/u/.config/monolithd");
        assert_eq!(resolve(None, Some(user.clone()), |_| true), (user.clone(), Source::User));
        assert_eq!(resolve(Some(OsString::new()), Some(user.clone()), |_| true), (user, Source::User), "an empty variable counts as unset");
    }

    #[test]
    fn otherwise_the_source_tree() {
        assert_eq!(resolve(None, Some(PathBuf::from("/home/u/.config/monolithd")), |_| false), (source_tree(), Source::SourceTree), "a missing user directory is skipped");
        assert_eq!(resolve(None, None, |_| true), (source_tree(), Source::SourceTree));
    }

    #[test]
    fn the_user_directory_follows_the_xdg_rules() {
        assert_eq!(user_dir(Some("/cfg".into()), Some("/home/u".into())), Some(PathBuf::from("/cfg/monolithd")));
        assert_eq!(user_dir(None, Some("/home/u".into())), Some(PathBuf::from("/home/u/.config/monolithd")));
        assert_eq!(user_dir(Some("".into()), Some("/home/u".into())), Some(PathBuf::from("/home/u/.config/monolithd")), "empty is unset");
        assert_eq!(user_dir(Some("relative".into()), Some("/home/u".into())), Some(PathBuf::from("/home/u/.config/monolithd")), "relative is ignored");
        assert_eq!(user_dir(None, None), None);
    }

    #[test]
    fn tests_never_read_a_live_user_config() {
        if std::env::var_os(CONFIG_ENV).is_none() {
            assert_eq!(lookup(), (source_tree(), Source::SourceTree));
        }
    }
}
