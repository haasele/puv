//! Content-addressed package cache.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use flate2::read::GzDecoder;
use puv_core::{Dirs, is_safe_relative, sha256_hex, sha256_prefixed};
use sha1::Sha1;
use sha2::{Digest, Sha256};

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
pub struct Fetched {
    pub dir: PathBuf,
    pub sha256: String,
}

pub struct PackageCache {
    dirs: Dirs,
    client: reqwest::blocking::Client,
    gate: Gate,
    index: Mutex<()>,
}

struct Gate {
    state: Mutex<usize>,
    cv: Condvar,
}

impl PackageCache {
    pub fn new(dirs: Dirs) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("puv/0.1.0")
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|err| Error::new(format!("failed to build http client: {err}")))?;
        Ok(Self {
            dirs,
            client,
            gate: Gate {
                state: Mutex::new(0),
                cv: Condvar::new(),
            },
            index: Mutex::new(()),
        })
    }

    pub fn lookup_url(&self, url: &str) -> Option<PathBuf> {
        let _guard = self.index.lock().ok()?;
        let map = read_index(&self.dirs.packages().join("index.json"))?;
        let hex = map.get(url)?;
        let dir = self.dirs.packages().join(hex);
        dir.join(".ok").is_file().then_some(dir)
    }

    pub fn fetch(&self, url: &str, kind: &str, registry_checksum: Option<&str>) -> Result<Fetched> {
        self.gate.enter();
        let result = self.fetch_inner(url, kind, registry_checksum);
        self.gate.leave();
        result
    }

    fn fetch_inner(
        &self,
        url: &str,
        kind: &str,
        registry_checksum: Option<&str>,
    ) -> Result<Fetched> {
        if let Some(dir) = self.lookup_url(url) {
            let stamp = fs::read_to_string(dir.join(".ok")).unwrap_or_default();
            let stamp = stamp.trim();
            if let Some(digest) = stamp.strip_prefix("sha256:") {
                return Ok(Fetched {
                    dir,
                    sha256: format!("sha256:{digest}"),
                });
            }
        }
        let bytes = read_url(&self.client, url)?;
        if let Some(expected) = registry_checksum {
            verify_registry_checksum(&bytes, expected)?;
        }
        let digest = sha256_hex(&bytes);
        let dir = self.dirs.packages().join(&digest);
        if !dir.join(".ok").is_file() {
            if dir.exists() {
                fs::remove_dir_all(&dir)
                    .map_err(|err| Error::new(format!("failed to reset cache entry: {err}")))?;
            }
            fs::create_dir_all(&dir)
                .map_err(|err| Error::new(format!("failed to create {}: {err}", dir.display())))?;
            extract(&bytes, kind, url, &dir)?;
            hoist_single_directory(&dir)?;
            fs::write(dir.join(".ok"), sha256_prefixed(&bytes))
                .map_err(|err| Error::new(format!("failed to mark cache entry: {err}")))?;
        }
        self.remember_url(url, &digest);
        Ok(Fetched {
            dir,
            sha256: format!("sha256:{digest}"),
        })
    }

    fn remember_url(&self, url: &str, digest: &str) {
        let Ok(_guard) = self.index.lock() else {
            return;
        };
        let path = self.dirs.packages().join("index.json");
        let mut map = read_index(&path).unwrap_or_default();
        map.insert(url.to_string(), digest.to_string());
        if let Ok(encoded) = serde_json::to_string(&map) {
            fs::write(path, encoded).ok();
        }
    }

    pub fn artifact_dir(&self, checksum: &str) -> Option<PathBuf> {
        let hex = checksum.strip_prefix("sha256:")?;
        let dir = self.dirs.packages().join(hex);
        dir.join(".ok").is_file().then_some(dir)
    }

    pub fn empty_artifact(&self, name: &str, version: &str) -> Result<Fetched> {
        let digest = sha256_hex(format!("empty:{name}:{version}").as_bytes());
        let dir = self.dirs.packages().join(&digest);
        fs::create_dir_all(&dir)
            .map_err(|err| Error::new(format!("failed to create empty package: {err}")))?;
        fs::write(dir.join(".ok"), "empty").ok();
        Ok(Fetched {
            dir,
            sha256: format!("sha256:{digest}"),
        })
    }
}

impl Gate {
    fn enter(&self) {
        let mut held = self.state.lock().expect("cache gate");
        while *held >= 8 {
            held = self.cv.wait(held).expect("cache gate");
        }
        *held += 1;
    }

    fn leave(&self) {
        let mut held = self.state.lock().expect("cache gate");
        *held = held.saturating_sub(1);
        self.cv.notify_one();
    }
}

fn read_index(path: &Path) -> Option<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn read_url(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|err| Error::new(format!("failed to read {path}: {err}")));
    }
    let response = client
        .get(url)
        .send()
        .map_err(|err| Error::new(format!("failed to download {url}: {err}")))?;
    if !response.status().is_success() {
        return Err(Error::new(format!(
            "download of {url} returned {}",
            response.status()
        )));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| Error::new(format!("failed to read {url}: {err}")))
}

