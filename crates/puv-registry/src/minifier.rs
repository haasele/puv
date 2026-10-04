use serde_json::{Map, Value};

use crate::{Error, Result};

/// Expand a Composer 2.0 minified version list.
///
/// Each version inherits keys from the previous one. Keys listed in `__unset`
/// are removed after that inheritance. Nested values are replaced wholesale.
pub fn expand(versions: &[Value]) -> Result<Vec<Value>> {
    let mut previous: Option<Map<String, Value>> = None;
    let mut expanded = Vec::with_capacity(versions.len());
    for version in versions {
        let current = version
            .as_object()
            .ok_or_else(|| Error::new("package version metadata is not an object"))?
            .clone();
        let mut merged = previous.clone().unwrap_or_default();
        for (key, value) in current {
            merged.insert(key, value);
        }
        if let Some(unset) = merged.remove("__unset")
            && let Some(keys) = unset.as_array()
        {
            for key in keys {
                if let Some(name) = key.as_str() {
                    merged.remove(name);
                }
            }
        }
        previous = Some(merged.clone());
        expanded.push(Value::Object(merged));
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inherits_missing_keys_and_applies_unset() {
        let versions = vec![
            json!({
                "name": "psr/log",
                "version": "3.0.2",
                "require": {"php": ">=8.0.0"},
                "autoload": {"psr-4": {"Psr\\Log\\": "src"}}
            }),
            json!({
                "version": "3.0.1"
            }),
            json!({
                "version": "3.0.0",
                "__unset": ["autoload"]
            }),
        ];
        let expanded = expand(&versions).unwrap();
        assert_eq!(expanded[1]["name"], "psr/log");
        assert_eq!(expanded[1]["autoload"]["psr-4"]["Psr\\Log\\"], "src");
        assert_eq!(expanded[1]["version"], "3.0.1");
        assert!(expanded[2].get("autoload").is_none());
        assert_eq!(expanded[2]["require"]["php"], ">=8.0.0");
        assert!(expanded[2].get("__unset").is_none());
    }
}
