//! Composer-compatible versions and constraints.

mod constraint;
mod version;

pub use constraint::{Atom, Bound, Constraint};
pub use version::{Stability, Version};

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
