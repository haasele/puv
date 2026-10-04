//! Composer v2 registry client.

mod client;
mod minifier;
mod model;

pub use client::{HttpRegistry, MemoryRegistry, MetadataProvider};
pub use model::{Dist, PackageRelease};

use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
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
