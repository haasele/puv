//! Build pipeline, distinct from a project script named `build`.

use std::path::Path;

use puv_core::Manifest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildStep {
    Script(String),
    Builtin,
}

pub fn plan(manifest: &Manifest, root: &Path) -> Result<BuildStep, String> {
    if let Some(script) = manifest.scripts.get("build") {
        return Ok(BuildStep::Script(script.clone()));
    }
    if let Some(entrypoint) = &manifest.package.entrypoint
        && !root.join(entrypoint).is_file()
    {
        return Err(format!("entrypoint {entrypoint} does not exist"));
    }
    Ok(BuildStep::Builtin)
}
