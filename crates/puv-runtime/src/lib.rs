//! PHP runtime discovery and installation.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use puv_core::{Dirs, Error as CoreError, is_safe_relative, sha256_hex};
use regex::Regex;
use serde::{Deserialize, Serialize};

const INDEX_URL: &str = "https://dl.static-php.dev/static-php-cli/gnu-bulk/";
const CORE_EXTENSIONS: &[&str] = &[
    "core",
    "date",
    "filter",
    "hash",
    "json",
    "pcre",
    "random",
    "reflection",
    "spl",
    "standard",
];

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

impl From<CoreError> for Error {
    fn from(err: CoreError) -> Self {
        Self::new(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub version: String,
    pub target: String,
    pub url: String,
    pub sha256: Option<String>,
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhpRequest {
    pub major: u64,
    pub minor: Option<u64>,
    pub patch: Option<u64>,
}

impl PhpRequest {
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim().trim_start_matches('v').trim_start_matches('V');
        if spec.is_empty() {
            return Err(Error::new("empty PHP version"));
        }
        let mut parts = Vec::new();
        for part in spec.split('.') {
            if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()) {
                return Err(Error::new(format!("invalid PHP version '{spec}'")));
            }
            parts.push(
                part.parse::<u64>()
                    .map_err(|_| Error::new(format!("invalid PHP version '{spec}'")))?,
            );
        }
        if parts.len() > 3 {
            return Err(Error::new(format!("invalid PHP version '{spec}'")));
        }
        Ok(Self {
            major: parts[0],
            minor: parts.get(1).copied(),
            patch: parts.get(2).copied(),
        })
    }

    pub fn matches(&self, version: &str) -> bool {
        let Ok(parsed) = Self::parse(version) else {
            return false;
        };
        parsed.major == self.major
            && self.minor.is_none_or(|minor| parsed.minor == Some(minor))
            && self.patch.is_none_or(|patch| parsed.patch == Some(patch))
    }
}

pub fn current_target() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        other => other,
    }
}

pub fn bundled_extensions() -> Vec<String> {
    let mut names = vec![
        "bcmath",
        "bz2",
        "calendar",
        "ctype",
        "curl",
        "dba",
        "dom",
        "event",
        "exif",
        "fileinfo",
        "ftp",
        "gd",
        "gmp",
        "iconv",
        "imagick",
        "imap",
        "intl",
        "libxml",
        "mbregex",
        "mbstring",
        "mysqli",
        "mysqlnd",
        "opcache",
        "openssl",
        "pcntl",
        "pdo",
        "pdo_mysql",
        "pgsql",
        "phar",
        "posix",
        "readline",
        "redis",
        "session",
        "shmop",
        "simplexml",
        "soap",
        "sockets",
        "sodium",
        "sqlite3",
        "swoole",
        "sysvmsg",
        "sysvsem",
        "sysvshm",
        "tokenizer",
        "xml",
        "xmlreader",
        "xmlwriter",
        "xsl",
        "zip",
        "zlib",
    ];
    names.extend(CORE_EXTENSIONS.iter().copied());
    names.sort();
    names.dedup();
    names.into_iter().map(str::to_string).collect()
}

fn embedded() -> Vec<Artifact> {
    let extensions = bundled_extensions();
    ["x86_64", "aarch64"]
        .into_iter()
        .map(|arch| {
            let target = if arch == "x86_64" {
                "x86_64-unknown-linux-gnu"
            } else {
                "aarch64-unknown-linux-gnu"
            };
            let sha256 = if arch == "x86_64" {
                Some("bfa838bc924171480fd9b833d9b4241b6fd703a86a30501afa893752c5e79fd9".to_string())
            } else {
                None
            };
            Artifact {
                version: "8.4.23".to_string(),
                target: target.to_string(),
                url: format!("{INDEX_URL}php-8.4.23-cli-linux-{arch}.tar.gz"),
                sha256,
                extensions: extensions.clone(),
            }
        })
        .collect()
}

