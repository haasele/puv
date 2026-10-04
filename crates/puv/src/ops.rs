use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use miette::Report;
use puv_build::BuildStep;
use puv_cache::PackageCache;
use puv_core::{
    Dirs, LOCK_VERSION, LockFile, LockedPackage, LockedRuntime, LockedTool, Manifest, Project,
    content_hash, parse_spec, project_name_from_dir, read_manifest, remove_dependency, set_php,
    upsert_dependency, write_atomic,
};
use puv_env::{SyncRequest, env_is_current, project_tool_bin, record_tool, sync, write_php_shim};
use puv_registry::{HttpRegistry, MetadataProvider, PackageRelease};
use puv_resolver::{SolveRequest, solve};
use puv_runtime::{self, Artifact};
use puv_semver::Version;

use crate::cli::{Cli, Command as CliCommand, PhpCommand, ToolCommand};

type Result<T> = std::result::Result<T, Report>;

pub fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        CliCommand::Create {
            template,
            directory,
        } => create(template, directory),
        CliCommand::Init { path, force } => init(path, force),
        CliCommand::Add { package, dev } => add(&package, dev),
        CliCommand::Install { package, dev } => match package {
            Some(package) => add(&package, dev),
            None => sync_project(),
        },
        CliCommand::Remove { package } => remove(&package),
        CliCommand::Sync => sync_project(),
        CliCommand::Lock { upgrade, package } => {
            lock_only(upgrade, package.as_deref())?;
            Ok(0)
        }
        CliCommand::Run { php, args } => execute(args, php, cli.verbose, false),
        CliCommand::Use { version, global } => use_php(&version, global),
        CliCommand::Load { package, dev: _ } => load(&package),
        CliCommand::Tool { command } => tool(command),
        CliCommand::Build => build(),
        CliCommand::Package { format } => pack(format),
        CliCommand::List => list_packages(),
        CliCommand::Why { package } => why_package(&package),
        CliCommand::Info { package } => info_package(&package),
        CliCommand::Audit => audit_packages(),
        CliCommand::Check { format } => check(format.as_deref()),
        CliCommand::Contain { clean } => contain(clean),
        CliCommand::Prune { global, all } => prune(global, all),
        CliCommand::Php { command } => php(command),
    }
}

pub fn execute_code(code: &str, verbose: u8) -> Result<i32> {
    let cwd = std::env::current_dir().map_err(err)?;
    let project = Project::discover_optional(&cwd);
    let (php, autoload, workdir) = prepare_runtime(project.as_ref(), None, verbose)?;
    let mut command = Command::new(&php);
    if let Some(autoload) = &autoload {
        command
            .arg("-d")
            .arg(format!("auto_prepend_file={}", autoload.display()));
    }
    command.arg("-r").arg(code);
    if let Some(dir) = workdir {
        command.current_dir(dir);
    }
    apply_env(&mut command, project.as_ref(), &php);
    run_status(&mut command)
}

pub fn execute(
    args: Vec<String>,
    php_override: Option<String>,
    verbose: u8,
    build_pipeline: bool,
) -> Result<i32> {
    if args.is_empty() {
        return Err(Report::msg("missing command"));
    }
    let cwd = std::env::current_dir().map_err(err)?;
    let project = Project::discover_optional(&cwd);
    let (php, autoload, workdir) =
        prepare_runtime(project.as_ref(), php_override.as_deref(), verbose)?;
    let manifest = project
        .as_ref()
        .map(|project| read_manifest(&project.manifest_path()))
        .transpose()
        .map_err(err)?;
    let head = &args[0];
    let rest = &args[1..];
    if let Some(path) = as_php_file(project.as_ref(), head) {
        let mut command = Command::new(&php);
        if let Some(autoload) = &autoload {
            command
                .arg("-d")
                .arg(format!("auto_prepend_file={}", autoload.display()));
        }
        command.arg(&path).args(rest);
        if let Some(dir) = &workdir {
            command.current_dir(dir);
        }
        apply_env(&mut command, project.as_ref(), &php);
        return run_status(&mut command);
    }
    if let Some(script) = manifest
        .as_ref()
        .and_then(|manifest| manifest.scripts.get(head))
    {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{script} \"$@\""))
            .arg("puv")
            .args(rest);
        if let Some(dir) = &workdir {
            command.current_dir(dir);
        }
        apply_env(&mut command, project.as_ref(), &php);
        if build_pipeline {
            command.env("PUV_BUILD", "1");
        }
        return run_status(&mut command);
    }
    if let Some(project) = &project
        && let Some(bin) = project_tool_bin(&project.root, head).or_else(|| {
            let local = project.root.join(".puv/bin").join(head);
            local.is_file().then_some(local)
        })
    {
        let mut command = Command::new(&bin);
        command.args(rest);
        if let Some(dir) = &workdir {
            command.current_dir(dir);
        }
        apply_env(&mut command, Some(project), &php);
        return run_status(&mut command);
    }
    let mut command = Command::new(head);
    command.args(rest);
    if let Some(dir) = &workdir {
        command.current_dir(dir);
    }
    apply_env(&mut command, project.as_ref(), &php);
    run_status(&mut command)
}

