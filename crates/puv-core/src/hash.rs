use sha2::{Digest, Sha256};

use crate::Manifest;

pub fn content_hash(manifest: &Manifest) -> String {
    let mut lines = vec![format!("php={}", manifest.project.php.trim())];
    for (name, constraint) in &manifest.dependencies {
        lines.push(format!("dep {name}={constraint}"));
    }
    for (name, constraint) in &manifest.dev_dependencies {
        lines.push(format!("dev {name}={constraint}"));
    }
    for (name, constraint) in &manifest.tool_dependencies {
        lines.push(format!("tool {name}={constraint}"));
    }
    sha256_prefixed(lines.join("\n").as_bytes())
}

pub fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
