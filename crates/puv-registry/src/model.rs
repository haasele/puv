use std::collections::BTreeMap;

use puv_core::Autoload;
use puv_semver::Version;
use serde_json::Value;

use crate::minifier::expand;
use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dist {
    pub url: String,
    pub kind: String,
    pub shasum: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRelease {
    pub name: String,
    pub version: Version,
    pub dependencies: BTreeMap<String, String>,
    pub provides: BTreeMap<String, String>,
    pub replaces: BTreeMap<String, String>,
    pub conflicts: BTreeMap<String, String>,
    pub autoload: Autoload,
    pub bins: Vec<String>,
    pub dist: Option<Dist>,
}

pub fn releases_from_v2(body: &Value) -> Result<Vec<PackageRelease>> {
    let packages = body
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::new("metadata is missing packages"))?;
    let mut releases = Vec::new();
    for (name, versions) in packages {
        let Some(list) = versions.as_array() else {
            continue;
        };
        let expanded = expand(list)?;
        for version in expanded {
            if let Some(release) = release_from_value(name, &version)? {
                releases.push(release);
            }
        }
    }
    Ok(releases)
}

fn release_from_value(fallback_name: &str, value: &Value) -> Result<Option<PackageRelease>> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(fallback_name);
    let name = puv_core::normalize_name(name);
    let raw = value
        .get("version_normalized")
        .and_then(Value::as_str)
        .or_else(|| value.get("version").and_then(Value::as_str))
        .unwrap_or("");
    let Ok(version) = Version::parse(raw) else {
        return Ok(None);
    };
    Ok(Some(PackageRelease {
        name,
        version,
        dependencies: string_map(value.get("require")),
        provides: string_map(value.get("provide")),
        replaces: string_map(value.get("replace")),
        conflicts: string_map(value.get("conflict")),
        autoload: parse_autoload(value.get("autoload")),
        bins: string_list(value.get("bin")),
        dist: parse_dist(value.get("dist")),
    }))
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

fn parse_dist(value: Option<&Value>) -> Option<Dist> {
    let dist = value?;
    let url = dist.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return None;
    }
    let shasum = dist
        .get("shasum")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sum| !sum.is_empty())
        .map(str::to_string);
    Some(Dist {
        url: url.to_string(),
        kind: dist
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("zip")
            .to_string(),
        shasum,
    })
}

fn parse_autoload(value: Option<&Value>) -> Autoload {
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
        .map(|(prefix, paths)| (prefix.clone(), path_list(paths)))
        .collect()
}

fn path_list(value: &Value) -> Vec<String> {
    if let Some(path) = value.as_str() {
        vec![path.to_string()]
    } else {
        string_list(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expands_real_shaped_metadata() {
        let body = json!({
            "minified": "composer/2.0",
            "packages": {
                "psr/log": [
                    {
                        "name": "psr/log",
                        "version": "3.0.2",
                        "version_normalized": "3.0.2.0",
                        "require": {"php": ">=8.0.0"},
                        "autoload": {"psr-4": {"Psr\\Log\\": "src"}},
                        "dist": {
                            "type": "zip",
                            "url": "https://example.test/psr-log.zip",
                            "shasum": ""
                        }
                    },
                    {"version": "3.0.1", "version_normalized": "3.0.1.0"}
                ]
            }
        });
        let releases = releases_from_v2(&body).unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[1].name, "psr/log");
        assert_eq!(releases[1].dependencies["php"], ">=8.0.0");
        assert_eq!(
            releases[1].autoload.psr4["Psr\\Log\\"],
            vec!["src".to_string()]
        );
        assert!(releases[0].dist.as_ref().unwrap().shasum.is_none());
    }
}
