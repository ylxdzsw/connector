use std::{
    ffi::{OsStr, OsString},
    fs::DirBuilder,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{io::AsyncReadExt, process::Command, time};

use crate::crypto::random_token;

const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Screenshot {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
    pub backend: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionKind {
    Wayland,
    X11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Grim,
    Spectacle,
    GnomeScreenshot,
    Flameshot,
    Maim,
    Scrot,
    Magick,
    Import,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Grim => "grim",
            Self::Spectacle => "spectacle",
            Self::GnomeScreenshot => "gnome-screenshot",
            Self::Flameshot => "flameshot",
            Self::Maim => "maim",
            Self::Scrot => "scrot",
            Self::Magick => "magick",
            Self::Import => "import",
        }
    }

    fn output_name(self) -> &'static str {
        match self {
            Self::GnomeScreenshot | Self::Flameshot => "capture.png",
            _ => "capture.jpg",
        }
    }

    fn arguments(self, output: &Path) -> Vec<OsString> {
        let output = output.as_os_str().to_owned();
        match self {
            Self::Grim => os_args(["-t", "jpeg", "-q", "85"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::Spectacle => os_args(["-f", "-b", "-n", "-o"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::GnomeScreenshot => os_args(["-f"]).into_iter().chain([output]).collect(),
            Self::Flameshot => os_args(["full", "-p"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::Maim => os_args(["--format=jpg", "--quality=8"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::Scrot => os_args(["-q", "85", "-p"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::Magick => os_args(["import", "-window", "root", "-quality", "85"])
                .into_iter()
                .chain([output])
                .collect(),
            Self::Import => os_args(["-window", "root", "-quality", "85"])
                .into_iter()
                .chain([output])
                .collect(),
        }
    }
}

pub async fn capture() -> Result<Screenshot, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    let x11 = std::env::var_os("DISPLAY");
    let Some(kind) = session_kind(wayland.as_deref(), x11.as_deref()) else {
        return Err("screenshot unavailable: no graphical session was detected".into());
    };
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    capture_with(kind, &desktop, None).await
}

fn session_kind(wayland: Option<&OsStr>, x11: Option<&OsStr>) -> Option<SessionKind> {
    if wayland.is_some_and(|value| !value.is_empty()) {
        Some(SessionKind::Wayland)
    } else if x11.is_some_and(|value| !value.is_empty()) {
        Some(SessionKind::X11)
    } else {
        None
    }
}

async fn capture_with(
    kind: SessionKind,
    desktop: &str,
    path: Option<&OsStr>,
) -> Result<Screenshot, String> {
    let directory = CaptureDirectory::new()?;
    let backends = backends(kind, desktop);
    let deadline = time::Instant::now() + TOTAL_TIMEOUT;

    for backend in &backends {
        let output = directory.0.join(backend.output_name());
        let _ = tokio::fs::remove_file(&output).await;
        let remaining = deadline.saturating_duration_since(time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if !run_command(
            backend.name(),
            backend.arguments(&output),
            remaining.min(COMMAND_TIMEOUT),
            path,
        )
        .await
        {
            continue;
        }
        if let Some(image) = read_image(&output, MAX_IMAGE_BYTES).await {
            return Ok(Screenshot {
                data: image.0,
                mime_type: image.1,
                backend: backend.name(),
            });
        }
        if valid_oversized_image(&output).await
            && let Some(image) = reduce_image(&directory.0, &output, deadline, path).await
        {
            return Ok(Screenshot {
                data: image.0,
                mime_type: image.1,
                backend: backend.name(),
            });
        }
    }

    let names = backends
        .iter()
        .map(|backend| backend.name())
        .collect::<Vec<_>>()
        .join(", ");
    let session = match kind {
        SessionKind::Wayland => "Wayland",
        SessionKind::X11 => "X11",
    };
    Err(format!(
        "screenshot unavailable: {session} session detected; tried {names}"
    ))
}

fn backends(kind: SessionKind, desktop: &str) -> Vec<Backend> {
    let desktop = desktop.to_ascii_lowercase();
    let kde = desktop.contains("kde") || desktop.contains("plasma");
    let gnome = desktop.contains("gnome") || desktop.contains("cinnamon");
    let mut result = Vec::new();
    if kde {
        push_unique(&mut result, Backend::Spectacle);
    }
    if gnome {
        push_unique(&mut result, Backend::GnomeScreenshot);
    }
    match kind {
        SessionKind::Wayland => {
            push_unique(&mut result, Backend::Grim);
            push_unique(&mut result, Backend::Spectacle);
            push_unique(&mut result, Backend::GnomeScreenshot);
            push_unique(&mut result, Backend::Flameshot);
        }
        SessionKind::X11 => {
            push_unique(&mut result, Backend::Maim);
            push_unique(&mut result, Backend::Scrot);
            push_unique(&mut result, Backend::Magick);
            push_unique(&mut result, Backend::Import);
            push_unique(&mut result, Backend::Spectacle);
            push_unique(&mut result, Backend::GnomeScreenshot);
            push_unique(&mut result, Backend::Flameshot);
        }
    }
    result
}

fn push_unique(backends: &mut Vec<Backend>, backend: Backend) {
    if !backends.contains(&backend) {
        backends.push(backend);
    }
}

async fn run_command(
    program: &str,
    arguments: Vec<OsString>,
    timeout: Duration,
    path: Option<&OsStr>,
) -> bool {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let pid = child.id();
    let mut process_group = ProcessGroupGuard(pid);
    match time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            process_group.0 = None;
            status.success()
        }
        _ => false,
    }
}

async fn read_image(path: &Path, limit: u64) -> Option<(Vec<u8>, &'static str)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if metadata.len() == 0 || metadata.len() > limit {
        return None;
    }
    let data = tokio::fs::read(path).await.ok()?;
    let mime_type = image_mime_type(&data)?;
    Some((data, mime_type))
}

async fn valid_oversized_image(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if metadata.len() <= MAX_IMAGE_BYTES || metadata.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut header = [0; 8];
    let Ok(read) = file.read(&mut header).await else {
        return false;
    };
    image_header_mime_type(&header[..read]).is_some()
}

async fn reduce_image(
    directory: &Path,
    source: &Path,
    deadline: time::Instant,
    path: Option<&OsStr>,
) -> Option<(Vec<u8>, &'static str)> {
    let output = directory.join("reduced.jpg");
    let arguments = || {
        Vec::from([
            source.as_os_str().to_owned(),
            OsString::from("-strip"),
            OsString::from("-resize"),
            OsString::from("1920x1920>"),
            OsString::from("-quality"),
            OsString::from("85"),
            output.as_os_str().to_owned(),
        ])
    };
    for program in ["magick", "convert"] {
        let _ = tokio::fs::remove_file(&output).await;
        let remaining = deadline.saturating_duration_since(time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        if run_command(program, arguments(), remaining.min(COMMAND_TIMEOUT), path).await
            && let Some(image) = read_image(&output, MAX_IMAGE_BYTES).await
        {
            return Some(image);
        }
    }
    None
}

fn image_mime_type(data: &[u8]) -> Option<&'static str> {
    let mime_type = image_header_mime_type(data)?;
    if mime_type == "image/png"
        && data.len() >= 20
        && data[data.len() - 8..data.len() - 4] == *b"IEND"
    {
        Some("image/png")
    } else if mime_type == "image/jpeg" && data.ends_with(b"\xff\xd9") {
        Some("image/jpeg")
    } else {
        None
    }
}

fn image_header_mime_type(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else {
        None
    }
}

fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

struct CaptureDirectory(PathBuf);

impl CaptureDirectory {
    fn new() -> Result<Self, String> {
        for _ in 0..8 {
            let path =
                std::env::temp_dir().join(format!("connector-screenshot-{}", random_token()));
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
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

struct ProcessGroupGuard(Option<u32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn chooses_desktop_native_wayland_backends_first() {
        assert_eq!(
            backends(SessionKind::Wayland, "KDE"),
            vec![
                Backend::Spectacle,
                Backend::Grim,
                Backend::GnomeScreenshot,
                Backend::Flameshot,
            ]
        );
        assert_eq!(
            backends(SessionKind::Wayland, "GNOME"),
            vec![
                Backend::GnomeScreenshot,
                Backend::Grim,
                Backend::Spectacle,
                Backend::Flameshot,
            ]
        );
    }

    #[test]
    fn never_uses_x11_backends_for_wayland() {
        let backends = backends(SessionKind::Wayland, "unknown");
        assert!(!backends.contains(&Backend::Maim));
        assert!(!backends.contains(&Backend::Scrot));
        assert!(!backends.contains(&Backend::Import));
    }

    #[test]
    fn wayland_takes_precedence_over_xwayland() {
        assert_eq!(
            session_kind(Some(OsStr::new("wayland-1")), Some(OsStr::new(":0"))),
            Some(SessionKind::Wayland)
        );
        assert_eq!(
            session_kind(None, Some(OsStr::new(":0"))),
            Some(SessionKind::X11)
        );
        assert_eq!(session_kind(None, None), None);
    }

    #[test]
    fn recognizes_supported_images_by_content() {
        assert_eq!(
            image_mime_type(b"\x89PNG\r\n\x1a\ncontentsIENDxxxx"),
            Some("image/png")
        );
        assert_eq!(
            image_mime_type(b"\xff\xd8\xffcontents\xff\xd9"),
            Some("image/jpeg")
        );
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\ntruncated"), None);
        assert_eq!(image_mime_type(b"\xff\xd8\xfftruncated"), None);
        assert_eq!(image_mime_type(b"GIF89a"), None);
    }

    #[tokio::test]
    async fn falls_back_to_the_next_available_backend() {
        let directory = tempfile::tempdir().unwrap();
        write_script(directory.path().join("grim"), "exit 1");
        write_script(
            directory.path().join("spectacle"),
            r#"for output do :; done
printf '\211PNG\r\n\032\ncontentsIENDxxxx' > "$output""#,
        );

        let screenshot = capture_with(
            SessionKind::Wayland,
            "unknown",
            Some(directory.path().as_os_str()),
        )
        .await
        .unwrap();
        assert_eq!(screenshot.backend, "spectacle");
        assert_eq!(screenshot.mime_type, "image/png");
    }

    fn write_script(path: PathBuf, body: &str) {
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
