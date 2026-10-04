//! Project environment materialization: autoload, shims, and dependency links.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use puv_cache::PackageCache;
use puv_core::{Autoload, Dirs, LockFile, LockedPackage, LockedTool, Manifest, tool_dir_name};
use regex::Regex;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvState {
    pub php: String,
    pub php_bin: PathBuf,
    pub content_hash: String,
}

pub struct SyncRequest<'a> {
    pub root: &'a Path,
    pub manifest: &'a Manifest,
    pub lock: &'a LockFile,
    pub dirs: &'a Dirs,
    pub php_bin: &'a Path,
    pub cache: &'a PackageCache,
}

pub fn sync(request: &SyncRequest<'_>) -> Result<()> {
    if puv_core::content_hash(request.manifest) != request.lock.content_hash {
        return Err(Error::new(
            "lockfile is out of date with puv.toml; run `puv lock`",
        ));
    }
    let env = request.root.join(".puv");
    let deps = env.join("deps");
    let bin = env.join("bin");
    reset_dir(&deps)?;
    reset_dir(&bin)?;
    fs::create_dir_all(&env).map_err(|err| Error::new(err.to_string()))?;
    let packages = link_packages(request.cache, &deps, &request.lock.packages)?;
    let autoload = env.join("autoload.php");
    write_autoload(&autoload, &packages)?;
    fs::write(
        deps.join("autoload.php"),
        "<?php\nrequire dirname(__DIR__) . '/autoload.php';\n",
    )
    .map_err(|err| Error::new(err.to_string()))?;
    write_bins(&bin, request.php_bin, &autoload, &packages)?;
    write_php_shim(&bin.join("php"), request.php_bin, &autoload)?;
    sync_tools(request, &env)?;
    let state = EnvState {
        php: request.lock.runtime.php.clone(),
        php_bin: request.php_bin.to_path_buf(),
        content_hash: request.lock.content_hash.clone(),
    };
    fs::write(
        env.join("env.json"),
        serde_json::to_string_pretty(&state).map_err(|err| Error::new(err.to_string()))?,
    )
    .map_err(|err| Error::new(err.to_string()))?;
    record_project(request.dirs, request.root, &request.lock.checksums())?;
    Ok(())
}

pub fn env_is_current(root: &Path, lock: &LockFile) -> bool {
    let Ok(text) = fs::read_to_string(root.join(".puv/env.json")) else {
        return false;
    };
    let Ok(state) = serde_json::from_str::<EnvState>(&text) else {
        return false;
    };
    state.content_hash == lock.content_hash
        && state.php == lock.runtime.php
        && state.php_bin.is_file()
        && root.join(".puv/autoload.php").is_file()
}

pub fn project_tool_bin(root: &Path, name: &str) -> Option<PathBuf> {
    let tools = root.join(".puv/tools");
    let entries = fs::read_dir(tools).ok()?;
    for entry in entries.flatten() {
        let bin = entry.path().join("bin").join(name);
        if bin.is_file() {
            return Some(bin);
        }
    }
    None
}

fn sync_tools(request: &SyncRequest<'_>, env: &Path) -> Result<()> {
    let tools_root = env.join("tools");
    if tools_root.exists() {
        fs::remove_dir_all(&tools_root).map_err(|err| Error::new(err.to_string()))?;
    }
    for tool in &request.lock.tools {
        materialize_tool(request, &tools_root, tool)?;
    }
    Ok(())
}

fn materialize_tool(request: &SyncRequest<'_>, tools_root: &Path, tool: &LockedTool) -> Result<()> {
    let dir = tools_root.join(tool_dir_name(&tool.name));
    let deps = dir.join("deps");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).map_err(|err| Error::new(err.to_string()))?;
    let packages = link_packages(request.cache, &deps, &tool.packages)?;
    let autoload = dir.join("autoload.php");
    write_autoload(&autoload, &packages)?;
    fs::write(
        deps.join("autoload.php"),
        "<?php\nrequire dirname(__DIR__) . '/autoload.php';\n",
    )
    .map_err(|err| Error::new(err.to_string()))?;
    write_bins(&bin, request.php_bin, &autoload, &packages)?;
    write_php_shim(&bin.join("php"), request.php_bin, &autoload)?;
    Ok(())
}

