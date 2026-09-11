//! Filesystem safety, durability, and lease mechanics.
//!
//! The execution lease and the short journal append lock are intentionally
//! different locks. The lease covers the complete actuator run; the append
//! lock only serializes a single durable journal transition.

use crate::{Result, Store, StoreError};
use std::{
    fs::{self, File},
    path::Path,
};

pub(crate) struct FileLock(pub(crate) std::path::PathBuf);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn reject_symlink(path: &Path) -> Result<()> {
    if path.exists() && fs::symlink_metadata(path)?.file_type().is_symlink() {
        Err(StoreError::InvalidId)
    } else {
        Ok(())
    }
}

pub(crate) fn list_dirs(root: &Path) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_type()?.is_symlink() {
            if let Some(id) = entry.file_name().to_str() {
                if Store::safe_id(id).is_ok() {
                    result.push(id.to_owned());
                }
            }
        }
    }
    result.sort();
    Ok(result)
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