const TEMPLATES: &[(&str, &str, &str)] = &[
    ("laravel", "laravel/laravel", "Laravel application"),
    (
        "codeigniter",
        "codeigniter4/appstarter",
        "CodeIgniter 4 application",
    ),
    ("symfony", "symfony/skeleton", "Symfony skeleton"),
    ("slim", "slim/slim-skeleton", "Slim application"),
];

fn create(template: Option<String>, directory: Option<PathBuf>) -> Result<i32> {
    let name = match template {
        Some(name) => name,
        None => prompt_template()?,
    };
    let template = TEMPLATES
        .iter()
        .find(|template| template.0.eq_ignore_ascii_case(&name))
        .copied()
        .ok_or_else(|| {
            Report::msg(format!(
                "unknown template '{name}'. Choose {}",
                TEMPLATES
                    .iter()
                    .map(|template| template.0)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let cwd = std::env::current_dir().map_err(err)?;
    let directory = match directory {
        Some(path) => {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        }
        None if std::io::stdin().is_terminal() => prompt_directory(template.0, &cwd)?,
        None => cwd.join(template.0),
    };
    if directory.exists() && fs::read_dir(&directory).map_err(err)?.next().is_some() {
        return Err(Report::msg(format!("{} is not empty", directory.display())));
    }
    fs::create_dir_all(&directory).map_err(err)?;
    let dirs = Dirs::from_env();
    let registry = registry(&dirs)?;
    let releases = registry.releases(template.1).map_err(err)?;
    let release = releases
        .iter()
        .filter(|release| release.version.is_stable())
        .max_by(|left, right| left.version.cmp(&right.version))
        .ok_or_else(|| Report::msg(format!("no stable release for {}", template.1)))?;
    let dist = release
        .dist
        .as_ref()
        .ok_or_else(|| Report::msg(format!("{} has no dist archive", template.1)))?;
    let cache = PackageCache::new(dirs.clone()).map_err(err)?;
    let fetched = cache
        .fetch_named(
            &dist.url,
            &dist.kind,
            dist.shasum.as_deref(),
            &format!("{} {}", template.1, release.version),
        )
        .map_err(err)?;
    copy_tree(&fetched.dir, &directory)?;
    if directory.join("composer.json").is_file() {
        contain_at(&directory, false)?;
    }
    println!(
        "created {} from {} {}",
        directory.display(),
        template.1,
        release.version
    );
    Ok(0)
}

fn prompt_template() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        return Err(Report::msg(
            "pass a template name, for example `puv create laravel`",
        ));
    }
    eprintln!("Templates");
    for (index, template) in TEMPLATES.iter().enumerate() {
        eprintln!(
            "  {}  {:<14} {}  {}",
            index + 1,
            template.0,
            template.1,
            template.2
        );
    }
    eprint!("template: ");
    let _ = std::io::stderr().flush();
    let line = read_line()?;
    if let Ok(choice) = line.parse::<usize>() {
        return TEMPLATES
            .get(choice.wrapping_sub(1))
            .map(|template| template.0.to_string())
            .ok_or_else(|| Report::msg(format!("unknown template '{line}'")));
    }
    if TEMPLATES
        .iter()
        .any(|template| template.0.eq_ignore_ascii_case(&line))
    {
        return Ok(line);
    }
    Err(Report::msg(format!("unknown template '{line}'")))
}

fn prompt_directory(template: &str, cwd: &Path) -> Result<PathBuf> {
    eprint!("directory [{template}]: ");
    let _ = std::io::stderr().flush();
    let line = read_line()?;
    let name = if line.is_empty() {
        template
    } else {
        line.as_str()
    };
    Ok(cwd.join(name))
}

fn read_line() -> Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(err)?;
    Ok(line.trim().to_string())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let mut pending = vec![from.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(err)? {
            let entry = entry.map_err(err)?;
            if entry.file_name() == ".ok" {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(from).unwrap_or(&path);
            let dest = to.join(relative);
            if path.is_dir() {
                fs::create_dir_all(&dest).map_err(err)?;
                pending.push(path);
            } else if path.is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(err)?;
                }
                fs::copy(&path, &dest).map_err(err)?;
            }
        }
    }
    Ok(())
}

fn init(path: Option<PathBuf>, force: bool) -> Result<i32> {
    let cwd = std::env::current_dir().map_err(err)?;
    let cwd = if let Some(path) = path {
        let root = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if root.is_file() {
            return Err(Report::msg(format!("{} is a file", root.display())));
        }
        fs::create_dir_all(&root).map_err(err)?;
        root
    } else {
        cwd
    };
    let manifest_path = cwd.join("puv.toml");
    if manifest_path.exists() && !force {
        return Err(Report::new(puv_core::Error::AlreadyInitialized {
            path: manifest_path,
        }));
    }
    let dirs = Dirs::from_env();
    let php = dirs.read_pin().unwrap_or_else(|| "8.4".to_string());
    let name = project_name_from_dir(&cwd);
    write_atomic(&manifest_path, puv_core::init_template(&name, &php)).map_err(err)?;
    let src = cwd.join("src");
    fs::create_dir_all(&src).map_err(err)?;
    let main = src.join("main.php");
    if force || !main.exists() {
        fs::write(&main, puv_core::main_php(&name)).map_err(err)?;
    }
    ensure_gitignore(&cwd)?;
    println!("initialized {name}");
    Ok(0)
}

