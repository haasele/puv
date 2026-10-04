use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};

use crate::{Error, Result, write_atomic};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub project: ProjectMeta,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "dev-dependencies")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "tool-dependencies")]
    pub tool_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    #[serde(default)]
    pub package: PackageMeta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub name: String,
    pub php: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMeta {
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub entrypoint: Option<String>,
}

impl Default for PackageMeta {
    fn default() -> Self {
        Self {
            format: default_format(),
            entrypoint: None,
        }
    }
}

fn default_format() -> String {
    "phar".to_string()
}

pub fn read_manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path)
        .map_err(|err| Error::message(format!("failed to read {}: {err}", path.display())))?;
    toml::from_str(&text)
        .map_err(|err| Error::message(format!("invalid {}: {err}", path.display())))
}

pub fn init_template(name: &str, php: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
php = "{php}"

[dependencies]

[dev-dependencies]

[tool-dependencies]

[scripts]

[package]
format = "phar"
entrypoint = "src/main.php"
"#
    )
}

pub fn main_php(name: &str) -> String {
    format!(
        r#"<?php

declare(strict_types=1);

fwrite(STDOUT, "Hello from {name}\n");
"#
    )
}

pub fn upsert_dependency(path: &Path, table: &str, name: &str, constraint: &str) -> Result<()> {
    let mut doc = read_document(path)?;
    if !doc.get(table).is_some_and(Item::is_table) {
        doc[table] = Item::Table(Table::new());
    }
    doc[table][name] = toml_edit::value(constraint);
    write_document(path, &doc)
}

pub fn remove_dependency(path: &Path, name: &str) -> Result<bool> {
    let mut doc = read_document(path)?;
    let mut removed = false;
    for table in ["dependencies", "dev-dependencies", "tool-dependencies"] {
        if let Some(item) = doc.get_mut(table).and_then(Item::as_table_mut)
            && item.remove(name).is_some()
        {
            removed = true;
        }
    }
    if removed {
        write_document(path, &doc)?;
    }
    Ok(removed)
}

pub fn set_php(path: &Path, php: &str) -> Result<()> {
    let mut doc = read_document(path)?;
    doc["project"]["php"] = toml_edit::value(php);
    write_document(path, &doc)
}

fn read_document(path: &Path) -> Result<DocumentMut> {
    let text = fs::read_to_string(path)
        .map_err(|err| Error::message(format!("failed to read {}: {err}", path.display())))?;
    text.parse::<DocumentMut>()
        .map_err(|err| Error::message(format!("invalid {}: {err}", path.display())))
}

fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    write_atomic(path, doc.to_string())
}

pub fn project_name_from_dir(path: &Path) -> String {
    let raw = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let mut name: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        name = "project".to_string();
    }
    name
}
