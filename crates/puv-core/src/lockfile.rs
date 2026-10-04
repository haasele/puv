use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Autoload, Error, Result, toml_basic_string, toml_string_array, write_atomic};

pub const LOCK_VERSION: u32 = 1;
const LOCKB_MAGIC: &[u8] = b"PUVB";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(rename = "lock-version")]
    pub lock_version: u32,
    #[serde(rename = "content-hash")]
    pub content_hash: String,
    pub runtime: LockedRuntime,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
    #[serde(default, rename = "tool")]
    pub tools: Vec<LockedTool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedRuntime {
    pub php: String,
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub source: String,
    #[serde(rename = "source-type", default)]
    pub source_type: String,
    pub checksum: String,
    #[serde(rename = "registry-checksum", default)]
    pub registry_checksum: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub autoload: Autoload,
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedTool {
    pub name: String,
    pub version: String,
    pub php: String,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockedPackage>,
}

impl LockFile {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .map_err(|err| Error::message(format!("failed to read {}: {err}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|err| Error::message(format!("invalid lockfile: {err}")))
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("lock-version = {}\n", self.lock_version));
        out.push_str(&format!(
            "content-hash = {}\n\n",
            toml_basic_string(&self.content_hash)
        ));
        out.push_str("[runtime]\n");
        out.push_str(&format!("php = {}\n", toml_basic_string(&self.runtime.php)));
        out.push_str("extensions = ");
        out.push_str(&toml_string_array(&self.runtime.extensions));
        out.push('\n');
        for package in &self.packages {
            render_package(&mut out, "package", package);
        }
        for tool in &self.tools {
            out.push_str("\n[[tool]]\n");
            out.push_str(&format!("name = {}\n", toml_basic_string(&tool.name)));
            out.push_str(&format!("version = {}\n", toml_basic_string(&tool.version)));
            out.push_str(&format!("php = {}\n", toml_basic_string(&tool.php)));
            for package in &tool.packages {
                render_package(&mut out, "tool.package", package);
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        write_atomic(path, self.render())
    }

    pub fn checksums(&self) -> Vec<String> {
        let mut sums = Vec::new();
        for package in self
            .packages
            .iter()
            .chain(self.tools.iter().flat_map(|tool| tool.packages.iter()))
        {
            if let Some(hex) = package.checksum.strip_prefix("sha256:") {
                sums.push(hex.to_string());
            }
        }
        sums.sort();
        sums.dedup();
        sums
    }
}

fn render_package(out: &mut String, header: &str, package: &LockedPackage) {
    out.push_str(&format!("\n[[{header}]]\n"));
    out.push_str(&format!("name = {}\n", toml_basic_string(&package.name)));
    out.push_str(&format!(
        "version = {}\n",
        toml_basic_string(&package.version)
    ));
    if !package.source.is_empty() {
        out.push_str(&format!(
            "source = {}\n",
            toml_basic_string(&package.source)
        ));
    }
    if !package.source_type.is_empty() {
        out.push_str(&format!(
            "source-type = {}\n",
            toml_basic_string(&package.source_type)
        ));
    }
    out.push_str(&format!(
        "checksum = {}\n",
        toml_basic_string(&package.checksum)
    ));
    if let Some(registry) = &package.registry_checksum {
        out.push_str(&format!(
            "registry-checksum = {}\n",
            toml_basic_string(registry)
        ));
    }
    if !package.dependencies.is_empty() {
        out.push_str("dependencies = ");
        out.push_str(&toml_string_array(&package.dependencies));
        out.push('\n');
    }
    if !package.bins.is_empty() {
        out.push_str("bins = ");
        out.push_str(&toml_string_array(&package.bins));
        out.push('\n');
    }
    if !package.provides.is_empty() {
        out.push_str("provides = ");
        out.push_str(&toml_string_array(&package.provides));
        out.push('\n');
    }
    if !package.conflicts.is_empty() {
        out.push_str("conflicts = ");
        out.push_str(&toml_string_array(&package.conflicts));
        out.push('\n');
    }
    render_autoload(out, header, &package.autoload);
}

fn render_autoload(out: &mut String, header: &str, autoload: &Autoload) {
    if autoload.is_empty() {
        return;
    }
    if !autoload.files.is_empty() || !autoload.classmap.is_empty() {
        out.push_str(&format!("\n[{header}.autoload]\n"));
        if !autoload.files.is_empty() {
            out.push_str("files = ");
            out.push_str(&toml_string_array(&autoload.files));
            out.push('\n');
        }
        if !autoload.classmap.is_empty() {
            out.push_str("classmap = ");
            out.push_str(&toml_string_array(&autoload.classmap));
            out.push('\n');
        }
    }
    render_prefix_map(out, &format!("{header}.autoload.psr-4"), &autoload.psr4);
    render_prefix_map(out, &format!("{header}.autoload.psr-0"), &autoload.psr0);
}

fn render_prefix_map(
    out: &mut String,
    header: &str,
    map: &std::collections::BTreeMap<String, Vec<String>>,
) {
    if map.is_empty() {
        return;
    }
    out.push_str(&format!("\n[{header}]\n"));
    for (prefix, paths) in map {
        out.push_str(&toml_basic_string(prefix));
        out.push_str(" = ");
        out.push_str(&toml_string_array(paths));
        out.push('\n');
    }
}

pub fn write_lockb(path: &Path, lock_bytes: &[u8], lock: &LockFile) -> Result<()> {
    let hash = Sha256::digest(lock_bytes);
    let payload = serde_json::to_vec(lock)
        .map_err(|err| Error::message(format!("failed to encode lock cache: {err}")))?;
    let mut bytes = Vec::with_capacity(44 + payload.len());
    bytes.extend_from_slice(LOCKB_MAGIC);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&hash);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&payload);
    write_atomic(path, bytes)
}

/// Returns the cached lock when it still matches `lock_bytes`.
pub fn load_lockb(path: &Path, lock_bytes: &[u8]) -> Option<LockFile> {
    let data = fs::read(path).ok()?;
    if data.len() < 44 || &data[0..4] != LOCKB_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
    if version != 1 {
        return None;
    }
    let expected = Sha256::digest(lock_bytes);
    if data[8..40] != expected[..] {
        return None;
    }
    let len = u32::from_le_bytes(data[40..44].try_into().ok()?) as usize;
    let payload = data.get(44..44 + len)?;
    serde_json::from_slice(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Autoload;

    #[test]
    fn lock_roundtrip_is_stable() {
        let lock = LockFile {
            lock_version: 1,
            content_hash: "sha256:abc".to_string(),
            runtime: LockedRuntime {
                php: "8.4.23".to_string(),
                extensions: vec!["ctype".to_string(), "json".to_string()],
            },
            packages: vec![LockedPackage {
                name: "symfony/console".to_string(),
                version: "7.3.0".to_string(),
                source: "https://example.test/console.zip".to_string(),
                source_type: "zip".to_string(),
                checksum: "sha256:dead".to_string(),
                registry_checksum: Some("sha1:beef".to_string()),
                dependencies: vec!["psr/log".to_string()],
                autoload: Autoload {
                    psr4: [(
                        "Symfony\\Component\\Console\\".to_string(),
                        vec!["src/".to_string()],
                    )]
                    .into_iter()
                    .collect(),
                    ..Autoload::default()
                },
                bins: vec!["bin/console".to_string()],
                provides: Vec::new(),
                conflicts: Vec::new(),
            }],
            tools: vec![LockedTool {
                name: "phpstan/phpstan".to_string(),
                version: "2.1.0".to_string(),
                php: "8.4.23".to_string(),
                packages: vec![LockedPackage {
                    name: "phpstan/phpstan".to_string(),
                    version: "2.1.0".to_string(),
                    source: "https://example.test/phpstan.zip".to_string(),
                    source_type: "zip".to_string(),
                    checksum: "sha256:cafe".to_string(),
                    registry_checksum: None,
                    dependencies: Vec::new(),
                    autoload: Autoload::default(),
                    bins: vec!["phpstan".to_string()],
                    provides: Vec::new(),
                    conflicts: Vec::new(),
                }],
            }],
        };
        let rendered = lock.render();
        let parsed = LockFile::parse(&rendered).unwrap();
        assert_eq!(parsed, lock);
        assert_eq!(parsed.render(), rendered);
    }
}