fn add(spec: &str, dev: bool) -> Result<i32> {
    let project = project()?;
    let parsed = parse_spec(spec).map_err(err)?;
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let constraint = match parsed.constraint {
        Some(constraint) => constraint,
        None => latest_constraint(&dirs, &parsed.name)?,
    };
    let table = if dev {
        "dev-dependencies"
    } else {
        "dependencies"
    };
    upsert_dependency(&project.manifest_path(), table, &parsed.name, &constraint).map_err(err)?;
    relock_and_sync(&project, false, None)?;
    println!("added {} {}", parsed.name, constraint);
    Ok(0)
}

fn remove(name: &str) -> Result<i32> {
    let project = project()?;
    let removed = remove_dependency(&project.manifest_path(), &puv_core::normalize_name(name))
        .map_err(err)?;
    if !removed {
        return Err(Report::msg(format!("{name} is not a dependency")));
    }
    relock_and_sync(&project, false, None)?;
    println!("removed {}", puv_core::normalize_name(name));
    Ok(0)
}

fn sync_project() -> Result<i32> {
    let project = project()?;
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let lock = read_lock(&project)?;
    if lock.content_hash != content_hash(&manifest) {
        return Err(Report::new(puv_core::Error::LockOutdated));
    }
    let php = ensure_installed(&dirs, &lock.runtime.php)?;
    let cache = PackageCache::new(dirs.clone()).map_err(err)?;
    sync(&SyncRequest {
        root: &project.root,
        manifest: &manifest,
        lock: &lock,
        dirs: &dirs,
        php_bin: &php,
        cache: &cache,
    })
    .map_err(err)?;
    persist_lockb(&project, &lock)?;
    println!("synced {}", project.root.display());
    Ok(0)
}

fn lock_only(upgrade: bool, package: Option<&str>) -> Result<LockFile> {
    let project = project()?;
    let lock = relock(&project, upgrade, package)?;
    println!("locked {} packages", lock.packages.len());
    Ok(lock)
}

fn load(spec: &str) -> Result<i32> {
    let project = project()?;
    let parsed = parse_spec(spec).map_err(err)?;
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let constraint = match parsed.constraint {
        Some(constraint) => constraint,
        None => latest_constraint(&dirs, &parsed.name)?,
    };
    upsert_dependency(
        &project.manifest_path(),
        "tool-dependencies",
        &parsed.name,
        &constraint,
    )
    .map_err(err)?;
    relock_and_sync(&project, false, None)?;
    println!("loaded {} {}", parsed.name, constraint);
    Ok(0)
}

fn use_php(spec: &str, global: bool) -> Result<i32> {
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let (artifact, php) = puv_runtime::ensure_spec(&dirs, spec).map_err(err)?;
    if global {
        dirs.write_pin(&artifact.version).map_err(err)?;
        println!("global php {}", artifact.version);
        return Ok(0);
    }
    let project = project()?;
    set_php(&project.manifest_path(), &artifact.version).map_err(err)?;
    if project.lock_path().is_file() {
        retarget_lock(&project, &artifact)?;
        ensure_shim(&project, &php)?;
    } else if manifest_has_deps(&project)? {
        relock_and_sync(&project, false, None)?;
    } else {
        ensure_shim(&project, &php)?;
    }
    println!("using {}", artifact.version);
    Ok(0)
}

fn retarget_lock(project: &Project, artifact: &Artifact) -> Result<()> {
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let mut lock = read_lock(project)?;
    let hash = content_hash(&manifest);
    if lock.runtime.php == artifact.version && lock.content_hash == hash {
        return Ok(());
    }
    lock.runtime.php = artifact.version.clone();
    lock.runtime.extensions = artifact.extensions.clone();
    lock.content_hash = hash;
    lock.write(&project.lock_path()).map_err(err)?;
    persist_lockb(project, &lock)?;
    Ok(())
}

fn tool(command: ToolCommand) -> Result<i32> {
    match command {
        ToolCommand::Install { package } => tool_install(&package),
        ToolCommand::List => tool_list(),
        ToolCommand::Upgrade { package } => tool_upgrade(&package),
        ToolCommand::Uninstall { package } => tool_uninstall(&package),
    }
}

fn tool_install(spec: &str) -> Result<i32> {
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let parsed = parse_spec(spec).map_err(err)?;
    let home = puv_tool::tool_home(&dirs, &parsed.name);
    fs::create_dir_all(&home).map_err(err)?;
    let php = dirs.read_pin().unwrap_or_else(|| "8.4".to_string());
    if !home.join("puv.toml").is_file() {
        write_atomic(
            &home.join("puv.toml"),
            puv_core::init_template(&puv_core::tool_dir_name(&parsed.name), &php),
        )
        .map_err(err)?;
    }
    let constraint = match parsed.constraint {
        Some(constraint) => constraint,
        None => latest_constraint(&dirs, &parsed.name)?,
    };
    upsert_dependency(
        &home.join("puv.toml"),
        "dependencies",
        &parsed.name,
        &constraint,
    )
    .map_err(err)?;
    let project = Project { root: home.clone() };
    relock_and_sync(&project, true, None)?;
    let linked = puv_tool::link_tool_bins(&dirs.bins, &home.join(".puv/bin")).map_err(err)?;
    puv_tool::write_shim_list(&home.join("shims.txt"), &linked).map_err(err)?;
    if let Ok(lock) = LockFile::read(&home.join("puv.lock")) {
        record_tool(&dirs, &home, &lock.checksums()).map_err(err)?;
    }
    println!("installed {}", parsed.name);
    Ok(0)
}

