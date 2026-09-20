use std::{process::Stdio, time::Duration};

use tokio::{io::AsyncWriteExt, process::Command, time};

use super::{Action, Button, Direction, Frame, Geometry, Point, key};

pub async fn geometry() -> Result<Geometry, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
    {
        return Err("Wayland input is not supported".into());
    }
    if !std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
        return Err("no X11 display is configured".into());
    }
    let output = command(&["getdisplaygeometry".into()], None).await?;
    let dimensions: Vec<u32> = String::from_utf8_lossy(&output)
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    match dimensions.as_slice() {
        [width, height]
            if *width > 0
                && *height > 0
                && *width <= i32::MAX as u32
                && *height <= i32::MAX as u32 =>
        {
            Ok(Geometry {
                x: 0,
                y: 0,
                width: *width,
                height: *height,
            })
        }
        _ => Err("xdotool did not report a valid X11 display size".into()),
    }
}

async fn command(args: &[String], stdin: Option<&str>) -> Result<Vec<u8>, String> {
    let mut child = Command::new("xdotool")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "could not start xdotool; install it and run the client in an X11 session")?;
    time::timeout(Duration::from_secs(5), async {
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            pipe.write_all(text.as_bytes())
                .await
                .map_err(|_| "xdotool input write failed")?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|_| "could not wait for xdotool")?;
        if !output.status.success() {
            return Err("xdotool failed; check display access and XTEST support".into());
        }
        Ok(output.stdout)
    })
    .await
    .map_err(|_| "xdotool timed out".to_owned())?
}

fn button(button: Button) -> &'static str {
    match button {
        Button::Left => "1",
        Button::Middle => "2",
        Button::Right => "3",
    }
}

async fn move_to(frame: Frame, point: Point) -> Result<(), String> {
    let (x, y) = frame.point(point)?;
    command(&["mousemove".into(), x.to_string(), y.to_string()], None).await?;
    Ok(())
}

#[derive(Default)]
struct Held {
    keys: Vec<String>,
    button: Option<&'static str>,
}

impl Held {
    async fn press(&mut self, name: &str) -> Result<(), String> {
        let name = key(name)?.x11;
        self.keys.push(name.clone());
        command(&["keydown".into(), name], None).await?;
        Ok(())
    }

    async fn release(self) -> Result<(), String> {
        let mut args = Vec::new();
        if let Some(button) = self.button {
            args.extend(["mouseup".into(), button.into()]);
        }
        for name in self.keys.into_iter().rev() {
            args.extend(["keyup".into(), name]);
        }
        if !args.is_empty() && command(&args, None).await.is_err() {
            // A failed command chain may have stopped before later releases.
            let mut failed = false;
            for release in args.chunks(2) {
                failed |= command(release, None).await.is_err();
            }
            if failed {
                return Err(
                    "could not release desktop input; inspect keyboard/button state".into(),
                );
            }
        }
        Ok(())
    }
}

pub async fn execute(action: Action, frame: Frame) -> Result<(), String> {
    let mut held = Held::default();
    let result = async {
        let modifiers = match &action {
            Action::Click { modifiers, .. }
            | Action::Scroll { modifiers, .. }
            | Action::Drag { modifiers, .. } => modifiers.as_slice(),
            _ => &[],
        };
        for modifier in modifiers {
            held.press(modifier.name()).await?;
        }
        match action {
            Action::Move { x, y } => move_to(frame, Point { x, y }).await?,
            Action::Click {
                x,
                y,
                button: b,
                count,
                ..
            } => {
                move_to(frame, Point { x, y }).await?;
                held.button = Some(button(b));
                command(
                    &[
                        "click".into(),
                        "--repeat".into(),
                        count.to_string(),
                        "--delay".into(),
                        "80".into(),
                        button(b).into(),
                    ],
                    None,
                )
                .await?;
            }
            Action::Scroll {
                x,
                y,
                direction,
                amount,
                ..
            } => {
                move_to(frame, Point { x, y }).await?;
                let b = match direction {
                    Direction::Up => "4",
                    Direction::Down => "5",
                    Direction::Left => "6",
                    Direction::Right => "7",
                };
                held.button = Some(b);
                command(
                    &[
                        "click".into(),
                        "--repeat".into(),
                        amount.to_string(),
                        "--delay".into(),
                        "10".into(),
                        b.into(),
                    ],
                    None,
                )
                .await?;
            }
            Action::Drag {
                from,
                to,
                button: b,
                ..
            } => {
                move_to(frame, from).await?;
                held.button = Some(button(b));
                command(&["mousedown".into(), button(b).into()], None).await?;
                let (x0, y0) = frame.point(from)?;
                let (x1, y1) = frame.point(to)?;
                let mut args = Vec::new();
                for step in 1..=10 {
                    args.extend([
                        "mousemove".into(),
                        (i64::from(x0) + (i64::from(x1) - i64::from(x0)) * step / 10).to_string(),
                        (i64::from(y0) + (i64::from(y1) - i64::from(y0)) * step / 10).to_string(),
                        "sleep".into(),
                        "0.02".into(),
                    ]);
                }
                command(&args, None).await?;
            }
            Action::Key { keys } => {
                for name in keys {
                    held.press(&name).await?;
                }
            }
            Action::Type { text } => {
                command(
                    &[
                        "type".into(),
                        "--delay".into(),
                        "0".into(),
                        "--file".into(),
                        "-".into(),
                    ],
                    Some(&text),
                )
                .await?;
            }
            Action::Wait { .. } => unreachable!("wait handled by batch executor"),
        }
        Ok(())
    }
    .await;
    let cleanup = held.release().await;
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}