fn link_packages<'a>(
    cache: &PackageCache,
    deps: &Path,
    packages: &'a [LockedPackage],
) -> Result<Vec<(&'a LockedPackage, PathBuf)>> {
    fs::create_dir_all(deps).map_err(|err| Error::new(err.to_string()))?;
    let mut linked = Vec::new();
    for package in packages {
        let dir = ensure_artifact(cache, package)?;
        link_package(deps, &package.name, &dir)?;
        linked.push((package, dir));
    }
    Ok(linked)
}

fn ensure_artifact(cache: &PackageCache, package: &LockedPackage) -> Result<PathBuf> {
    if let Some(dir) = cache.artifact_dir(&package.checksum) {
        return Ok(dir);
    }
    if let Some(dir) = cache.lookup_url(&package.source) {
        return Ok(dir);
    }
    if package.source.is_empty() {
        let fetched = cache
            .empty_artifact(&package.name, &package.version)
            .map_err(|err| Error::new(err.to_string()))?;
        if fetched.sha256 != package.checksum {
            return Err(Error::new(format!(
                "empty artifact for {} did not match the lock checksum",
                package.name
            )));
        }
        return Ok(fetched.dir);
    }
    let fetched = cache
        .fetch_named(
            &package.source,
            &package.source_type,
            package.registry_checksum.as_deref(),
            &format!("{} {}", package.name, package.version),
        )
        .map_err(|err| Error::new(err.to_string()))?;
    if package.checksum.starts_with("sha256:") && fetched.sha256 != package.checksum {
        return Err(Error::new(format!(
            "checksum for {} changed: lock has {}, download is {}",
            package.name, package.checksum, fetched.sha256
        )));
    }
    Ok(fetched.dir)
}

fn link_package(deps: &Path, name: &str, target: &Path) -> Result<()> {
    let dest = deps.join(name);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|err| Error::new(err.to_string()))?;
    }
    if dest.symlink_metadata().is_ok() {
        if dest.is_dir() && !dest.is_symlink() {
            fs::remove_dir_all(&dest).map_err(|err| Error::new(err.to_string()))?;
        } else {
            fs::remove_file(&dest).map_err(|err| Error::new(err.to_string()))?;
        }
    }
    puv_core::link_path(target, &dest).map_err(|err| {
        Error::new(format!(
            "failed to link {} -> {}: {err}",
            dest.display(),
            target.display()
        ))
    })
}

