use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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

    pub fn try_acquire(path: &Path) -> Result<Self> {
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
            Ok(()) => Ok(Self {
                _file: file,
                _path: path.to_path_buf(),
            }),
            Err(_) => bail!("subspace-daemon is running; stop it before running setup"),
        }
    }
}
