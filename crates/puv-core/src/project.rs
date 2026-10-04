use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
}

impl Project {
    pub fn discover(start: &Path) -> Result<Self> {
        let start = if start.is_file() {
            start.parent().unwrap_or(start).to_path_buf()
        } else {
            start.to_path_buf()
        };
        let mut current = start.clone();
        loop {
            if current.join("puv.toml").is_file() {
                return Ok(Self { root: current });
            }
            if !current.pop() {
                break;
            }
        }
        Err(Error::NoProject { start })
    }

    pub fn discover_optional(start: &Path) -> Option<Self> {
        Self::discover(start).ok()
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("puv.toml")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("puv.lock")
    }

    pub fn env_dir(&self) -> PathBuf {
        self.root.join(".puv")
    }

    pub fn lockb_path(&self) -> PathBuf {
        self.env_dir().join("lockb")
    }
}
