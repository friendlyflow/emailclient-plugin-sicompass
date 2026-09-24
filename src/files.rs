//! Writing the plugin's own state file.

use std::path::Path;

/// Write `contents` to `path` through a temporary file and a rename, so a
/// reader never sees a half-written file.
pub fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Create `dir` and its parents, ignoring failure (the write after it says so).
pub fn make_dirs(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}
