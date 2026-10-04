//! Fast syntax and project checks.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use puv_core::{LockFile, Manifest, Project, content_hash};
use rayon::prelude::*;
use regex::Regex;
use serde::Serialize;
use tree_sitter::Parser;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: String,
    pub rule: String,
    pub file: String,
    pub line: usize,
    pub message: String,
}

pub fn check(project: &Project) -> Result<Vec<Diagnostic>> {
    let manifest = puv_core::read_manifest(&project.manifest_path())
        .map_err(|err| Error::new(err.to_string()))?;
    let php = parse_minor(&manifest.project.php);
    let mut diagnostics = Vec::new();
    diagnostics.extend(project_diagnostics(project, &manifest)?);
    let files = php_files(&project.root);
    let syntax: Vec<_> = files
        .par_iter()
        .flat_map(|path| file_diagnostics(path, &project.root, php))
        .collect();
    diagnostics.extend(syntax);
    diagnostics.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.rule.cmp(&right.rule))
    });
    Ok(diagnostics)
}

pub fn file_count(project: &Project) -> usize {
    php_files(&project.root).len()
}

pub fn render_text(diagnostics: &[Diagnostic]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics {
        out.push_str(&format!(
            "{}:{}\n  {}[{}]\n  {}\n\n",
            diagnostic.file,
            diagnostic.line,
            diagnostic.severity,
            diagnostic.rule,
            diagnostic.message
        ));
    }
    out
}

pub fn render_json(diagnostics: &[Diagnostic]) -> Result<String> {
    serde_json::to_string_pretty(&serde_json::json!({ "diagnostics": diagnostics }))
        .map_err(|err| Error::new(err.to_string()))
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
}

fn project_diagnostics(project: &Project, manifest: &Manifest) -> Result<Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let has_deps = !manifest.dependencies.is_empty()
        || !manifest.dev_dependencies.is_empty()
        || !manifest.tool_dependencies.is_empty();
    let lock = if project.lock_path().is_file() {
        match LockFile::read(&project.lock_path()) {
            Ok(lock) => Some(lock),
            Err(err) => {
                diagnostics.push(error(
                    "puv.lock",
                    1,
                    "project.lock-outdated",
                    err.to_string(),
                ));
                None
            }
        }
    } else if has_deps {
        diagnostics.push(error(
            "puv.toml",
            1,
            "project.lock-missing",
            "dependencies are declared but puv.lock is missing",
        ));
        None
    } else {
        None
    };
    if let Some(lock) = &lock {
        if lock.content_hash != content_hash(manifest) {
            diagnostics.push(error(
                "puv.lock",
                1,
                "project.lock-outdated",
                "lockfile does not match puv.toml",
            ));
        }
        let dirs = puv_core::Dirs::from_env();
        let php = dirs.runtimes().join(&lock.runtime.php).join("php");
        if !php.is_file() {
            diagnostics.push(error(
                "puv.lock",
                1,
                "project.runtime-missing",
                format!("PHP {} is not installed", lock.runtime.php),
            ));
        }
        let mut required = Vec::new();
        for package in lock
            .packages
            .iter()
            .chain(lock.tools.iter().flat_map(|tool| tool.packages.iter()))
        {
            for dependency in &package.dependencies {
                if let Some(extension) = dependency.strip_prefix("ext-") {
                    required.push(extension.to_string());
                }
            }
        }
        required.sort();
        required.dedup();
        for extension in required {
            let present = lock
                .runtime
                .extensions
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&extension));
            if !present {
                diagnostics.push(error(
                    "puv.lock",
                    1,
                    "project.extension-missing",
                    format!(
                        "lock requires ext-{extension}, which the runtime index does not provide"
                    ),
                ));
            }
        }
    }
    if let Some(entrypoint) = &manifest.package.entrypoint
        && !project.root.join(entrypoint).is_file()
    {
        diagnostics.push(error(
            "puv.toml",
            1,
            "project.entrypoint-missing",
            format!("entrypoint {entrypoint} does not exist"),
        ));
    }
    for (name, script) in &manifest.scripts {
        for token in script.split_whitespace() {
            if token.ends_with(".php") && !project.root.join(token).is_file() {
                diagnostics.push(error(
                    "puv.toml",
                    1,
                    "project.script-missing",
                    format!("script '{name}' references missing file {token}"),
                ));
            }
        }
    }
    Ok(diagnostics)
}