fn tool_upgrade(spec: &str) -> Result<i32> {
    let dirs = Dirs::from_env();
    let name = parse_spec(spec).map_err(err)?.name;
    let home = puv_tool::tool_home(&dirs, &name);
    if !home.join("puv.toml").is_file() {
        return Err(Report::msg(format!("{name} is not installed")));
    }
    let project = Project { root: home.clone() };
    relock_and_sync(&project, true, Some(&name))?;
    let linked = puv_tool::link_tool_bins(&dirs.bins, &home.join(".puv/bin")).map_err(err)?;
    puv_tool::write_shim_list(&home.join("shims.txt"), &linked).map_err(err)?;
    println!("upgraded {name}");
    Ok(0)
}

fn tool_uninstall(spec: &str) -> Result<i32> {
    let dirs = Dirs::from_env();
    let name = parse_spec(spec).map_err(err)?.name;
    let home = puv_tool::tool_home(&dirs, &name);
    if !home.exists() {
        return Err(Report::msg(format!("{name} is not installed")));
    }
    let shims = puv_tool::read_shim_list(&home.join("shims.txt"));
    puv_tool::unlink_bins(&dirs.bins, &shims);
    fs::remove_dir_all(&home).map_err(err)?;
    println!("uninstalled {name}");
    Ok(0)
}

fn tool_list() -> Result<i32> {
    let dirs = Dirs::from_env();
    let Ok(entries) = fs::read_dir(dirs.tools()) else {
        return Ok(0);
    };
    let mut lines = Vec::new();
    for entry in entries.flatten() {
        let lock_path = entry.path().join("puv.lock");
        let Ok(lock) = LockFile::read(&lock_path) else {
            continue;
        };
        let manifest = read_manifest(&entry.path().join("puv.toml")).map_err(err)?;
        let name = manifest
            .dependencies
            .keys()
            .next()
            .cloned()
            .unwrap_or(manifest.project.name);
        let version = lock
            .packages
            .iter()
            .find(|package| package.name == name)
            .map(|package| package.version.clone())
            .unwrap_or_else(|| "unknown".to_string());
        lines.push(format!("{name} {version}"));
    }
    lines.sort();
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

fn build() -> Result<i32> {
    let project = project()?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    if manifest_has_deps(&project)? || project.lock_path().is_file() {
        sync_project()?;
    } else {
        let dirs = Dirs::from_env();
        puv_runtime::ensure_spec(&dirs, &manifest.project.php).map_err(err)?;
    }
    match puv_build::plan(&manifest, &project.root).map_err(Report::msg)? {
        BuildStep::Script(script) => execute(
            vec![script_name_or_inline(&manifest, &script)],
            None,
            0u8,
            true,
        ),
        BuildStep::Builtin => {
            let written = write_package(&project, &manifest, None)?;
            println!("built {}", written.display());
            Ok(0)
        }
    }
}

fn script_name_or_inline(manifest: &Manifest, script: &str) -> String {
    manifest
        .scripts
        .iter()
        .find(|(_, value)| value.as_str() == script)
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "build".to_string())
}

fn pack(format: Option<String>) -> Result<i32> {
    let project = project()?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    if project.lock_path().is_file() {
        sync_project()?;
    }
    let written = write_package(&project, &manifest, format)?;
    println!("packaged {}", written.display());
    Ok(0)
}

fn write_package(
    project: &Project,
    manifest: &Manifest,
    format: Option<String>,
) -> Result<PathBuf> {
    let format = format.unwrap_or_else(|| manifest.package.format.clone());
    let entrypoint = manifest
        .package
        .entrypoint
        .clone()
        .ok_or_else(|| Report::msg("package.entrypoint is not set"))?;
    if !project.root.join(&entrypoint).is_file() {
        return Err(Report::msg(format!(
            "entrypoint {entrypoint} does not exist"
        )));
    }
    let extension = match format.as_str() {
        "zip" => "zip",
        "tar" => "tar.gz",
        _ => "phar",
    };
    let name = manifest.project.name.replace('/', "-");
    let output = project
        .root
        .join("dist")
        .join(format!("{name}.{extension}"));
    puv_package::package(&puv_package::PackageRequest {
        root: &project.root,
        name,
        entrypoint,
        format,
        output,
    })
    .map_err(err)
}

fn list_packages() -> Result<i32> {
    let project = project()?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let lock = project
        .lock_path()
        .is_file()
        .then(|| read_lock(&project))
        .transpose()?;
    print!("{}", crate::inspect::render_list(&manifest, lock.as_ref()));
    Ok(0)
}

fn why_package(name: &str) -> Result<i32> {
    let project = project()?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let lock = read_lock(&project)?;
    match crate::inspect::render_why(&manifest, &lock, name) {
        Ok(text) => {
            print!("{text}");
            Ok(0)
        }
        Err(message) => Err(Report::msg(message)),
    }
}

