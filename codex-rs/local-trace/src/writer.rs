use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;

pub fn create_dir(path: &Path) -> Result<()> {
    create_dir_all_private(path)?;
    set_dir_permissions(path)?;
    Ok(())
}

pub fn create_dir_new(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    private_dir_builder()
        .create(path)
        .with_context(|| format!("create {}", path.display()))?;
    set_dir_permissions(path)?;
    Ok(())
}

pub fn write_json_pretty(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let file = create_file(path)?;
    serde_json::to_writer_pretty(file, value)
        .with_context(|| format!("write JSON {}", path.display()))
}

pub fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    set_private_file_mode(&mut options);
    let mut file = options
        .open(path)
        .with_context(|| format!("open JSONL {}", path.display()))?;
    set_file_permissions(path)?;
    serde_json::to_writer(&mut file, value)
        .with_context(|| format!("append JSONL {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("append newline {}", path.display()))
}

pub fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let mut file = create_file(path)?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))
}

fn create_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    set_private_file_mode(&mut options);
    let file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    set_file_permissions(path)?;
    Ok(file)
}

fn create_dir_all_private(path: &Path) -> Result<()> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component);
        if current.exists() {
            continue;
        }
        match private_dir_builder().create(&current) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| format!("create {}", current.display()));
            }
        }
        set_dir_permissions(&current)?;
    }
    Ok(())
}

fn private_dir_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    set_private_dir_mode(&mut builder);
    builder
}

#[cfg(unix)]
fn set_private_dir_mode(builder: &mut DirBuilder) {
    use std::os::unix::fs::DirBuilderExt;

    builder.mode(0o700);
}

#[cfg(not(unix))]
fn set_private_dir_mode(_builder: &mut DirBuilder) {}

#[cfg(unix)]
fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set permissions {}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set permissions {}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
