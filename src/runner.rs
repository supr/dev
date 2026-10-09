use crate::{DevError, Result};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn output(&self, current_dir: &Path, program: &str, args: &[String]) -> Result<CommandOutput>;

    fn interactive(&self, current_dir: &Path, program: &str, args: &[String]) -> Result<bool>;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, current_dir: &Path, program: &str, args: &[String]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .current_dir(current_dir)
            .output()
            .map_err(|error| DevError(format!("could not run {program}: {error}")))?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn interactive(&self, current_dir: &Path, program: &str, args: &[String]) -> Result<bool> {
        let status = Command::new(program)
            .args(args)
            .current_dir(current_dir)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| DevError(format!("could not run {program}: {error}")))?;
        Ok(status.success())
    }
}