fn info_package(query: &str) -> Result<i32> {
    let project = project().ok();
    let lock = project
        .as_ref()
        .filter(|project| project.lock_path().is_file())
        .map(read_lock)
        .transpose()?;
    let dirs = Dirs::from_env();
    let registry = registry(&dirs)?;
    match crate::inspect::render_info(&registry, lock.as_ref(), query) {
        Ok(text) => {
            print!("{text}");
            Ok(0)
        }
        Err(message) => Err(Report::msg(message)),
    }
}

fn audit_packages() -> Result<i32> {
    let project = project()?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let lock = read_lock(&project)?;
    let dirs = Dirs::from_env();
    let registry = registry(&dirs)?;
    let (text, vulnerable) =
        crate::inspect::render_audit(&registry, &manifest, &lock).map_err(Report::msg)?;
    print!("{text}");
    Ok(if vulnerable { 1 } else { 0 })
}

fn check(format: Option<&str>) -> Result<i32> {
    let project = project()?;
    let diagnostics = puv_check::check(&project).map_err(err)?;
    if format == Some("json") {
        println!("{}", puv_check::render_json(&diagnostics).map_err(err)?);
    } else {
        if !diagnostics.is_empty() {
            print!("{}", puv_check::render_text(&diagnostics));
        }
        let files = puv_check::file_count(&project);
        let errors = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == "error")
            .count();
        let warnings = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == "warning")
            .count();
        let files = if files == 1 {
            "1 file".to_string()
        } else {
            format!("{files} files")
        };
        if errors == 0 && warnings == 0 {
            println!("checked {files}, no issues");
        } else {
            println!("{errors} errors, {warnings} warnings in {files}");
        }
    }
    Ok(if puv_check::has_errors(&diagnostics) {
        1
    } else {
        0
    })
}

fn contain(clean: bool) -> Result<i32> {
    let cwd = std::env::current_dir().map_err(err)?;
    contain_at(&cwd, clean)
}

fn contain_at(root: &Path, clean: bool) -> Result<i32> {
    if root.join("puv.toml").is_file() {
        return Err(Report::msg("puv.toml already exists"));
    }
    let dirs = Dirs::from_env();
    let versions = match puv_runtime::refresh_index(&dirs) {
        Ok(artifacts) => artifacts
            .into_iter()
            .map(|artifact| artifact.version)
            .collect::<Vec<_>>(),
        Err(_) => puv_runtime::load_index(&dirs)
            .map_err(err)?
            .into_iter()
            .map(|artifact| artifact.version)
            .collect(),
    };
    let spinner = puv_core::Spinner::start("reading composer.json");
    let migration = puv_contain::migrate(root, &versions).map_err(err)?;
    let lock = match migration.lock {
        Some(lock) => {
            spinner.set("folding composer.lock into puv.lock");
            lock
        }
        None => {
            spinner.set("resolving dependencies");
            resolve_lock(&dirs, &migration.manifest, None, false, None)?
        }
    };
    spinner.set("writing puv.toml");
    write_atomic(&root.join("puv.toml"), render_manifest(&migration.manifest)).map_err(err)?;
    lock.write(&root.join("puv.lock")).map_err(err)?;
    if clean {
        spinner.set("removing composer files");
        puv_contain::clean_composer_files(root).map_err(err)?;
    }
    spinner.finish();
    for warning in &migration.warnings {
        eprintln!("warning: {warning}");
    }
    println!("contained {} packages", lock.packages.len());
    Ok(0)
}

fn prune(global: bool, all: bool) -> Result<i32> {
    if !global {
        return Err(Report::msg("pass --global to prune the user cache"));
    }
    let dirs = Dirs::from_env();
    let report = puv_tool::prune(&dirs, all).map_err(err)?;
    println!("removed {} cache entries", report.removed);
    Ok(0)
}

fn php(command: PhpCommand) -> Result<i32> {
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    match command {
        PhpCommand::List { all } => php_list(&dirs, all),
        PhpCommand::Install { version, all } => php_install(&dirs, version, all),
        PhpCommand::Remove { version, all } => php_remove(&dirs, version, all),
    }
}

fn php_list(dirs: &Dirs, all: bool) -> Result<i32> {
    let artifacts = index(dirs)?;
    let installed = puv_runtime::installed_versions(dirs);
    let mut versions: Vec<String> = artifacts
        .iter()
        .map(|artifact| artifact.version.clone())
        .collect();
    for version in &installed {
        if !versions.iter().any(|item| item == version) {
            versions.push(version.clone());
        }
    }
    versions.sort_by(|left, right| puv_runtime::cmp_version(left, right));
    versions.dedup();
    for version in versions {
        let is_installed = installed.iter().any(|item| item == &version);
        if !all && !is_installed {
            continue;
        }
        println!("{version}  {}", php_status(is_installed));
    }
    Ok(0)
}

fn php_install(dirs: &Dirs, version: Option<String>, all: bool) -> Result<i32> {
    if version.is_none() && !all {
        return Err(Report::msg("pass a PHP version or --all"));
    }
    let artifacts = index(dirs)?;
    let matched = puv_runtime::matching_artifacts(&artifacts, version.as_deref()).map_err(err)?;
    for artifact in matched {
        puv_runtime::install(dirs, artifact).map_err(err)?;
        println!("installed php {}", artifact.version);
    }
    Ok(0)
}

