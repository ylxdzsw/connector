use std::{path::PathBuf, process::Stdio, time::Duration};

#[cfg(windows)]
use std::io::Read;

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command, time};

pub const DEFAULT_TIMEOUT: u64 = 60;
pub const MAX_TIMEOUT: u64 = 3600;
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

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

#[derive(Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl OutputBuffer {
    fn append(&mut self, bytes: &[u8]) {
        let remaining = MAX_OUTPUT_BYTES - self.bytes.len();
        self.exceeded |= bytes.len() > remaining;
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    fn finish(self, exit_code: i32) -> Result<RunOutput, String> {
        if self.exceeded {
            return Err(format!(
                "combined command output exceeded 8 MiB and was discarded (exit code {exit_code}); the command ran and may have changed state; inspect its effects before retrying"
            ));
        }
        Ok(RunOutput {
            output: String::from_utf8_lossy(&self.bytes).into_owned(),
            exit_code,
        })
    }
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
    let mut stdout = child.stdout.take().expect("piped stdout");
    let drain = async {
        use tokio::io::AsyncReadExt;
        let mut output = OutputBuffer::default();
        let mut chunk = [0; 8192];
        loop {
            let count = stdout.read(&mut chunk).await?;
            if count == 0 {
                return Ok::<_, std::io::Error>(output);
            }
            output.append(&chunk[..count]);
        }
    };
    let collect = async { tokio::try_join!(child.wait(), drain) };
    match time::timeout(Duration::from_secs(timeout), collect).await {
        Ok(Ok((status, output))) => {
            process_group.0 = None;
            output.finish(status.code().unwrap_or(128 + signal(&status).unwrap_or(0)))
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
        let mut output = OutputBuffer::default();
        let mut chunk = [0; 8192];
        loop {
            let count = output_reader.read(&mut chunk)?;
            if count == 0 {
                return Ok::<_, std::io::Error>(output);
            }
            output.append(&chunk[..count]);
        }
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
            output.finish(status.code().unwrap_or(1))
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

    #[tokio::test]
    async fn excessive_output_is_drained_and_reported_as_an_error() {
        let error = run_shell(RunArgs {
            command: format!("head -c {} /dev/zero; exit 7", MAX_OUTPUT_BYTES + 1),
            cwd: None,
            timeout: Some(10),
            stdin: None,
        })
        .await
        .unwrap_err();
        assert!(error.contains("exceeded 8 MiB"));
        assert!(error.contains("exit code 7"));
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
