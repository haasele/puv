//! Project model, paths, manifest, and lockfile.

mod error;
mod hash;
mod lockfile;
mod manifest;
mod paths;
mod progress;
mod project;
mod spinner;
mod style;

pub use error::{Error, Result};
pub use hash::{content_hash, sha256_hex, sha256_prefixed};
pub use lockfile::{
    LOCK_VERSION, LockFile, LockedPackage, LockedRuntime, LockedTool, load_lockb, write_lockb,
};
pub use manifest::project_name_from_dir;
pub use manifest::{
    Manifest, PackageMeta, ProjectMeta, init_template, main_php, read_manifest, remove_dependency,
    set_php, upsert_dependency,
};
pub use paths::{Dirs, is_safe_relative};
pub use progress::Progress;
pub use project::Project;
pub use spinner::Spinner;
pub use style::{paint, stderr_is_tty, stdout_is_tty};

use std::fmt::Write as _;

/// `vendor/package`, lowercased.
pub fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageSpec {
    pub name: String,
    pub constraint: Option<String>,
}

pub fn parse_spec(input: &str) -> Result<PackageSpec> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::message("package name is empty"));
    }
    let (name, constraint) = if let Some((name, constraint)) = input.split_once('@') {
        (name, Some(constraint))
    } else if let Some((name, constraint)) = input.split_once(':') {
        (name, Some(constraint))
    } else {
        (input, None)
    };
    let name = normalize_name(name);
    if !name.contains('/') || name.starts_with('/') || name.ends_with('/') {
        return Err(Error::message(format!(
            "'{input}' is not a vendor/package name"
        )));
    }
    let constraint = constraint
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    if constraint.as_deref() == Some("") {
        return Err(Error::message(format!("missing constraint in '{input}'")));
    }
    Ok(PackageSpec { name, constraint })
}

pub fn tool_dir_name(package: &str) -> String {
    normalize_name(package).replace('/', "-")
}

pub fn toml_basic_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub fn toml_string_array(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("[\n");
    for item in items {
        let _ = writeln!(out, "    {},", toml_basic_string(item));
    }
    out.push(']');
    out
}

pub fn write_atomic(path: &std::path::Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            Error::message(format!("failed to create {}: {err}", parent.display()))
        })?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes.as_ref())
        .map_err(|err| Error::message(format!("failed to write {}: {err}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|err| Error::message(format!("failed to replace {}: {err}", path.display())))?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Autoload {
    #[serde(default, rename = "psr-4")]
    pub psr4: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "psr-0")]
    pub psr0: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub classmap: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}

impl Autoload {
    pub fn is_empty(&self) -> bool {
        self.psr4.is_empty()
            && self.psr0.is_empty()
            && self.classmap.is_empty()
            && self.files.is_empty()
    }
}