fn php_remove(dirs: &Dirs, version: Option<String>, all: bool) -> Result<i32> {
    if version.is_none() && !all {
        return Err(Report::msg("pass a PHP version or --all"));
    }
    let removed = puv_runtime::remove(dirs, version.as_deref()).map_err(err)?;
    for version in removed {
        println!("removed php {version}");
    }
    Ok(0)
}

fn php_status(installed: bool) -> String {
    let (text, color) = if installed {
        ("installed", "32")
    } else {
        ("installation required", "33")
    };
    puv_core::paint(text, color, puv_core::stdout_is_tty())
}

fn relock_and_sync(project: &Project, upgrade: bool, only: Option<&str>) -> Result<()> {
    let lock = relock(project, upgrade, only)?;
    let dirs = Dirs::from_env();
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let php = ensure_installed(&dirs, &lock.runtime.php)?;
    let cache = PackageCache::new(dirs.clone()).map_err(err)?;
    sync(&SyncRequest {
        root: &project.root,
        manifest: &manifest,
        lock: &lock,
        dirs: &dirs,
        php_bin: &php,
        cache: &cache,
    })
    .map_err(err)?;
    Ok(())
}

fn relock(project: &Project, upgrade: bool, only: Option<&str>) -> Result<LockFile> {
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    let previous = project
        .lock_path()
        .is_file()
        .then(|| LockFile::read(&project.lock_path()))
        .transpose()
        .map_err(err)?;
    let lock = resolve_lock(&dirs, &manifest, previous.as_ref(), upgrade, only)?;
    lock.write(&project.lock_path()).map_err(err)?;
    persist_lockb(project, &lock)?;
    Ok(lock)
}

fn resolve_lock(
    dirs: &Dirs,
    manifest: &Manifest,
    previous: Option<&LockFile>,
    upgrade: bool,
    only: Option<&str>,
) -> Result<LockFile> {
    let artifacts = index(dirs)?;
    let artifact = puv_runtime::select(&artifacts, &manifest.project.php).map_err(err)?;
    let runtime = Version::parse(&artifact.version).map_err(err)?;
    let extensions: BTreeSet<String> = artifact.extensions.iter().cloned().collect();
    let registry = registry(dirs)?;
    let cache = PackageCache::new(dirs.clone()).map_err(err)?;
    let preferred = preferred_versions(previous, upgrade, only);
    let solved = solve(SolveRequest {
        root_deps: root_dependencies(manifest),
        registry: &registry,
        preferred,
        runtime: runtime.clone(),
        extensions: extensions.clone(),
    })
    .map_err(err)?;
    let packages = fetch_locked(&cache, &solved)?;
    let mut tools = Vec::new();
    for (name, spec) in &manifest.tool_dependencies {
        let mut deps = BTreeMap::new();
        deps.insert(name.clone(), spec.clone());
        let preferred = tool_preferred(previous, name, upgrade || only == Some(name.as_str()));
        let solved = solve(SolveRequest {
            root_deps: deps,
            registry: &registry,
            preferred,
            runtime: runtime.clone(),
            extensions: extensions.clone(),
        })
        .map_err(err)?;
        let version = solved
            .iter()
            .find(|release| &release.name == name)
            .map(|release| release.version.to_string())
            .unwrap_or_else(|| spec.clone());
        tools.push(LockedTool {
            name: name.clone(),
            version,
            php: artifact.version.clone(),
            packages: fetch_locked(&cache, &solved)?,
        });
    }
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let mut extension_list: Vec<_> = extensions.into_iter().collect();
    extension_list.sort();
    Ok(LockFile {
        lock_version: LOCK_VERSION,
        content_hash: content_hash(manifest),
        runtime: LockedRuntime {
            php: artifact.version.clone(),
            extensions: extension_list,
        },
        packages,
        tools,
    })
}

fn fetch_locked(cache: &PackageCache, releases: &[PackageRelease]) -> Result<Vec<LockedPackage>> {
    let mut results = Vec::with_capacity(releases.len());
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for release in releases {
            handles.push(scope.spawn(|| locked_from_release(cache, release)));
        }
        for handle in handles {
            results.push(handle.join().expect("package fetch panicked"));
        }
    });
    let mut packages = Vec::new();
    for result in results {
        packages.push(result?);
    }
    packages.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.version.cmp(&right.version))
    });
    Ok(packages)
}

fn locked_from_release(cache: &PackageCache, release: &PackageRelease) -> Result<LockedPackage> {
    let (source, source_type, registry_checksum, checksum) = if let Some(dist) = &release.dist {
        let fetched = cache
            .fetch_named(
                &dist.url,
                &dist.kind,
                dist.shasum.as_deref(),
                &format!("{} {}", release.name, release.version),
            )
            .map_err(err)?;
        (
            dist.url.clone(),
            dist.kind.clone(),
            dist.shasum.clone(),
            fetched.sha256,
        )
    } else {
        let fetched = cache
            .empty_artifact(&release.name, &release.version.to_string())
            .map_err(err)?;
        (String::new(), String::new(), None, fetched.sha256)
    };
    let mut dependencies: Vec<_> = release.dependencies.keys().cloned().collect();
    dependencies.sort();
    let mut provides: Vec<_> = release.provides.keys().cloned().collect();
    provides.sort();
    let mut conflicts: Vec<_> = release
        .conflicts
        .iter()
        .map(|(name, spec)| format!("{name} {spec}"))
        .collect();
    conflicts.sort();
    Ok(LockedPackage {
        name: release.name.clone(),
        version: release.version.to_string(),
        source,
        source_type,
        checksum,
        registry_checksum,
        dependencies,
        autoload: release.autoload.clone(),
        bins: release.bins.clone(),
        provides,
        conflicts,
    })
}

