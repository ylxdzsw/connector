use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{process::Command, time};

use crate::crypto::random_token;

const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Screenshot {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
    pub backend: &'static str,
}

pub async fn capture() -> Result<Screenshot, String> {
    let directory = CaptureDirectory::new()?;
    let script = directory.0.join("capture.ps1");
    let output = directory.0.join("capture.jpg");
    tokio::fs::write(&script, CAPTURE_SCRIPT)
        .await
        .map_err(|error| {
            format!("screenshot unavailable: could not write capture script: {error}")
        })?;

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
        .arg(&script)
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("screenshot unavailable: could not start pwsh: {error}"))?;
    let status = time::timeout(CAPTURE_TIMEOUT, child.wait())
        .await
        .map_err(|_| "screenshot unavailable: capture timed out".to_owned())?
        .map_err(|error| format!("screenshot unavailable: could not wait for pwsh: {error}"))?;
    if !status.success() {
        return Err(format!(
            "screenshot unavailable: pwsh capture exited with {}",
            status.code().unwrap_or(1)
        ));
    }

    let data = tokio::fs::read(&output)
        .await
        .map_err(|error| format!("screenshot unavailable: could not read capture: {error}"))?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err("screenshot unavailable: captured image exceeds 8 MiB".into());
    }
    if data.len() < 4 || !data.starts_with(b"\xff\xd8\xff") || !data.ends_with(b"\xff\xd9") {
        return Err("screenshot unavailable: pwsh produced an invalid JPEG".into());
    }
    Ok(Screenshot {
        data,
        mime_type: "image/jpeg",
        backend: "pwsh-system-drawing",
    })
}

struct CaptureDirectory(PathBuf);

impl CaptureDirectory {
    fn new() -> Result<Self, String> {
        for _ in 0..8 {
            let path =
                std::env::temp_dir().join(format!("connector-screenshot-{}", random_token()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "screenshot unavailable: could not create temporary directory: {error}"
                    ));
                }
            }
        }
        Err("screenshot unavailable: could not create a unique temporary directory".into())
    }
}

impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const CAPTURE_SCRIPT: &str = r#"
param([Parameter(Mandatory=$true)][string]$OutputPath)
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
$bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {
    $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bitmap.Save($OutputPath, [System.Drawing.Imaging.ImageFormat]::Jpeg)
} finally {
    $graphics.Dispose()
    $bitmap.Dispose()
}
"#;
