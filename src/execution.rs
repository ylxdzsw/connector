use std::{path::PathBuf, process::Stdio, time::Duration};

#[cfg(windows)]
use std::io::Read;

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time};

pub const DEFAULT_TIMEOUT: u64 = 60;
pub const MAX_TIMEOUT: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    #[schemars(description = "Command text to execute in a fresh shell")]
    pub command: String,
    #[schemars(description = "Working directory for the command")]
    pub cwd: Option<PathBuf>,
    #[schemars(description = "Maximum run time in seconds (default 60, maximum 3600)")]
    pub timeout: Option<u64>,
    #[schemars(description = "Literal bytes supplied to standard input as UTF-8 text")]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct RunOutput {
    pub output: String,
    pub exit_code: i32,
}

pub async fn run_shell(args: RunArgs) -> Result<RunOutput, String> {
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

    run_platform(args, timeout).await
}

#[cfg(unix)]
async fn run_platform(args: RunArgs, timeout: u64) -> Result<RunOutput, String> {
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
            Ok(RunOutput {
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

#[cfg(windows)]
async fn run_platform(args: RunArgs, timeout: u64) -> Result<RunOutput, String> {
    let script = format!(
        "$global:LASTEXITCODE = 0\n& {{\n{}\n}} *>&1\n$connectorSucceeded = $?\nif (-not $connectorSucceeded) {{\n    if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n    exit 1\n}}\nexit 0\n",
        args.command
    );
    let script = CommandScript::new(&script)?;
    let (mut output_reader, output_writer) = std::io::pipe()
        .map_err(|error| format!("could not create command output pipe: {error}"))?;
    let mut command = Command::new("pwsh");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script.0)
        .stdin(if args.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(output_writer.try_clone().map_err(|error| {
            format!("could not clone command output pipe: {error}")
        })?))
        .stderr(Stdio::from(output_writer))
        .kill_on_drop(true);
    if let Some(cwd) = args.cwd {
        command.current_dir(cwd);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start pwsh: {error}"))?;
    drop(command);
    let output_task = tokio::task::spawn_blocking(move || {
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).map(|_| output)
    });
    if let Some(input) = args.stdin {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "could not open command stdin".to_owned())?;
        tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
    }
    match time::timeout(Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => {
            let output = output_task
                .await
                .map_err(|error| format!("could not join command output task: {error}"))?
                .map_err(|error| format!("could not read command output: {error}"))?;
            Ok(RunOutput {
                output: String::from_utf8_lossy(&output).into_owned(),
                exit_code: status.code().unwrap_or(1),
            })
        }
        Ok(Err(error)) => Err(format!("could not wait for pwsh: {error}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = output_task.await;
            Err(format!("command timed out after {timeout} seconds"))
        }
    }
}

#[cfg(windows)]
struct CommandScript(PathBuf);

#[cfg(windows)]
impl CommandScript {
    fn new(content: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "connector-command-{}.ps1",
            crate::crypto::random_token()
        ));
        std::fs::write(&path, content)
            .map_err(|error| format!("could not write temporary PowerShell script: {error}"))?;
        Ok(Self(path))
    }
}

#[cfg(windows)]
impl Drop for CommandScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execution_is_fresh_and_combines_output() {
        let result = run_shell(RunArgs {
            command: "printf out; printf err >&2; exit 7".into(),
            cwd: None,
            timeout: None,
            stdin: None,
        })
        .await
        .unwrap();
        assert_eq!(
            result,
            RunOutput {
                output: "outerr".into(),
                exit_code: 7
            }
        );
    }

    #[tokio::test]
    async fn stdin_is_literal() {
        let result = run_shell(RunArgs {
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[tokio::test]
    async fn execution_combines_output_and_returns_exit_code() {
        let result = run_shell(RunArgs {
            command: "Write-Output out; Write-Error err; exit 7".into(),
            cwd: None,
            timeout: None,
            stdin: None,
        })
        .await
        .unwrap();
        assert!(result.output.contains("out"));
        assert!(result.output.contains("err"));
        assert_eq!(result.exit_code, 7);
    }

    #[tokio::test]
    async fn stdin_is_literal() {
        let result = run_shell(RunArgs {
            command: "[Console]::Write([Console]::In.ReadToEnd())".into(),
            cwd: None,
            timeout: None,
            stdin: Some("$HOME\r\n".into()),
        })
        .await
        .unwrap();
        assert_eq!(result.output, "$HOME\r\n");
    }
}