fn prepare_runtime(
    project: Option<&Project>,
    override_php: Option<&str>,
    verbose: u8,
) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>)> {
    let dirs = Dirs::from_env();
    dirs.ensure().map_err(err)?;
    if let Some(project) = project {
        let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
        let spec = override_php.unwrap_or(manifest.project.php.as_str());
        if project.lock_path().is_file() {
            let lock = load_for_run(project, verbose)?;
            if override_php.is_none() && lock.content_hash != content_hash(&manifest) {
                return Err(Report::new(puv_core::Error::LockOutdated));
            }
            let version = if override_php.is_some() {
                spec.to_string()
            } else {
                lock.runtime.php.clone()
            };
            let php = ensure_installed(&dirs, &version)?;
            if override_php.is_none() && !env_is_current(&project.root, &lock) {
                let cache = PackageCache::new(dirs.clone()).map_err(err)?;
                sync(&SyncRequest {
                    root: &project.root,
                    manifest: &manifest,
                    lock: &lock,
                    dirs: &dirs,
                    php_bin: &php,
                    cache: &cache,
                })
                .map_err(err)?;
            }
            let autoload = ensure_shim(project, &php)?;
            return Ok((php, autoload, Some(project.root.clone())));
        }
        let php = ensure_installed(&dirs, spec)?;
        let autoload = ensure_shim(project, &php)?;
        return Ok((php, autoload, Some(project.root.clone())));
    }
    let spec = override_php
        .map(str::to_string)
        .or_else(|| dirs.read_pin())
        .ok_or_else(|| Report::msg("no PHP version selected; run `puv use --global <version>`"))?;
    let php = ensure_installed(&dirs, &spec)?;
    Ok((php, None, None))
}

fn load_for_run(project: &Project, verbose: u8) -> Result<LockFile> {
    let bytes = fs::read(project.lock_path()).map_err(err)?;
    if let Some(lock) = puv_core::load_lockb(&project.lockb_path(), &bytes) {
        if verbose > 0 {
            eprintln!("lock source: lockb");
        }
        return Ok(lock);
    }
    if verbose > 0 {
        eprintln!("lock source: toml");
    }
    let text = String::from_utf8(bytes).map_err(err)?;
    let lock = LockFile::parse(&text).map_err(err)?;
    persist_lockb(project, &lock)?;
    Ok(lock)
}

fn persist_lockb(project: &Project, lock: &LockFile) -> Result<()> {
    let bytes = fs::read(project.lock_path()).map_err(err)?;
    puv_core::write_lockb(&project.lockb_path(), &bytes, lock).map_err(err)
}

fn ensure_shim(project: &Project, php: &Path) -> Result<Option<PathBuf>> {
    let env = project.root.join(".puv");
    let bin = env.join("bin");
    fs::create_dir_all(&bin).map_err(err)?;
    let autoload = env.join("autoload.php");
    if !autoload.is_file() {
        fs::write(&autoload, "<?php\n").map_err(err)?;
    }
    write_php_shim(&bin.join("php"), php, &autoload).map_err(err)?;
    Ok(Some(autoload))
}

fn ensure_installed(dirs: &Dirs, spec: &str) -> Result<PathBuf> {
    if let Some(path) = puv_runtime::php_bin(dirs, spec) {
        return Ok(path);
    }
    puv_runtime::ensure_spec(dirs, spec)
        .map(|(_, bin)| bin)
        .map_err(err)
}

fn latest_constraint(dirs: &Dirs, name: &str) -> Result<String> {
    let registry = registry(dirs)?;
    let releases = registry.releases(name).map_err(err)?;
    let best = releases
        .iter()
        .filter(|release| release.version.is_stable())
        .max_by(|left, right| left.version.cmp(&right.version))
        .ok_or_else(|| Report::msg(format!("no stable release for {name}")))?;
    Ok(caret_spec(&best.version))
}

fn caret_spec(version: &Version) -> String {
    if version.major > 0 {
        format!("^{}.{}", version.major, version.minor)
    } else if version.minor > 0 {
        format!("^0.{}", version.minor)
    } else {
        format!("^0.0.{}", version.patch)
    }
}

fn root_dependencies(manifest: &Manifest) -> BTreeMap<String, String> {
    let mut deps = manifest.dependencies.clone();
    for (name, spec) in &manifest.dev_dependencies {
        deps.entry(name.clone())
            .and_modify(|existing| {
                if existing != spec {
                    *existing = format!("{existing}, {spec}");
                }
            })
            .or_insert_with(|| spec.clone());
    }
    deps
}

fn preferred_versions(
    previous: Option<&LockFile>,
    upgrade: bool,
    only: Option<&str>,
) -> BTreeMap<String, Version> {
    if upgrade && only.is_none() {
        return BTreeMap::new();
    }
    let Some(previous) = previous else {
        return BTreeMap::new();
    };
    previous
        .packages
        .iter()
        .filter(|package| only != Some(package.name.as_str()))
        .filter_map(|package| {
            Version::parse(&package.version)
                .ok()
                .map(|version| (package.name.clone(), version))
        })
        .collect()
}

