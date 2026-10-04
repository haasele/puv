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
    let dest = dirs.runtimes().join(&artifact.version);
    let php = dest.join("php");
    if php.is_file() {
        ensure_executable(&php)?;
        return Ok(php);
    }
    let archive = archive_path(dirs, artifact);
    if !archive.is_file() {
        download_archive(artifact, &archive)?;
    } else if let Some(expected) = &artifact.sha256 {
        verify_file(&archive, expected)?;
    }
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
            puv_core::link_path(&found, &php)
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

pub fn matching_artifacts<'a>(
    artifacts: &'a [Artifact],
    spec: Option<&str>,
) -> Result<Vec<&'a Artifact>> {
    let request = spec.map(PhpRequest::parse).transpose()?;
    let mut matched: Vec<_> = artifacts
        .iter()
        .filter(|artifact| {
            artifact.target == current_target()
                && request
                    .as_ref()
                    .is_none_or(|request| request.matches(&artifact.version))
        })
        .collect();
    matched.sort_by(|left, right| cmp_version(&left.version, &right.version));
    matched.dedup_by(|left, right| left.version == right.version);
    if matched.is_empty() {
        return Err(Error::new(match spec {
            Some(spec) => format!("no PHP runtime matching {spec} for {}", current_target()),
            None => format!("no PHP runtime for {}", current_target()),
        }));
    }
    Ok(matched)
}

pub fn remove(dirs: &Dirs, spec: Option<&str>) -> Result<Vec<String>> {
    let request = spec.map(PhpRequest::parse).transpose()?;
    let mut versions: Vec<String> = installed_versions(dirs)
        .into_iter()
        .filter(|version| {
            request
                .as_ref()
                .is_none_or(|request| request.matches(version))
        })
        .collect();
    if versions.is_empty() {
        return Err(Error::new(match spec {
            Some(spec) => format!("PHP {spec} is not installed"),
            None => "no PHP runtime is installed".to_string(),
        }));
    }
    versions.sort_by(|left, right| cmp_version(left, right));
    let artifacts = load_index(dirs).unwrap_or_default();
    for version in &versions {
        let dest = dirs.runtimes().join(version);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|err| Error::new(format!("failed to remove {}: {err}", dest.display())))?;
        }
        if let Some(artifact) = artifacts
            .iter()
            .find(|artifact| artifact.version == *version && artifact.target == current_target())
        {
            let archive = archive_path(dirs, artifact);
            if archive.is_file() {
                fs::remove_file(archive).ok();
            }
        }
    }
    Ok(versions)
}

pub fn ensure_spec(dirs: &Dirs, spec: &str) -> Result<(Artifact, PathBuf)> {
    if std::env::var("PUV_RUNTIME_INDEX").is_err()
        && index_is_fresh(dirs)
        && let Ok(artifacts) = load_index(dirs)
        && let Ok(artifact) = select(&artifacts, spec)
    {
        let php = dirs.runtimes().join(&artifact.version).join("php");
        if php.is_file() {
            return Ok((artifact.clone(), php));
        }
    }
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

fn index_is_fresh(dirs: &Dirs) -> bool {
    let Ok(meta) = fs::metadata(index_cache(dirs)) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|age| age < std::time::Duration::from_secs(24 * 60 * 60))
        .unwrap_or(false)
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
        let mut progress = puv_core::Progress::new(format!("php {}", artifact.version));
        progress.begin();
        let mut response = client
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
        if let Some(total) = response_length(&response) {
            progress.set_total(total);
        }
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read = std::io::Read::read(&mut response, &mut buffer)
                .map_err(|err| Error::new(format!("failed to read {}: {err}", artifact.url)))?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            progress.advance(read as u64);
        }
        progress.finish();
        bytes
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

fn response_length(response: &reqwest::blocking::Response) -> Option<u64> {
    let encoded = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"));
    if encoded {
        return None;
    }
    response.content_length()
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
    puv_core::make_executable(path)
        .map_err(|err| Error::new(format!("failed to mark php executable: {err}")))
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

    fn artifact(version: &str) -> Artifact {
        Artifact {
            version: version.into(),
            target: current_target().into(),
            url: format!("file://{version}"),
            sha256: None,
            extensions: vec!["json".into()],
        }
    }

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
        let exact = select(&artifacts, "8.4.1").unwrap();
        assert_eq!(exact.version, "8.4.1");
    }

    #[test]
    fn remove_drops_one_patch_or_the_whole_minor_line() {
        let root = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            cache: root.path().join("cache"),
            data: root.path().join("data"),
            bins: root.path().join("bin"),
        };
        dirs.ensure().unwrap();
        for version in ["8.2.1", "8.2.32", "8.3.1"] {
            let dir = dirs.runtimes().join(version);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("php"), b"php").unwrap();
        }
        assert_eq!(
            remove(&dirs, Some("8.2.1")).unwrap(),
            vec!["8.2.1".to_string()]
        );
        assert!(!dirs.runtimes().join("8.2.1").exists());
        assert!(dirs.runtimes().join("8.2.32/php").is_file());
        fs::create_dir_all(dirs.runtimes().join("8.2.1")).unwrap();
        fs::write(dirs.runtimes().join("8.2.1/php"), b"php").unwrap();
        assert_eq!(
            remove(&dirs, Some("8.2")).unwrap(),
            vec!["8.2.1".to_string(), "8.2.32".to_string()]
        );
        assert!(!dirs.runtimes().join("8.2.1").exists());
        assert!(!dirs.runtimes().join("8.2.32").exists());
        assert!(dirs.runtimes().join("8.3.1/php").is_file());
        assert_eq!(remove(&dirs, None).unwrap(), vec!["8.3.1".to_string()]);
        assert!(remove(&dirs, None).is_err());
    }

    #[test]
    fn matching_artifacts_expand_a_minor_line() {
        let artifacts = [artifact("8.4.1"), artifact("8.4.23"), artifact("8.3.20")];
        let minor: Vec<_> = matching_artifacts(&artifacts, Some("8.4"))
            .unwrap()
            .into_iter()
            .map(|item| item.version.as_str())
            .collect();
        assert_eq!(minor, vec!["8.4.1", "8.4.23"]);
        let exact: Vec<_> = matching_artifacts(&artifacts, Some("8.4.1"))
            .unwrap()
            .into_iter()
            .map(|item| item.version.as_str())
            .collect();
        assert_eq!(exact, vec!["8.4.1"]);
        let all = matching_artifacts(&artifacts, None).unwrap();
        assert_eq!(all.len(), 3);
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
