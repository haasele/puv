use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Cache, data, and global shim directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    pub cache: PathBuf,
    pub data: PathBuf,
    pub bins: PathBuf,
}

impl Dirs {
    pub fn from_env() -> Self {
        let base = directories::BaseDirs::new();
        let home = base
            .as_ref()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let cache = env_path("PUV_CACHE_DIR").unwrap_or_else(|| {
            base.as_ref()
                .map(|dirs| dirs.cache_dir().join("puv"))
                .unwrap_or_else(|| home.join(".cache/puv"))
        });
        let data = env_path("PUV_DATA_DIR").unwrap_or_else(|| {
            base.as_ref()
                .map(|dirs| dirs.data_local_dir().join("puv"))
                .unwrap_or_else(|| home.join(".local/share/puv"))
        });
        let bins = env_path("PUV_BIN_DIR").unwrap_or_else(|| home.join(".local/bin"));
        Self { cache, data, bins }
    }

    pub fn ensure(&self) -> Result<()> {
        for path in [
            self.cache.clone(),
            self.packages(),
            self.runtime_cache(),
            self.metadata(),
            self.indexes(),
            self.classmaps(),
            self.data.clone(),
            self.runtimes(),
            self.tools(),
            self.bins.clone(),
        ] {
            fs::create_dir_all(&path).map_err(|err| {
                Error::message(format!("failed to create {}: {err}", path.display()))
            })?;
        }
        Ok(())
    }

    pub fn packages(&self) -> PathBuf {
        self.cache.join("packages")
    }

    pub fn runtime_cache(&self) -> PathBuf {
        self.cache.join("runtimes")
    }

    pub fn metadata(&self) -> PathBuf {
        self.cache.join("metadata")
    }

    pub fn indexes(&self) -> PathBuf {
        self.cache.join("indexes")
    }

    pub fn classmaps(&self) -> PathBuf {
        self.cache.join("classmaps")
    }

    pub fn runtimes(&self) -> PathBuf {
        self.data.join("runtimes")
    }

    pub fn tools(&self) -> PathBuf {
        self.data.join("tools")
    }

    pub fn php_pin(&self) -> PathBuf {
        self.data.join("php-pin")
    }

    pub fn refs(&self) -> PathBuf {
        self.data.join("refs.json")
    }

    pub fn read_pin(&self) -> Option<String> {
        fs::read_to_string(self.php_pin())
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    }

    pub fn write_pin(&self, spec: &str) -> Result<()> {
        self.ensure()?;
        fs::write(self.php_pin(), format!("{spec}\n"))
            .map_err(|err| Error::message(format!("failed to write php pin: {err}")))
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn is_safe_relative(path: &Path) -> bool {
    let mut depth = 0i32;
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
        }
    }
    true
}