fn verify_registry_checksum(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = if expected.len() == 40 {
        hex::encode(Sha1::digest(bytes))
    } else if expected.len() == 64 {
        hex::encode(Sha256::digest(bytes))
    } else {
        return Err(Error::new(format!(
            "unsupported registry checksum length {}",
            expected.len()
        )));
    };
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Error::new(format!(
            "registry checksum mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn extract(bytes: &[u8], kind: &str, url: &str, dest: &Path) -> Result<()> {
    let kind = if kind.is_empty() {
        if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
            "tar"
        } else {
            "zip"
        }
    } else {
        kind
    };
    match kind {
        "tar" | "tgz" | "tar.gz" => extract_tar(bytes, dest),
        _ => extract_zip(bytes, dest),
    }
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|err| Error::new(format!("invalid zip archive: {err}")))?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|err| Error::new(format!("invalid zip entry: {err}")))?;
        let name = file
            .enclosed_name()
            .ok_or_else(|| Error::new("zip entry escapes the package root"))?
            .to_path_buf();
        if !is_safe_relative(&name) {
            return Err(Error::new(format!(
                "zip entry {} escapes the package root",
                name.display()
            )));
        }
        let out = dest.join(&name);
        if file.is_dir() {
            fs::create_dir_all(&out).map_err(|err| Error::new(err.to_string()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::new(err.to_string()))?;
        }
        let mut output = File::create(&out)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", out.display())))?;
        std::io::copy(&mut file, &mut output)
            .map_err(|err| Error::new(format!("failed to unpack {}: {err}", out.display())))?;
    }
    Ok(())
}

fn extract_tar(bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .map_err(|err| Error::new(format!("invalid tar archive: {err}")))?
    {
        let mut entry = entry.map_err(|err| Error::new(format!("invalid tar entry: {err}")))?;
        let path = entry
            .path()
            .map_err(|err| Error::new(format!("invalid tar path: {err}")))?
            .into_owned();
        if !is_safe_relative(&path) {
            return Err(Error::new(format!(
                "tar entry {} escapes the package root",
                path.display()
            )));
        }
        let out = dest.join(&path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out).map_err(|err| Error::new(err.to_string()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::new(err.to_string()))?;
        }
        let mut output = File::create(&out)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", out.display())))?;
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|err| Error::new(format!("failed to read tar entry: {err}")))?;
        output
            .write_all(&data)
            .map_err(|err| Error::new(format!("failed to unpack {}: {err}", out.display())))?;
    }
    Ok(())
}

fn hoist_single_directory(dest: &Path) -> Result<()> {
    let entries: Vec<_> = fs::read_dir(dest)
        .map_err(|err| Error::new(err.to_string()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != ".ok")
        .collect();
    if entries.len() != 1 || !entries[0].path().is_dir() {
        return Ok(());
    }
    let nested = entries[0].path();
    let nested_entries: Vec<_> = fs::read_dir(&nested)
        .map_err(|err| Error::new(err.to_string()))?
        .filter_map(|entry| entry.ok())
        .collect();
    for entry in nested_entries {
        let target = dest.join(entry.file_name());
        fs::rename(entry.path(), &target)
            .map_err(|err| Error::new(format!("failed to hoist package root: {err}")))?;
    }
    fs::remove_dir_all(&nested).map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_registry_checksum_mismatches_and_extracts_zip() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            cache: dir.path().join("cache"),
            data: dir.path().join("data"),
            bins: dir.path().join("bin"),
        };
        dirs.ensure().unwrap();
        let cache = PackageCache::new(dirs).unwrap();
        let zip_path = dir.path().join("pkg.zip");
        write_zip(&zip_path);
        let bytes = fs::read(&zip_path).unwrap();
        let sum = hex::encode(Sha1::digest(&bytes));
        let fetched = cache
            .fetch(&format!("file://{}", zip_path.display()), "zip", Some(&sum))
            .unwrap();
        assert!(fetched.dir.join("composer.json").is_file());
        assert!(fetched.sha256.starts_with("sha256:"));
        let again = cache
            .fetch(&format!("file://{}", zip_path.display()), "zip", Some(&sum))
            .unwrap();
        assert_eq!(fetched.sha256, again.sha256);
    }

    fn write_zip(path: &Path) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer
            .start_file("pkg-1.0.0/composer.json", options)
            .unwrap();
        writer.write_all(br#"{"name":"acme/lib"}"#).unwrap();
        writer.start_file("pkg-1.0.0/src/Lib.php", options).unwrap();
        writer
            .write_all(b"<?php\nnamespace Acme;\nclass Lib {}\n")
            .unwrap();
        writer.finish().unwrap();
    }
}
