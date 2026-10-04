//! Non-destructive migration from Composer manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use puv_core::{
    Autoload, LOCK_VERSION, LockFile, LockedPackage, LockedRuntime, Manifest, PackageMeta,
    ProjectMeta, content_hash,
};
use puv_semver::{Constraint, Version};
use serde_json::Value;

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

pub struct Migration {
    pub manifest: Manifest,
    /// Copied from `composer.lock` when that file exists. `None` means the
    /// caller should resolve `manifest` itself.
    pub lock: Option<LockFile>,
    pub warnings: Vec<String>,
}

pub fn migrate(root: &Path, php_versions: &[String]) -> Result<Migration> {
    let composer_path = root.join("composer.json");
    let lock_path = root.join("composer.lock");
    let composer: Value = read_json(&composer_path)?;
    let locked = if lock_path.is_file() {
        Some(read_json(&lock_path)?)
    } else {
        None
    };
    let mut warnings = Vec::new();
    if locked.is_none() {
        warnings.push("no composer.lock; resolving dependencies from composer.json".to_string());
    }
    let name = composer
        .get("name")
        .and_then(Value::as_str)
        .map(|name| name.to_string())
        .unwrap_or_else(|| {
            root.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("project")
                .to_string()
        });
    let require = string_map(composer.get("require"));
    let require_dev = string_map(composer.get("require-dev"));
    let php_constraint = require.get("php").cloned();
    let php_line = php_minor_line(php_constraint.as_deref(), php_versions, &mut warnings)?;
    let exact = php_versions
        .iter()
        .filter(|version| minor_line(version) == php_line)
        .max_by(|left, right| cmp_version(left, right))
        .cloned()
        .unwrap_or_else(|| format!("{php_line}.0"));
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    for (name, constraint) in &require {
        if is_platform(name) {
            continue;
        }
        dependencies.insert(name.clone(), constraint.clone());
    }
    for (name, constraint) in &require_dev {
        if is_platform(name) {
            continue;
        }
        dev_dependencies.insert(name.clone(), constraint.clone());
    }
    let scripts = script_map(composer.get("scripts"), &mut warnings);
    let manifest = Manifest {
        project: ProjectMeta {
            name,
            php: php_line,
        },
        dependencies,
        dev_dependencies,
        tool_dependencies: BTreeMap::new(),
        scripts,
        package: PackageMeta {
            format: "phar".to_string(),
            entrypoint: Some("src/main.php".to_string()),
        },
    };
    let mut packages = Vec::new();
    let mut extensions = BTreeSet::new();
    if let Some(locked) = &locked {
        for key in ["packages", "packages-dev"] {
            if let Some(list) = locked.get(key).and_then(Value::as_array) {
                for item in list {
                    if let Some(package) = locked_package(item, &mut extensions) {
                        packages.push(package);
                    }
                }
            }
        }
    }
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages.dedup_by(|left, right| left.name == right.name);
    let hash = content_hash(&manifest);
    let lock = locked.map(|_| LockFile {
        lock_version: LOCK_VERSION,
        content_hash: hash,
        runtime: LockedRuntime {
            php: exact,
            extensions: extensions.into_iter().collect(),
        },
        packages,
        tools: Vec::new(),
    });
    Ok(Migration {
        manifest,
        lock,
        warnings,
    })
}

pub fn clean_composer_files(root: &Path) -> Result<()> {
    for name in ["composer.json", "composer.lock"] {
        let path = root.join(name);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|err| Error::new(format!("failed to remove {}: {err}", path.display())))?;
        }
    }
    let vendor = root.join("vendor");
    if vendor.exists() {
        fs::remove_dir_all(&vendor)
            .map_err(|err| Error::new(format!("failed to remove {}: {err}", vendor.display())))?;
    }
    Ok(())
}

