//! Deterministic PHAR, zip, and tar packages.

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

const FIXED_TIME: u32 = 946_684_800;

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

pub struct PackageRequest<'a> {
    pub root: &'a Path,
    pub name: String,
    pub entrypoint: String,
    pub format: String,
    pub output: PathBuf,
}

pub fn package(request: &PackageRequest<'_>) -> Result<PathBuf> {
    let files = collect_files(request)?;
    fs::create_dir_all(request.output.parent().unwrap_or(Path::new(".")))
        .map_err(|err| Error::new(err.to_string()))?;
    match request.format.as_str() {
        "phar" => write_phar(&request.output, &request.name, &request.entrypoint, &files)?,
        "zip" => write_zip(&request.output, &files)?,
        "tar" => write_tar(&request.output, &files)?,
        other => return Err(Error::new(format!("unknown package format '{other}'"))),
    }
    Ok(request.output.clone())
}

fn collect_files(request: &PackageRequest<'_>) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let autoload = request.root.join(".puv/autoload.php");
    if autoload.is_file() {
        files.insert(
            "autoload.php".to_string(),
            fs::read(&autoload).map_err(|err| Error::new(err.to_string()))?,
        );
    }
    copy_tree(&request.root.join("src"), "src", &mut files)?;
    let entry = request.root.join(&request.entrypoint);
    if entry.is_file() {
        let key = request.entrypoint.replace('\\', "/");
        files
            .entry(key)
            .or_insert(fs::read(&entry).map_err(|err| Error::new(err.to_string()))?);
    }
    if let Ok(text) = fs::read(request.root.join("puv.toml")) {
        files.insert("puv.toml".to_string(), text);
    }
    let deps = request.root.join(".puv/deps");
    if deps.is_dir() {
        copy_deps(&deps, "deps", &mut files)?;
    }
    Ok(files)
}

fn copy_tree(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).map_err(|err| Error::new(err.to_string()))? {
            let entry = entry.map_err(|err| Error::new(err.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(dir)
                .map_err(|err| Error::new(err.to_string()))?;
            let key = format!("{prefix}/{}", relative.to_string_lossy().replace('\\', "/"));
            files.insert(
                key,
                fs::read(&path).map_err(|err| Error::new(err.to_string()))?,
            );
        }
    }
    Ok(())
}

fn copy_deps(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let mut pending = vec![(dir.to_path_buf(), prefix.to_string())];
    while let Some((current, prefix)) = pending.pop() {
        let entries = fs::read_dir(&current).map_err(|err| Error::new(err.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|err| Error::new(err.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".ok" || name == "autoload.php" {
                continue;
            }
            let path = entry.path();
            let key = format!("{prefix}/{name}");
            if path.is_dir() {
                pending.push((path, key));
            } else if path.is_file() {
                files.insert(
                    key,
                    fs::read(&path).map_err(|err| Error::new(err.to_string()))?,
                );
            }
        }
    }
    Ok(())
}

fn write_phar(
    path: &Path,
    name: &str,
    entrypoint: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let alias = format!("{name}.phar");
    let entrypoint = entrypoint.trim_start_matches("./").replace('\\', "/");
    let stub = format!(
        "#!/usr/bin/env php\n<?php\nPhar::mapPhar(\"{alias}\");\nrequire 'phar://{alias}/autoload.php';\nrequire 'phar://{alias}/{entrypoint}';\n__HALT_COMPILER(); ?>\r\n"
    );
    let mut manifest_body = Vec::new();
    manifest_body.extend_from_slice(&(files.len() as u32).to_le_bytes());
    manifest_body.extend_from_slice(&[0x11, 0x00]);
    manifest_body.extend_from_slice(&0x0001_0000u32.to_le_bytes());
    manifest_body.extend_from_slice(&(alias.len() as u32).to_le_bytes());
    manifest_body.extend_from_slice(alias.as_bytes());
    manifest_body.extend_from_slice(&0u32.to_le_bytes());
    let mut contents = Vec::new();
    for (name, bytes) in files {
        let crc = crc32fast::hash(bytes);
        manifest_body.extend_from_slice(&(name.len() as u32).to_le_bytes());
        manifest_body.extend_from_slice(name.as_bytes());
        manifest_body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        manifest_body.extend_from_slice(&FIXED_TIME.to_le_bytes());
        manifest_body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        manifest_body.extend_from_slice(&crc.to_le_bytes());
        manifest_body.extend_from_slice(&0x1a4u32.to_le_bytes());
        manifest_body.extend_from_slice(&0u32.to_le_bytes());
        contents.extend_from_slice(bytes);
    }
    let mut archive = Vec::new();
    archive.extend_from_slice(stub.as_bytes());
    archive.extend_from_slice(&(manifest_body.len() as u32).to_le_bytes());
    archive.extend_from_slice(&manifest_body);
    archive.extend_from_slice(&contents);
    let digest = Sha256::digest(&archive);
    archive.extend_from_slice(&digest);
    archive.extend_from_slice(&0x0000_0003u32.to_le_bytes());
    archive.extend_from_slice(b"GBMB");
    fs::write(path, archive).map_err(|err| Error::new(err.to_string()))
}

fn write_zip(path: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let file = File::create(path).map_err(|err| Error::new(err.to_string()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(
            zip::DateTime::from_date_and_time(2000, 1, 1, 0, 0, 0)
                .map_err(|err| Error::new(format!("invalid zip timestamp: {err}")))?,
        )
        .unix_permissions(0o644);
    for (name, bytes) in files {
        writer
            .start_file(name, options)
            .map_err(|err| Error::new(err.to_string()))?;
        writer
            .write_all(bytes)
            .map_err(|err| Error::new(err.to_string()))?;
    }
    writer.finish().map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

fn write_tar(path: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let file = File::create(path).map_err(|err| Error::new(err.to_string()))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (name, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(u64::from(FIXED_TIME));
        header.set_cksum();
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .map_err(|err| Error::new(err.to_string()))?;
    }
    builder
        .finish()
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phar_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".puv")).unwrap();
        fs::write(root.join("src/main.php"), b"<?php echo \"hi\\n\";\n").unwrap();
        fs::write(root.join(".puv/autoload.php"), b"<?php\n").unwrap();
        fs::write(
            root.join("puv.toml"),
            b"[project]\nname = \"demo\"\nphp = \"8.4\"\n",
        )
        .unwrap();
        let output = root.join("dist/demo.phar");
        let request = PackageRequest {
            root,
            name: "demo".into(),
            entrypoint: "src/main.php".into(),
            format: "phar".into(),
            output: output.clone(),
        };
        package(&request).unwrap();
        let first = fs::read(&output).unwrap();
        package(&request).unwrap();
        let second = fs::read(&output).unwrap();
        assert_eq!(first, second);
        assert!(first.windows(4).any(|window| window == b"GBMB"));
    }
}