fn write_autoload(path: &Path, packages: &[(&LockedPackage, PathBuf)]) -> Result<()> {
    let mut psr4: Vec<(String, String)> = Vec::new();
    let mut psr0: Vec<(String, String)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut classmap: BTreeMap<String, String> = BTreeMap::new();
    for (package, dir) in packages {
        let root = format!("$deps . '/{}'", package.name);
        collect_prefixes(&package.autoload, &root, &mut psr4, &mut psr0);
        for file in &package.autoload.files {
            files.push(format!("{root} . '/{}'", trim_slashes(file)));
        }
        if !package.autoload.classmap.is_empty() {
            let scanned = scan_classmap(dir, &package.autoload.classmap);
            for (class, relative) in scanned {
                classmap.insert(class, format!("{root} . '/{}'", trim_slashes(&relative)));
            }
        }
    }
    psr4.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    psr0.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    let mut php = String::from(
        "<?php\nif (defined('PUV_AUTOLOAD')) {\n    return;\n}\ndefine('PUV_AUTOLOAD', true);\n$deps = __DIR__ . '/deps';\n",
    );
    php.push_str("$psr4 = array(\n");
    for (prefix, dir) in &psr4 {
        php.push_str(&format!("    array({}, {}),\n", php_quote(prefix), dir));
    }
    php.push_str(");\n$psr0 = array(\n");
    for (prefix, dir) in &psr0 {
        php.push_str(&format!("    array({}, {}),\n", php_quote(prefix), dir));
    }
    php.push_str(");\n$classmap = array(\n");
    for (class, file) in &classmap {
        php.push_str(&format!("    {} => {},\n", php_quote(class), file));
    }
    php.push_str(
        r#");
spl_autoload_register(static function (string $class) use ($psr4, $psr0, $classmap): void {
    foreach ($psr4 as $entry) {
        $prefix = $entry[0];
        $dir = $entry[1];
        if ($prefix !== '' && !str_starts_with($class, $prefix)) {
            continue;
        }
        $relative = str_replace('\\', '/', substr($class, strlen($prefix)));
        $path = $dir . '/' . $relative . '.php';
        if (is_file($path)) {
            require $path;
            return;
        }
    }
    foreach ($psr0 as $entry) {
        $prefix = $entry[0];
        $dir = $entry[1];
        if ($prefix !== '' && !str_starts_with($class, $prefix)) {
            continue;
        }
        $relative = substr($class, strlen($prefix));
        $relative = str_replace(array('\\', '_'), '/', $relative);
        $path = $dir . '/' . $relative . '.php';
        if (is_file($path)) {
            require $path;
            return;
        }
    }
    if (isset($classmap[$class])) {
        require $classmap[$class];
    }
});
"#,
    );
    for file in files {
        php.push_str(&format!("require_once {file};\n"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| Error::new(err.to_string()))?;
    }
    fs::write(path, php).map_err(|err| Error::new(err.to_string()))
}

fn collect_prefixes(
    autoload: &Autoload,
    root: &str,
    psr4: &mut Vec<(String, String)>,
    psr0: &mut Vec<(String, String)>,
) {
    for (prefix, paths) in &autoload.psr4 {
        for path in paths {
            psr4.push((
                prefix.clone(),
                format!("{root} . '/{}'", trim_slashes(path)),
            ));
        }
    }
    for (prefix, paths) in &autoload.psr0 {
        for path in paths {
            psr0.push((
                prefix.clone(),
                format!("{root} . '/{}'", trim_slashes(path)),
            ));
        }
    }
}

fn write_bins(
    bin_dir: &Path,
    php: &Path,
    autoload: &Path,
    packages: &[(&LockedPackage, PathBuf)],
) -> Result<()> {
    fs::create_dir_all(bin_dir).map_err(|err| Error::new(err.to_string()))?;
    let mut seen = BTreeSet::new();
    for (package, dir) in packages {
        for bin in &package.bins {
            let name = Path::new(bin)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(bin);
            if !seen.insert(name.to_string()) || name == "php" {
                continue;
            }
            let script = dir.join(bin);
            write_exec_shim(&bin_dir.join(name), php, autoload, &script)?;
        }
    }
    Ok(())
}

pub fn write_php_shim(path: &Path, php: &Path, autoload: &Path) -> Result<()> {
    let body = format!(
        "#!/bin/sh\nprepend=1\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    -v|--version|-m|--modules|-i|--info|-h|--help) prepend=0 ;;\n  esac\ndone\nif [ \"$prepend\" -eq 1 ]; then\n  exec {} -d auto_prepend_file={} \"$@\"\nfi\nexec {} \"$@\"\n",
        sh_quote(&php.display().to_string()),
        sh_quote(&autoload.display().to_string()),
        sh_quote(&php.display().to_string()),
    );
    write_script(path, &body)
}

pub fn write_exec_shim(path: &Path, php: &Path, autoload: &Path, script: &Path) -> Result<()> {
    let body = format!(
        "#!/bin/sh\nexec {} -d auto_prepend_file={} {} \"$@\"\n",
        sh_quote(&php.display().to_string()),
        sh_quote(&autoload.display().to_string()),
        sh_quote(&script.display().to_string()),
    );
    write_script(path, &body)
}

fn write_script(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| Error::new(err.to_string()))?;
    }
    fs::write(path, body).map_err(|err| Error::new(err.to_string()))?;
    puv_core::make_executable(path).map_err(|err| Error::new(err.to_string()))
}

