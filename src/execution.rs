use std::{path::PathBuf, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time};

pub const DEFAULT_TIMEOUT: u64 = 60;
pub const MAX_TIMEOUT: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BashArgs {
    #[schemars(description = "Bash command to execute in a fresh shell")]
    pub command: String,
    #[schemars(description = "Working directory for the command")]
    pub cwd: Option<PathBuf>,
    #[schemars(description = "Maximum run time in seconds (default 60, maximum 3600)")]
    pub timeout: Option<u64>,
    #[schemars(description = "Literal bytes supplied to standard input as UTF-8 text")]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct BashOutput {
    pub output: String,
    pub exit_code: i32,
}

pub async fn run_bash(args: BashArgs) -> Result<BashOutput, String> {
    let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT);
    if timeout == 0 || timeout > MAX_TIMEOUT {
        return Err(format!(
            "timeout must be between 1 and {MAX_TIMEOUT} seconds"
        ));
    }
    if let Some(cwd) = &args.cwd
        && !cwd.is_dir()
    {
        return Err(format!(
            "working directory does not exist: {}",
            cwd.display()
        ));
    }

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(format!("exec 2>&1\n{}", args.command))
        .stdin(if args.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = args.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start bash: {error}"))?;
    if let Some(input) = args.stdin {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "could not open command stdin".to_owned())?;
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
    }
    let pid = child.id();
    let mut process_group = ProcessGroupGuard(pid);
    match time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
        Ok(Ok(result)) => {
            process_group.0 = None;
            Ok(BashOutput {
                output: String::from_utf8_lossy(&result.stdout).into_owned(),
                exit_code: result
                    .status
                    .code()
                    .unwrap_or(128 + signal(&result.status).unwrap_or(0)),
            })
        }
        Ok(Err(error)) => Err(format!("could not wait for bash: {error}")),
        Err(_) => Err(format!("command timed out after {timeout} seconds")),
    }
}

struct ProcessGroupGuard(Option<u32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

#[cfg(unix)]
fn signal(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn signal(_: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execution_is_fresh_and_combines_output() {
        let result = run_bash(BashArgs {
            command: "printf out; printf err >&2; exit 7".into(),
            cwd: None,
            timeout: None,
            stdin: None,
        })
        .await
        .unwrap();
        assert_eq!(
            result,
            BashOutput {
                output: "outerr".into(),
                exit_code: 7
            }
        );
    }

    #[tokio::test]
    async fn stdin_is_literal() {
        let result = run_bash(BashArgs {
            command: "cat".into(),
            cwd: None,
            timeout: None,
            stdin: Some("$HOME\n".into()),
        })
        .await
        .unwrap();
        assert_eq!(result.output, "$HOME\n");
    }
}