fn locked_package(value: &Value, extensions: &mut BTreeSet<String>) -> Option<LockedPackage> {
    let name = puv_core::normalize_name(value.get("name")?.as_str()?);
    let version = value
        .get("version_normalized")
        .and_then(Value::as_str)
        .or_else(|| value.get("version").and_then(Value::as_str))
        .unwrap_or("");
    let Ok(parsed) = Version::parse(version) else {
        return None;
    };
    let dist = value.get("dist");
    let source = dist
        .and_then(|dist| dist.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let source_type = dist
        .and_then(|dist| dist.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("zip")
        .to_string();
    let shasum = dist
        .and_then(|dist| dist.get("shasum"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sum| !sum.is_empty())
        .map(str::to_string);
    let checksum = match shasum.as_deref().map(str::len) {
        Some(64) => format!("sha256:{}", shasum.as_deref().unwrap_or("")),
        Some(40) => format!("sha1:{}", shasum.as_deref().unwrap_or("")),
        _ => format!("source:{}", puv_core::sha256_hex(source.as_bytes())),
    };
    let mut dependencies = Vec::new();
    if let Some(require) = value.get("require").and_then(Value::as_object) {
        for name in require.keys() {
            let name = puv_core::normalize_name(name);
            if let Some(extension) = name.strip_prefix("ext-") {
                extensions.insert(extension.to_string());
            }
            if !is_platform(&name) {
                dependencies.push(name);
            }
        }
    }
    dependencies.sort();
    let autoload = autoload_from(value.get("autoload"));
    let bins = value
        .get("bin")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let provides = value
        .get("provide")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    Some(LockedPackage {
        name,
        version: parsed.to_string(),
        source,
        source_type,
        checksum,
        registry_checksum: shasum,
        dependencies,
        autoload,
        bins,
        provides,
        conflicts: Vec::new(),
    })
}

fn autoload_from(value: Option<&Value>) -> Autoload {
    let Some(value) = value else {
        return Autoload::default();
    };
    Autoload {
        psr4: prefix_map(value.get("psr-4")),
        psr0: prefix_map(value.get("psr-0")),
        classmap: string_list(value.get("classmap")),
        files: string_list(value.get("files")),
    }
}

fn prefix_map(value: Option<&Value>) -> BTreeMap<String, Vec<String>> {
    let Some(object) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    object
        .iter()
        .map(|(prefix, paths)| {
            let list = if let Some(path) = paths.as_str() {
                vec![path.to_string()]
            } else {
                string_list(Some(paths))
            };
            (prefix.clone(), list)
        })
        .collect()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn script_map(value: Option<&Value>, warnings: &mut Vec<String>) -> BTreeMap<String, String> {
    let Some(object) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    let mut scripts = BTreeMap::new();
    for (name, body) in object {
        match script_body(body) {
            Some(body) => {
                scripts.insert(name.clone(), body);
            }
            None => warnings.push(format!(
                "skipped composer script '{name}' because it is not a plain shell command"
            )),
        }
    }
    scripts
}

fn script_body(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if is_plain_script(text) => Some(text.clone()),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let text = item.as_str()?;
                if !is_plain_script(text) {
                    return None;
                }
                parts.push(text.to_string());
            }
            Some(parts.join(" && "))
        }
        _ => None,
    }
}

fn is_plain_script(text: &str) -> bool {
    !text.starts_with('@') && !text.contains("@composer") && !text.contains("@php")
}

fn php_minor_line(
    constraint: Option<&str>,
    versions: &[String],
    warnings: &mut Vec<String>,
) -> Result<String> {
    if let Some(version) = lowest_satisfying_minor(constraint, versions) {
        return Ok(version);
    }
    if constraint.is_some() {
        warnings.push(
            "no indexed PHP runtime matched composer.json; defaulting the project line to 8.4"
                .to_string(),
        );
    }
    Ok("8.4".to_string())
}

fn lowest_satisfying_minor(constraint: Option<&str>, versions: &[String]) -> Option<String> {
    let constraint = constraint?;
    let parsed = Constraint::parse(constraint).ok()?;
    let mut matches: Vec<_> = versions
        .iter()
        .filter_map(|version| {
            Version::parse(version)
                .ok()
                .map(|parsed_version| (version, parsed_version))
        })
        .filter(|(_, version)| parsed.matches(version))
        .collect();
    matches.sort_by(|left, right| left.1.cmp(&right.1));
    matches.first().map(|(version, _)| minor_line(version))
}

fn minor_line(version: &str) -> String {
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!("{major}.{minor}"),
        (Some(major), None) => major.to_string(),
        _ => version.to_string(),
    }
}

