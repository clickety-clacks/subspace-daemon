use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

pub struct StateLock {
    _file: File,
    _path: PathBuf,
}

impl StateLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed opening state lock {}", path.display()))?;
        file.try_lock_exclusive()
            .with_context(|| format!("failed acquiring state lock {}", path.display()))?;
        Ok(Self {
            _file: file,
            _path: path.to_path_buf(),
        })
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed opening state lock {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                _path: path.to_path_buf(),
            })),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => {
                Err(err).with_context(|| format!("failed acquiring state lock {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reports_lock_held_without_string_matching() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.lock");
        let _held = StateLock::acquire(&path).unwrap();
        assert!(StateLock::try_acquire(&path).unwrap().is_none());
    }
}
