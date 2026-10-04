use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Diagnostic)]
pub enum Error {
    #[error("could not find puv.toml in {start} or any parent directory")]
    #[diagnostic(help("run `puv init` to create a project"))]
    NoProject { start: PathBuf },

    #[error("{path} already exists")]
    #[diagnostic(help("pass --force to overwrite the project manifest"))]
    AlreadyInitialized { path: PathBuf },

    #[error("lockfile is out of date with puv.toml")]
    #[diagnostic(help("run `puv lock` to update puv.lock"))]
    LockOutdated,

    #[error("{0}")]
    Message(String),
}

impl Error {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