fn scan_classmap(root: &Path, paths: &[String]) -> Vec<(String, String)> {
    static CLASS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(?:(?:abstract|final|readonly)\s+)*\s*(?:class|interface|trait|enum)\s+([A-Za-z_][A-Za-z0-9_]*)")
            .expect("class regex")
    });
    static NAMESPACE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*namespace\s+([A-Za-z_\\][A-Za-z0-9_\\]*)\s*[;{]")
            .expect("namespace regex")
    });
    let mut found = Vec::new();
    for rel in paths {
        let start = root.join(trim_slashes(rel));
        let mut files = Vec::new();
        collect_php(&start, &mut files);
        for file in files {
            let Ok(source) = fs::read_to_string(&file) else {
                continue;
            };
            let namespace = NAMESPACE
                .captures(&source)
                .and_then(|caps| caps.get(1))
                .map(|item| item.as_str().to_string())
                .unwrap_or_default();
            let Ok(relative) = file.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            for caps in CLASS.captures_iter(&source) {
                let name = caps.get(1).unwrap().as_str();
                let class = if namespace.is_empty() {
                    name.to_string()
                } else {
                    format!("{namespace}\\{name}")
                };
                found.push((class, relative.clone()));
            }
        }
    }
    found
}

fn collect_php(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("php") {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect_php(&entry.path(), out);
    }
}

fn reset_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).map_err(|err| Error::new(err.to_string()))?;
    }
    fs::create_dir_all(path).map_err(|err| Error::new(err.to_string()))
}

fn trim_slashes(path: &str) -> &str {
    path.trim_matches('/')
}

fn php_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Refs {
    projects: BTreeMap<String, Vec<String>>,
    tools: BTreeMap<String, Vec<String>>,
}

pub fn record_project(dirs: &Dirs, root: &Path, checksums: &[String]) -> Result<()> {
    let mut refs = read_refs(dirs);
    refs.projects
        .insert(root.display().to_string(), checksums.to_vec());
    write_refs(dirs, &refs)
}

pub fn record_tool(dirs: &Dirs, tool_root: &Path, checksums: &[String]) -> Result<()> {
    let mut refs = read_refs(dirs);
    refs.tools
        .insert(tool_root.display().to_string(), checksums.to_vec());
    write_refs(dirs, &refs)
}

pub fn referenced_checksums(dirs: &Dirs) -> BTreeSet<String> {
    let refs = read_refs(dirs);
    let mut sums = BTreeSet::new();
    for (path, listed) in refs.projects.iter().chain(refs.tools.iter()) {
        let lock_path = Path::new(path).join("puv.lock");
        if lock_path.is_file()
            && let Ok(lock) = LockFile::read(&lock_path)
        {
            sums.extend(lock.checksums());
            continue;
        }
        if Path::new(path).exists() {
            sums.extend(listed.iter().cloned());
        }
    }
    if dirs.tools().is_dir()
        && let Ok(entries) = fs::read_dir(dirs.tools())
    {
        for entry in entries.flatten() {
            let lock_path = entry.path().join("puv.lock");
            if let Ok(lock) = LockFile::read(&lock_path) {
                sums.extend(lock.checksums());
            }
        }
    }
    sums
}

fn read_refs(dirs: &Dirs) -> Refs {
    fs::read_to_string(dirs.refs())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_refs(dirs: &Dirs, refs: &Refs) -> Result<()> {
    dirs.ensure().map_err(|err| Error::new(err.to_string()))?;
    fs::write(
        dirs.refs(),
        serde_json::to_string_pretty(refs).map_err(|err| Error::new(err.to_string()))?,
    )
    .map_err(|err| Error::new(err.to_string()))
}
