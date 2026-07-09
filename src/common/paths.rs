use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

pub fn validate_relative_path(path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        bail!("path must not be empty");
    }
    if path.as_bytes().contains(&0) {
        bail!("path must not contain NUL bytes");
    }
    let path = Path::new(path);
    if path.is_absolute() {
        bail!("path must be relative");
    }

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!("path must not contain '..'"),
            Component::RootDir | Component::Prefix(_) => bail!("path must be relative"),
        }
    }
    if out.as_os_str().is_empty() {
        bail!("path must contain a normal component");
    }
    Ok(out)
}

pub fn validate_sandbox_absolute_path(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        bail!("sandbox path is invalid");
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        bail!("sandbox path must be absolute");
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!("sandbox path must not contain '..'");
        }
    }
    Ok(path.to_path_buf())
}
