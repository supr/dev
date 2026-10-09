use crate::{DevError, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub project: Project,
    pub repositories: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
}

impl ProjectConfig {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("project.toml");
        let source = fs::read_to_string(&path)
            .map_err(|error| DevError(format!("could not read {}: {error}", path.display())))?;
        let config: Self = toml::from_str(&source)
            .map_err(|error| DevError(format!("invalid {}: {error}", path.display())))?;

        if config.project.name.trim().is_empty() {
            return Err(DevError("project.name must not be empty".into()));
        }
        if config.repositories.is_empty() {
            return Err(DevError(
                "project.toml must define at least one repository".into(),
            ));
        }
        for (name, source) in &config.repositories {
            validate_component("repository", name)?;
            if source.trim().is_empty() {
                return Err(DevError(format!(
                    "repository {name:?} must have a non-empty Git source"
                )));
            }
        }

        Ok(config)
    }
}

pub fn find_project_root(start: &Path) -> Result<PathBuf> {
    let mut candidate = start.to_path_buf();
    loop {
        if candidate.join("project.toml").is_file() {
            return Ok(candidate);
        }
        if !candidate.pop() {
            return Err(DevError(
                "no project.toml found in this directory or its parents".into(),
            ));
        }
    }
}

pub fn validate_component(kind: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(DevError(format!(
            "invalid {kind} name {value:?}; use 1-80 letters, numbers, hyphens, or underscores"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dev-{label}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn finds_and_loads_project_from_a_child_directory() {
        let root = temporary_directory("config");
        let child = root.join("some/deep/path");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            root.join("project.toml"),
            r#"
[project]
name = "sample"

[repositories]
frontend = "git@example.test:org/frontend.git"
backend = "../backend"
"#,
        )
        .unwrap();

        assert_eq!(find_project_root(&child).unwrap(), root);
        let config = ProjectConfig::load(&root).unwrap();
        assert_eq!(config.project.name, "sample");
        assert_eq!(config.repositories.len(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_like_task_names() {
        assert!(validate_component("task", "oauth-login_2").is_ok());
        assert!(validate_component("task", "../escape").is_err());
        assert!(validate_component("task", "has spaces").is_err());
    }
}