pub fn load_index(dirs: &Dirs) -> Result<Vec<Artifact>> {
    if let Ok(path) = std::env::var("PUV_RUNTIME_INDEX") {
        return read_index_file(Path::new(&path));
    }
    let mut artifacts = embedded();
    if let Ok(cached) = read_index_file(&index_cache(dirs)) {
        merge_artifacts(&mut artifacts, cached);
    }
    Ok(artifacts
        .into_iter()
        .filter(|artifact| artifact.target == current_target())
        .collect())
}

pub fn refresh_index(dirs: &Dirs) -> Result<Vec<Artifact>> {
    if std::env::var("PUV_RUNTIME_INDEX").is_ok() {
        return load_index(dirs);
    }
    let client = http_client()?;
    let response = client
        .get(INDEX_URL)
        .send()
        .map_err(|err| Error::new(format!("failed to refresh runtime index: {err}")))?;
    if !response.status().is_success() {
        return Err(Error::new(format!(
            "runtime index returned {}",
            response.status()
        )));
    }
    let html = response
        .text()
        .map_err(|err| Error::new(format!("failed to read runtime index: {err}")))?;
    let mut artifacts = parse_index_html(&html);
    if let Ok(existing) = load_index(dirs) {
        let pinned: Vec<_> = existing
            .into_iter()
            .filter_map(|artifact| artifact.sha256.map(|sum| (artifact.url, sum)))
            .collect();
        for artifact in &mut artifacts {
            if artifact.sha256.is_none()
                && let Some((_, sum)) = pinned.iter().find(|(url, _)| url == &artifact.url)
            {
                artifact.sha256 = Some(sum.clone());
            }
        }
    }
    dirs.ensure()?;
    fs::write(
        index_cache(dirs),
        serde_json::to_string_pretty(&artifacts)
            .map_err(|err| Error::new(format!("failed to encode runtime index: {err}")))?,
    )
    .map_err(|err| Error::new(format!("failed to write runtime index: {err}")))?;
    Ok(artifacts
        .into_iter()
        .filter(|artifact| artifact.target == current_target())
        .collect())
}

pub fn select<'a>(artifacts: &'a [Artifact], spec: &str) -> Result<&'a Artifact> {
    let request = PhpRequest::parse(spec)?;
    artifacts
        .iter()
        .filter(|artifact| {
            artifact.target == current_target() && request.matches(&artifact.version)
        })
        .max_by(|left, right| cmp_version(&left.version, &right.version))
        .ok_or_else(|| {
            Error::new(format!(
                "no PHP runtime matching {spec} for {}",
                current_target()
            ))
        })
}

pub fn install(dirs: &Dirs, artifact: &Artifact) -> Result<PathBuf> {
    dirs.ensure()?;
    let archive = archive_path(dirs, artifact);
    if !archive.is_file() {
        download_archive(artifact, &archive)?;
    } else if let Some(expected) = &artifact.sha256 {
        verify_file(&archive, expected)?;
    }
    let dest = dirs.runtimes().join(&artifact.version);
    let php = dest.join("php");
    if !php.is_file() {
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|err| Error::new(format!("failed to reset {}: {err}", dest.display())))?;
        }
        fs::create_dir_all(&dest)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", dest.display())))?;
        extract_archive(&archive, &dest)?;
        let found = find_php(&dest).ok_or_else(|| {
            Error::new(format!(
                "archive for PHP {} does not contain a php binary",
                artifact.version
            ))
        })?;
        if found != php {
            std::os::unix::fs::symlink(&found, &php)
                .map_err(|err| Error::new(format!("failed to link php binary: {err}")))?;
        }
        fs::write(dest.join("extensions.txt"), artifact.extensions.join("\n")).ok();
    }
    ensure_executable(&php)?;
    Ok(php)
}

pub fn installed_versions(dirs: &Dirs) -> Vec<String> {
    let mut versions = Vec::new();
    let Ok(entries) = fs::read_dir(dirs.runtimes()) else {
        return versions;
    };
    for entry in entries.flatten() {
        if entry.path().join("php").is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            versions.push(name.to_string());
        }
    }
    versions.sort_by(|left, right| cmp_version(left, right));
    versions
}

