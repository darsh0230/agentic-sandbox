use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

pub fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > 128
        || run_id.as_bytes().contains(&0)
        || run_id.chars().any(char::is_control)
        || run_id.contains(['/', '\\'])
        || run_id.contains("..")
    {
        bail!("run_id must be a non-empty path-safe identifier");
    }

    let mut components = Path::new(run_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("run_id must be a non-empty path-safe identifier");
    }
    Ok(())
}

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
    let mut out = PathBuf::new();
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                has_normal_component = true;
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("sandbox path must not contain '..'"),
            Component::RootDir => out.push(Path::new("/")),
            Component::Prefix(_) => bail!("sandbox path must be an absolute Unix path"),
        }
    }
    if !has_normal_component {
        bail!("sandbox path must name a location below '/'");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{validate_relative_path, validate_run_id, validate_sandbox_absolute_path};
    use std::path::PathBuf;

    #[test]
    fn run_ids_accept_single_safe_path_components() {
        for run_id in ["run-001", "run_001", "r.1", "hello world", "仕事"] {
            assert!(validate_run_id(run_id).is_ok(), "{run_id:?}");
        }
        assert!(validate_run_id(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn run_ids_reject_unsafe_or_ambiguous_values() {
        for run_id in [
            "",
            ".",
            "..",
            "run..001",
            "/absolute",
            "nested/run",
            r"nested\run",
            "nul\0byte",
            "line\nbreak",
            "delete\u{7f}",
        ] {
            let error = validate_run_id(run_id).unwrap_err();
            assert_eq!(
                error.to_string(),
                "run_id must be a non-empty path-safe identifier",
                "{run_id:?}"
            );
        }
        assert!(validate_run_id(&"a".repeat(129)).is_err());
        assert!(
            validate_run_id(&"仕".repeat(64)).is_err(),
            "the limit is measured in encoded bytes"
        );
    }

    #[test]
    fn relative_paths_are_normalized_without_escaping() {
        for (input, expected) in [
            ("input/task.md", "input/task.md"),
            ("./input//task.md", "input/task.md"),
            ("name", "name"),
            ("unicode/仕事.txt", "unicode/仕事.txt"),
        ] {
            assert_eq!(
                validate_relative_path(input).unwrap(),
                PathBuf::from(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn relative_paths_reject_empty_absolute_parent_and_nul_components() {
        for path in [
            "",
            ".",
            "./",
            "/etc/passwd",
            "../secret",
            "safe/../secret",
            "safe/\0secret",
        ] {
            assert!(validate_relative_path(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn sandbox_paths_are_absolute_normalized_and_below_root() {
        for (input, expected) in [
            ("/opt/fixtures", "/opt/fixtures"),
            ("/opt/./fixtures//data", "/opt/fixtures/data"),
            ("/仕事/data", "/仕事/data"),
        ] {
            assert_eq!(
                validate_sandbox_absolute_path(input).unwrap(),
                PathBuf::from(expected),
                "{input:?}"
            );
        }
    }

    #[test]
    fn sandbox_paths_reject_root_relative_parent_and_nul_values() {
        for path in [
            "",
            "/",
            "/.",
            "opt/fixtures",
            "/opt/../etc",
            "/opt/\0fixtures",
        ] {
            assert!(validate_sandbox_absolute_path(path).is_err(), "{path:?}");
        }
    }
}
