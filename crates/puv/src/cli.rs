use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "puv",
    version,
    about = "Fast PHP toolchain and package manager",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Increase log verbosity.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a project from an application template.
    Create {
        /// laravel, codeigniter, symfony, or slim. Prompts in a terminal when omitted.
        template: Option<String>,
        /// Directory to create. Defaults to the template name.
        directory: Option<PathBuf>,
    },
    /// Create a puv.toml project.
    Init {
        /// Directory to create and initialize. Defaults to the current directory.
        path: Option<PathBuf>,
        /// Overwrite an existing puv.toml.
        #[arg(long)]
        force: bool,
    },
    /// Add a dependency, resolve it, and install it.
    Add {
        package: String,
        /// Record the package in dev-dependencies.
        #[arg(long)]
        dev: bool,
    },
    /// Install a package, or synchronize the lockfile when no package is given.
    Install {
        package: Option<String>,
        #[arg(long)]
        dev: bool,
    },
    /// Remove a dependency and synchronize the environment.
    Remove { package: String },
    /// Install exactly the packages recorded in puv.lock.
    Sync,
    /// Resolve dependencies and write puv.lock.
    Lock {
        /// Ignore locked versions and resolve the newest match.
        #[arg(long)]
        upgrade: bool,
        /// Upgrade only this package within its constraint.
        package: Option<String>,
    },
    /// Run a PHP file, project script, or tool inside the project environment.
    Run {
        /// Override the PHP version for this invocation.
        #[arg(long)]
        php: Option<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Select the project PHP version, installing it when needed.
    Use {
        version: String,
        /// Set the user-wide PHP pin instead of the project pin.
        #[arg(long)]
        global: bool,
    },
    /// Install an isolated project-local tool.
    Load {
        package: String,
        /// Accepted for compatibility; tools stay in tool-dependencies.
        #[arg(long)]
        dev: bool,
    },
    /// Manage globally installed tools.
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    /// Run the PUV build pipeline.
    Build,
    /// Build a distributable archive.
    Package {
        /// phar, zip, or tar.
        #[arg(long)]
        format: Option<String>,
    },
    /// List installed packages, extensions, and their dependencies.
    List,
    /// Show why a package or extension is installed.
    Why { package: String },
    /// Show registry information for a package.
    Info { package: String },
    /// Report vulnerabilities, outdated packages, and available upgrades.
    Audit,
    /// Check PHP syntax and project consistency.
    Check {
        /// text or json.
        #[arg(long)]
        format: Option<String>,
    },
    /// Migrate a Composer project without deleting its files.
    Contain {
        /// Remove composer.json, composer.lock, and vendor/ after migration.
        #[arg(long)]
        clean: bool,
    },
    /// Delete unreferenced cache artifacts.
    Prune {
        /// Operate on the user-wide cache.
        #[arg(long)]
        global: bool,
        /// Remove every cache artifact, including ones still referenced.
        #[arg(long)]
        all: bool,
    },
    /// Manage PHP runtimes.
    Php {
        #[command(subcommand)]
        command: PhpCommand,
    },
}

#[derive(Subcommand)]
pub enum ToolCommand {
    /// Install a tool into its own environment.
    Install { package: String },
    /// List installed tools.
    List,
    /// Upgrade one tool.
    Upgrade { package: String },
    /// Remove a tool and its shim.
    Uninstall { package: String },
}

#[derive(Subcommand)]
pub enum PhpCommand {
    /// List installed PHP versions.
    List {
        /// Also show versions that are not installed yet.
        #[arg(long)]
        all: bool,
    },
    /// Download and install PHP versions.
    Install {
        /// Minor line or exact patch. A minor line installs every matching patch.
        version: Option<String>,
        /// Install every indexed version. Ignored when a version is given.
        #[arg(long)]
        all: bool,
    },
    /// Remove installed PHP versions.
    Remove {
        /// Minor line or exact patch. A minor line removes every matching patch.
        version: Option<String>,
        /// Remove every PHP runtime installed by puv. Ignored when a version is given.
        #[arg(long)]
        all: bool,
    },
}

pub fn direct_script(args: &[String]) -> Option<(String, Vec<String>)> {
    let mut positional = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-c" || arg == "--code" || arg.starts_with("--code=") {
            return None;
        }
        if arg == "-v" || arg == "--verbose" {
            continue;
        }
        if arg == "--php" {
            skip = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        positional.push(arg.clone());
    }
    let first = positional.first()?.clone();
    if is_command(&first) {
        return None;
    }
    if first.ends_with(".php") || PathBuf::from(&first).is_file() {
        let rest = positional.into_iter().skip(1).collect();
        return Some((first, rest));
    }
    None
}

pub fn inline_code(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-c" || arg == "--code" {
            return Some(iter.next().cloned().unwrap_or_default());
        }
        if let Some(code) = arg.strip_prefix("--code=") {
            return Some(code.to_string());
        }
    }
    None
}

fn is_command(name: &str) -> bool {
    matches!(
        name,
        "init"
            | "create"
            | "add"
            | "install"
            | "remove"
            | "sync"
            | "lock"
            | "run"
            | "use"
            | "load"
            | "tool"
            | "build"
            | "package"
            | "check"
            | "list"
            | "why"
            | "info"
            | "audit"
            | "contain"
            | "prune"
            | "php"
            | "help"
    )
}