pub fn php_bin(dirs: &Dirs, version: &str) -> Option<PathBuf> {
    let path = dirs.runtimes().join(version).join("php");
    path.is_file().then_some(path)
}

pub fn remove(dirs: &Dirs, spec: &str) -> Result<String> {
    let installed = installed_versions(dirs);
    let request = PhpRequest::parse(spec)?;
    let version = installed
        .iter()
        .filter(|version| request.matches(version))
        .max_by(|left, right| cmp_version(left, right))
        .cloned()
        .ok_or_else(|| Error::new(format!("PHP {spec} is not installed")))?;
    let dest = dirs.runtimes().join(&version);
    fs::remove_dir_all(&dest)
        .map_err(|err| Error::new(format!("failed to remove {}: {err}", dest.display())))?;
    if let Ok(artifacts) = load_index(dirs)
        && let Some(artifact) = artifacts
            .iter()
            .find(|artifact| artifact.version == version)
    {
        let archive = archive_path(dirs, artifact);
        if archive.is_file() {
            fs::remove_file(archive).ok();
        }
    }
    Ok(version)
}

pub fn ensure_spec(dirs: &Dirs, spec: &str) -> Result<(Artifact, PathBuf)> {
    let artifacts = if std::env::var("PUV_RUNTIME_INDEX").is_ok() {
        load_index(dirs)?
    } else {
        match refresh_index(dirs) {
            Ok(artifacts) if !artifacts.is_empty() => artifacts,
            _ => load_index(dirs)?,
        }
    };
    let artifact = select(&artifacts, spec)?.clone();
    let bin = install(dirs, &artifact)?;
    Ok((artifact, bin))
}

fn archive_path(dirs: &Dirs, artifact: &Artifact) -> PathBuf {
    dirs.runtime_cache().join(format!(
        "php-{}-{}.tar.gz",
        artifact.version, artifact.target
    ))
}

fn index_cache(dirs: &Dirs) -> PathBuf {
    dirs.indexes().join("static-php.json")
}

fn read_index_file(path: &Path) -> Result<Vec<Artifact>> {
    let text = fs::read_to_string(path)
        .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|err| Error::new(format!("invalid runtime index {}: {err}", path.display())))
}

fn merge_artifacts(base: &mut Vec<Artifact>, extra: Vec<Artifact>) {
    for artifact in extra {
        if let Some(existing) = base
            .iter_mut()
            .find(|item| item.version == artifact.version && item.target == artifact.target)
        {
            if existing.sha256.is_none() {
                existing.sha256 = artifact.sha256.clone();
            }
            existing.url = artifact.url;
        } else {
            base.push(artifact);
        }
    }
}

pub fn parse_index_html(html: &str) -> Vec<Artifact> {
    let pattern = Regex::new(r"php-(\d+\.\d+\.\d+)-cli-linux-(x86_64|aarch64)\.tar\.gz").unwrap();
    let extensions = bundled_extensions();
    let mut artifacts = Vec::new();
    for found in pattern.captures_iter(html) {
        let version = found.get(1).unwrap().as_str();
        let arch = found.get(2).unwrap().as_str();
        let target = if arch == "x86_64" {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-unknown-linux-gnu"
        };
        let url = format!("{INDEX_URL}php-{version}-cli-linux-{arch}.tar.gz");
        if artifacts
            .iter()
            .any(|artifact: &Artifact| artifact.url == url)
        {
            continue;
        }
        artifacts.push(Artifact {
            version: version.to_string(),
            target: target.to_string(),
            url,
            sha256: None,
            extensions: extensions.clone(),
        });
    }
    artifacts
}