fn tool_preferred(
    previous: Option<&LockFile>,
    name: &str,
    upgrade: bool,
) -> BTreeMap<String, Version> {
    if upgrade {
        return BTreeMap::new();
    }
    let Some(tool) = previous.and_then(|lock| lock.tools.iter().find(|tool| tool.name == name))
    else {
        return BTreeMap::new();
    };
    tool.packages
        .iter()
        .filter_map(|package| {
            Version::parse(&package.version)
                .ok()
                .map(|version| (package.name.clone(), version))
        })
        .collect()
}

fn registry(dirs: &Dirs) -> Result<HttpRegistry> {
    let base = std::env::var("PUV_REGISTRY_URL")
        .unwrap_or_else(|_| "https://repo.packagist.org".to_string());
    HttpRegistry::new(dirs.metadata(), base).map_err(err)
}

fn index(dirs: &Dirs) -> Result<Vec<Artifact>> {
    if std::env::var_os("PUV_RUNTIME_INDEX").is_some() {
        return puv_runtime::load_index(dirs).map_err(err);
    }
    match puv_runtime::refresh_index(dirs) {
        Ok(artifacts) if !artifacts.is_empty() => Ok(artifacts),
        _ => puv_runtime::load_index(dirs).map_err(err),
    }
}

fn project() -> Result<Project> {
    let cwd = std::env::current_dir().map_err(err)?;
    Project::discover(&cwd).map_err(Report::new)
}

fn read_lock(project: &Project) -> Result<LockFile> {
    LockFile::read(&project.lock_path()).map_err(err)
}

fn manifest_has_deps(project: &Project) -> Result<bool> {
    let manifest = read_manifest(&project.manifest_path()).map_err(err)?;
    Ok(!manifest.dependencies.is_empty()
        || !manifest.dev_dependencies.is_empty()
        || !manifest.tool_dependencies.is_empty())
}

fn as_php_file(project: Option<&Project>, head: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(head);
    if direct.is_file() {
        return Some(absolute_from_cwd(&direct));
    }
    if let Some(project) = project {
        let nested = project.root.join(head);
        if nested.is_file() {
            return Some(absolute_from_cwd(&nested));
        }
    }
    if head.ends_with(".php") {
        Some(absolute_from_cwd(&direct))
    } else {
        None
    }
}

/// `puv run` changes the working directory to the project root. A relative
/// script path would then be opened there, not in the directory the user
/// invoked the command from.
fn absolute_from_cwd(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn apply_env(command: &mut Command, project: Option<&Project>, php: &Path) {
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(project) = project {
        let mut prefix = project.root.join(".puv/bin").into_os_string();
        if let Ok(entries) = fs::read_dir(project.root.join(".puv/tools")) {
            for entry in entries.flatten() {
                let bin = entry.path().join("bin");
                if bin.is_dir() {
                    let mut next = bin.into_os_string();
                    next.push(":");
                    next.push(&prefix);
                    prefix = next;
                }
            }
        }
        prefix.push(":");
        prefix.push(&path);
        path = prefix;
    }
    command.env("PATH", path);
    command.env("PUV_PHP", php);
}

fn run_status(command: &mut Command) -> Result<i32> {
    let status = command.status().map_err(err)?;
    Ok(status.code().unwrap_or(1))
}

fn ensure_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    for line in [".puv/", "dist/"] {
        if !text.lines().any(|existing| existing.trim() == line) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(line);
            text.push('\n');
        }
    }
    fs::write(&path, text).map_err(err)
}

fn render_manifest(manifest: &Manifest) -> String {
    let mut out = format!(
        "[project]\nname = \"{}\"\nphp = \"{}\"\n\n[dependencies]\n",
        manifest.project.name, manifest.project.php
    );
    for (name, spec) in &manifest.dependencies {
        out.push_str(&format!(
            "{} = \"{spec}\"\n",
            puv_core::toml_basic_string(name)
        ));
    }
    out.push_str("\n[dev-dependencies]\n");
    for (name, spec) in &manifest.dev_dependencies {
        out.push_str(&format!(
            "{} = \"{spec}\"\n",
            puv_core::toml_basic_string(name)
        ));
    }
    out.push_str("\n[tool-dependencies]\n");
    for (name, spec) in &manifest.tool_dependencies {
        out.push_str(&format!(
            "{} = \"{spec}\"\n",
            puv_core::toml_basic_string(name)
        ));
    }
    out.push_str("\n[scripts]\n");
    for (name, spec) in &manifest.scripts {
        out.push_str(&format!(
            "{} = \"{spec}\"\n",
            puv_core::toml_basic_string(name)
        ));
    }
    out.push_str(&format!(
        "\n[package]\nformat = \"{}\"\n",
        manifest.package.format
    ));
    if let Some(entrypoint) = &manifest.package.entrypoint {
        out.push_str(&format!("entrypoint = \"{entrypoint}\"\n"));
    }
    out
}

fn err(error: impl std::fmt::Display) -> Report {
    Report::msg(error.to_string())
}
