//! Where monolithd finds its versioned configuration.

use std::path::PathBuf;

/// The repository's `config/` directory, located through the source tree the binary was
/// built from. Moving the checkout therefore needs a rebuild.
pub fn config_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config")
}