fn download_archive(artifact: &Artifact, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", parent.display())))?;
    }
    let bytes = if let Some(path) = artifact.url.strip_prefix("file://") {
        fs::read(path).map_err(|err| Error::new(format!("failed to read {path}: {err}")))?
    } else {
        let client = http_client()?;
        let response = client
            .get(&artifact.url)
            .send()
            .map_err(|err| Error::new(format!("failed to download {}: {err}", artifact.url)))?;
        if !response.status().is_success() {
            return Err(Error::new(format!(
                "download of {} returned {}",
                artifact.url,
                response.status()
            )));
        }
        response
            .bytes()
            .map_err(|err| Error::new(format!("failed to read {}: {err}", artifact.url)))?
            .to_vec()
    };
    let actual = sha256_hex(&bytes);
    if let Some(expected) = &artifact.sha256
        && !expected.eq_ignore_ascii_case(&actual)
    {
        return Err(Error::new(format!(
            "checksum mismatch for PHP {}: expected {expected}, got {actual}",
            artifact.version
        )));
    }
    let tmp = dest.with_extension("download");
    fs::write(&tmp, &bytes)
        .map_err(|err| Error::new(format!("failed to write {}: {err}", tmp.display())))?;
    fs::rename(&tmp, dest)
        .map_err(|err| Error::new(format!("failed to store runtime archive: {err}")))?;
    Ok(())
}

fn verify_file(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path)
        .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
    let actual = sha256_hex(&bytes);
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(Error::new(format!(
            "cached runtime {} failed checksum verification",
            path.display()
        )));
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let bytes = fs::read(archive)
        .map_err(|err| Error::new(format!("failed to read {}: {err}", archive.display())))?;
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut tar = tar::Archive::new(decoder);
    for entry in tar
        .entries()
        .map_err(|err| Error::new(format!("invalid runtime archive: {err}")))?
    {
        let mut entry =
            entry.map_err(|err| Error::new(format!("invalid runtime archive: {err}")))?;
        let path = entry
            .path()
            .map_err(|err| Error::new(format!("invalid archive path: {err}")))?
            .into_owned();
        if !is_safe_relative(&path) {
            return Err(Error::new(format!(
                "runtime archive contains an unsafe path {}",
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
        entry
            .unpack(&out)
            .map_err(|err| Error::new(format!("failed to unpack {}: {err}", out.display())))?;
    }
    Ok(())
}

fn find_php(root: &Path) -> Option<PathBuf> {
    for candidate in ["php", "bin/php"] {
        let path = root.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut found = None;
    let _ = visit(root, &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) == Some("php") && path.is_file() {
            found = Some(path.to_path_buf());
        }
    });
    found
}

fn visit(dir: &Path, visit_file: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit(&path, visit_file)?;
        } else {
            visit_file(&path);
        }
    }
    Ok(())
}

fn ensure_executable(path: &Path) -> Result<()> {
    let meta = fs::metadata(path)
        .map_err(|err| Error::new(format!("failed to stat {}: {err}", path.display())))?;
    let mut perms = meta.permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)
        .map_err(|err| Error::new(format!("failed to mark php executable: {err}")))?;
    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("puv/0.1.0")
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|err| Error::new(format!("failed to build http client: {err}")))
}

pub fn cmp_version(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |spec: &str| {
        PhpRequest::parse(spec)
            .ok()
            .map(|request| {
                (
                    request.major,
                    request.minor.unwrap_or(0),
                    request.patch.unwrap_or(0),
                )
            })
            .unwrap_or((0, 0, 0))
    };
    parse(left).cmp(&parse(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_highest_patch_on_the_minor_line() {
        let artifacts = vec![
            Artifact {
                version: "8.4.1".into(),
                target: current_target().into(),
                url: "file://a".into(),
                sha256: None,
                extensions: vec!["json".into()],
            },
            Artifact {
                version: "8.4.23".into(),
                target: current_target().into(),
                url: "file://b".into(),
                sha256: None,
                extensions: vec!["json".into()],
            },
            Artifact {
                version: "8.3.20".into(),
                target: current_target().into(),
                url: "file://c".into(),
                sha256: None,
                extensions: vec!["json".into()],
            },
        ];
        let selected = select(&artifacts, "8.4").unwrap();
        assert_eq!(selected.version, "8.4.23");
    }

    #[test]
    fn parses_directory_listing() {
        let html = r#"<a href="php-8.4.23-cli-linux-x86_64.tar.gz">php</a>"#;
        let artifacts = parse_index_html(html);
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.version == "8.4.23")
        );
    }
}