fn file_diagnostics(path: &Path, root: &Path, php: (u64, u64)) -> Vec<Diagnostic> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let mut diagnostics = Vec::new();
    if bytes.contains(&0) {
        diagnostics.push(error(
            &relative,
            1,
            "syntax.nul-byte",
            "PHP file contains a NUL byte",
        ));
    }
    let source = String::from_utf8_lossy(&bytes);
    if let Some(line) = first_syntax_error(&source) {
        diagnostics.push(error(&relative, line, "syntax.error", "PHP syntax error"));
    }
    if php >= (8, 0) {
        flag_pattern(
            &source,
            &relative,
            r"\beach\s*\(",
            "deprecated.each",
            "each() was removed in PHP 8.0",
            &mut diagnostics,
        );
        flag_pattern(
            &source,
            &relative,
            r"\bcreate_function\s*\(",
            "deprecated.create-function",
            "create_function() was removed in PHP 8.0",
            &mut diagnostics,
        );
    }
    if php >= (8, 2) {
        flag_pattern(
            &source,
            &relative,
            r"\butf8_encode\s*\(",
            "deprecated.utf8-encode",
            "utf8_encode() is deprecated in PHP 8.2",
            &mut diagnostics,
        );
        flag_pattern(
            &source,
            &relative,
            r"\butf8_decode\s*\(",
            "deprecated.utf8-encode",
            "utf8_decode() is deprecated in PHP 8.2",
            &mut diagnostics,
        );
        flag_pattern(
            &source,
            &relative,
            r#"\$\{[A-Za-z_]"#,
            "deprecated.dollar-curly",
            "${var} string interpolation is deprecated in PHP 8.2",
            &mut diagnostics,
        );
    }
    if php >= (8, 4) {
        flag_implicit_nullable(&source, &relative, &mut diagnostics);
    }
    flag_pattern(
        &source,
        &relative,
        r"#\[\s*\]",
        "invalid.empty-attribute",
        "empty attribute",
        &mut diagnostics,
    );
    diagnostics
}

fn first_syntax_error(source: &str) -> Option<usize> {
    let mut parser = Parser::new();
    let language = tree_sitter_php::LANGUAGE_PHP;
    parser.set_language(&language.into()).ok()?;
    let tree = parser.parse(source, None)?;
    if !tree.root_node().has_error() {
        return None;
    }
    Some(error_line(tree.root_node()).unwrap_or(1))
}

fn error_line(node: tree_sitter::Node<'_>) -> Option<usize> {
    if node.is_error() || node.is_missing() {
        return Some(node.start_position().row + 1);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(line) = error_line(child) {
            return Some(line);
        }
    }
    None
}

fn flag_pattern(
    source: &str,
    file: &str,
    pattern: &str,
    rule: &str,
    message: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(regex) = Regex::new(pattern) else {
        return;
    };
    if let Some(found) = regex.find(source) {
        let line = source[..found.start()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        diagnostics.push(warning(file, line, rule, message));
    }
}

fn flag_implicit_nullable(source: &str, file: &str, diagnostics: &mut Vec<Diagnostic>) {
    static FN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)function\s+[A-Za-z_][A-Za-z0-9_]*\s*\(([^)]*)\)").expect("function regex")
    });
    static PARAM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<type>[A-Za-z_\\][A-Za-z0-9_\\]*)\s+\$[A-Za-z_][A-Za-z0-9_]*\s*=\s*null\b")
            .expect("param regex")
    });
    for found in FN.captures_iter(source) {
        let params = found.get(1).unwrap().as_str();
        if params.contains('?') || params.contains('|') {
            continue;
        }
        if PARAM.is_match(params) {
            let start = found.get(0).map(|item| item.start()).unwrap_or(0);
            let line = source[..start]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            diagnostics.push(warning(
                file,
                line,
                "deprecated.implicit-nullable",
                "implicit nullable parameters are deprecated in PHP 8.4",
            ));
        }
    }
}

fn php_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk(root, &mut files);
    files
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if path.is_dir() {
            if matches!(
                name,
                ".git" | ".puv" | "vendor" | "dist" | "node_modules" | "target"
            ) {
                continue;
            }
            walk(&path, files);
        } else if name.ends_with(".php") {
            files.push(path);
        }
    }
}

fn parse_minor(spec: &str) -> (u64, u64) {
    let mut parts = spec.trim().split('.');
    let major = parts.next().and_then(|part| part.parse().ok()).unwrap_or(8);
    let minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (major, minor)
}

fn error(file: &str, line: usize, rule: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        severity: "error".to_string(),
        rule: rule.to_string(),
        file: file.to_string(),
        line,
        message: message.into(),
    }
}

fn warning(file: &str, line: usize, rule: &str, message: &str) -> Diagnostic {
    Diagnostic {
        severity: "warning".to_string(),
        rule: rule.to_string(),
        file: file.to_string(),
        line,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_flags_broken_class() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .unwrap();
        let source = "<?php\nclass {\n";
        let tree = parser.parse(source, None).unwrap();
        assert!(
            tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        assert_eq!(first_syntax_error(source), Some(2));
    }
}