fn cmp_version(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |spec: &str| {
        Version::parse(spec)
            .map(|version| (version.major, version.minor, version.patch))
            .unwrap_or((0, 0, 0))
    };
    parse(left).cmp(&parse(right))
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    let Some(object) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    object
        .iter()
        .filter_map(|(key, item)| {
            item.as_str()
                .map(|constraint| (puv_core::normalize_name(key), constraint.to_string()))
        })
        .collect()
}

fn is_platform(name: &str) -> bool {
    name == "php"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name.starts_with("composer-")
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)
        .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|err| Error::new(format!("invalid {}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn migrates_a_locked_library_without_touching_composer_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("composer.json"),
            r#"{"name":"acme/demo","require":{"php":">=8.2","symfony/console":"^7.0"},"require-dev":{"phpunit/phpunit":"^11.0"},"scripts":{"test":"phpunit","weird":["@php -r 'echo 1;'", "phpunit"]}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("composer.lock"),
            r#"{"packages":[{"name":"symfony/console","version":"7.3.0","version_normalized":"7.3.0.0","dist":{"type":"zip","url":"https://example.test/console.zip","shasum":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"require":{"php":">=8.2","ext-mbstring":"*"},"autoload":{"psr-4":{"Symfony\\Component\\Console\\":"src/"}}}],"packages-dev":[{"name":"phpunit/phpunit","version":"11.2.0","dist":{"type":"zip","url":"https://example.test/phpunit.zip","shasum":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"bin":["phpunit"]}]}"#,
        )
        .unwrap();
        let before = fs::read(dir.path().join("composer.json")).unwrap();
        let migration = migrate(
            dir.path(),
            &["8.2.10".into(), "8.3.1".into(), "8.4.23".into()],
        )
        .unwrap();
        let lock = migration.lock.expect("composer.lock should be copied");
        assert_eq!(migration.manifest.project.php, "8.2");
        assert_eq!(lock.runtime.php, "8.2.10");
        assert_eq!(lock.packages.len(), 2);
        assert_eq!(lock.packages[0].name, "phpunit/phpunit");
        assert_eq!(lock.packages[1].version, "7.3.0");
        assert!(migration.manifest.scripts.contains_key("test"));
        assert!(!migration.manifest.scripts.contains_key("weird"));
        assert!(!migration.warnings.is_empty());
        assert_eq!(fs::read(dir.path().join("composer.json")).unwrap(), before);
    }

    #[test]
    fn migrates_a_project_that_has_no_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("composer.json"),
            r#"{"name":"acme/demo","require":{"php":">=8.3","symfony/console":"^7.0"}}"#,
        )
        .unwrap();
        let migration = migrate(dir.path(), &["8.3.1".into(), "8.4.23".into()]).unwrap();
        assert!(migration.lock.is_none());
        assert_eq!(
            migration
                .manifest
                .dependencies
                .get("symfony/console")
                .map(String::as_str),
            Some("^7.0")
        );
        assert_eq!(migration.manifest.project.php, "8.3");
        assert!(
            migration
                .warnings
                .iter()
                .any(|warning| warning.contains("no composer.lock"))
        );
        assert!(!dir.path().join("puv.toml").exists());
    }
}
