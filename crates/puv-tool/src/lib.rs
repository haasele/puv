//! Global tool shims and cache pruning.

use std::fs;
use std::path::{Path, PathBuf};

use puv_core::Dirs;
use puv_env::{referenced_checksums, sh_quote};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("{message}")]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
}

pub fn link_tool_bins(bin_dir: &Path, tool_bin: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(bin_dir).map_err(|err| Error::new(err.to_string()))?;
    let mut linked = Vec::new();
    let entries = fs::read_dir(tool_bin).map_err(|err| Error::new(err.to_string()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == "php" || !entry.path().is_file() {
            continue;
        }
        let dest = bin_dir.join(name);
        let body = format!(
            "#!/bin/sh\nexec {} \"$@\"\n",
            sh_quote(&entry.path().display().to_string())
        );
        fs::write(&dest, body).map_err(|err| Error::new(err.to_string()))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dest)
            .map_err(|err| Error::new(err.to_string()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms).map_err(|err| Error::new(err.to_string()))?;
        linked.push(name.to_string());
    }
    linked.sort();
    Ok(linked)
}

pub fn unlink_bins(bin_dir: &Path, names: &[String]) {
    for name in names {
        fs::remove_file(bin_dir.join(name)).ok();
    }
}

pub fn prune(dirs: &Dirs, all: bool) -> Result<PruneReport> {
    dirs.ensure().map_err(|err| Error::new(err.to_string()))?;
    if all {
        let mut removed = 0;
        for dir in [
            dirs.packages(),
            dirs.runtime_cache(),
            dirs.metadata(),
            dirs.indexes(),
            dirs.classmaps(),
        ] {
            if dir.exists() {
                removed += count_entries(&dir);
                fs::remove_dir_all(&dir).map_err(|err| Error::new(err.to_string()))?;
            }
            fs::create_dir_all(&dir).map_err(|err| Error::new(err.to_string()))?;
        }
        return Ok(PruneReport { removed });
    }
    let referenced = referenced_checksums(dirs);
    let mut removed = 0;
    if let Ok(entries) = fs::read_dir(dirs.packages()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == "index.json" || referenced.contains(name) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|err| Error::new(err.to_string()))?;
                removed += 1;
            }
        }
    }
    let installed = installed_runtime_versions(dirs);
    if let Ok(entries) = fs::read_dir(dirs.runtime_cache()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let referenced_runtime = installed.iter().any(|version| name.contains(version));
            if referenced_runtime {
                continue;
            }
            fs::remove_file(entry.path()).ok();
            removed += 1;
        }
    }
    Ok(PruneReport { removed })
}

fn installed_runtime_versions(dirs: &Dirs) -> Vec<String> {
    let mut versions = Vec::new();
    if let Ok(entries) = fs::read_dir(dirs.runtimes()) {
        for entry in entries.flatten() {
            if entry.path().join("php").is_file()
                && let Some(name) = entry.file_name().to_str()
            {
                versions.push(name.to_string());
            }
        }
    }
    versions
}

fn count_entries(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.count())
        .unwrap_or(0)
}

pub fn read_shim_list(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn write_shim_list(path: &Path, names: &[String]) -> Result<()> {
    fs::write(path, names.join("\n") + "\n").map_err(|err| Error::new(err.to_string()))
}

pub fn tool_home(dirs: &Dirs, package: &str) -> PathBuf {
    dirs.tools().join(puv_core::tool_dir_name(package))
}
