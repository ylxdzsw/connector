use std::{sync::Arc, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{sync::Mutex, time};
use tokio_util::sync::CancellationToken;

use crate::screenshot::{self, Screenshot};

#[cfg(unix)]
#[path = "computer/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "computer/windows.rs"]
mod platform;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Capability {
    /// Desktop input prerequisites detected at connection time; calls can still fail.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Default for Capability {
    fn default() -> Self {
        Self::unavailable("client did not advertise desktop input")
    }
}

impl Capability {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            backend: None,
            reason: Some(reason.into()),
        }
    }
}

pub async fn capability() -> Capability {
    match platform::geometry().await {
        Ok(_) => Capability {
            available: true,
            backend: Some(
                if cfg!(windows) {
                    "windows-sendinput"
                } else {
                    "x11-xdotool"
                }
                .into(),
            ),
            reason: None,
        },
        Err(reason) => Capability::unavailable(reason),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComputerArgs {
    /// Up to 32 sequential actions. Always returns a final screenshot; [] observes only.
    #[schemars(length(max = 32))]
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Move {
        x: u32,
        y: u32,
    },
    Click {
        x: u32,
        y: u32,
        #[serde(default)]
        button: Button,
        #[serde(default = "one")]
        #[schemars(range(min = 1, max = 3))]
        count: u8,
        #[serde(default)]
        modifiers: Vec<Modifier>,
    },
    Scroll {
        x: u32,
        y: u32,
        direction: Direction,
        /// Wheel steps, 1..100. The application determines the distance scrolled.
        #[schemars(range(min = 1, max = 100))]
        amount: u32,
        #[serde(default)]
        modifiers: Vec<Modifier>,
    },
    Drag {
        from: Point,
        to: Point,
        #[serde(default)]
        button: Button,
        #[serde(default)]
        modifiers: Vec<Modifier>,
    },
    Key {
        /// One chord: A-Z, 0-9, F1-F12, CTRL, ALT, SHIFT, SUPER, ENTER, TAB, ESC,
        /// SPACE, BACKSPACE, DELETE, INSERT, HOME, END, PAGEUP, PAGEDOWN, UP, DOWN, LEFT, RIGHT.
        #[schemars(length(min = 1, max = 8))]
        keys: Vec<String>,
    },
    Type {
        /// Literal Unicode text, at most 8192 UTF-8 bytes. Does not modify the clipboard.
        #[schemars(length(max = 8192))]
        text: String,
    },
    Wait {
        /// Wait 0..5000 milliseconds before continuing.
        #[schemars(range(max = 5000))]
        ms: u64,
    },
}

fn one() -> u8 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

impl Modifier {
    fn name(self) -> &'static str {
        match self {
            Self::Ctrl => "CTRL",
            Self::Alt => "ALT",
            Self::Shift => "SHIFT",
            Self::Super => "SUPER",
        }
    }
}

pub(super) struct Key {
    #[cfg_attr(windows, allow(dead_code))]
    pub x11: String,
    #[cfg_attr(unix, allow(dead_code))]
    pub windows: u16,
}

fn key(name: &str) -> Result<Key, String> {
    let (x11, windows) = match name {
        "CTRL" => ("Control_L", 0x11),
        "ALT" => ("Alt_L", 0x12),
        "SHIFT" => ("Shift_L", 0x10),
        "SUPER" => ("Super_L", 0x5b),
        "ENTER" => ("Return", 0x0d),
        "TAB" => ("Tab", 0x09),
        "ESC" => ("Escape", 0x1b),
        "SPACE" => ("space", 0x20),
        "BACKSPACE" => ("BackSpace", 0x08),
        "DELETE" => ("Delete", 0x2e),
        "INSERT" => ("Insert", 0x2d),
        "HOME" => ("Home", 0x24),
        "END" => ("End", 0x23),
        "PAGEUP" => ("Prior", 0x21),
        "PAGEDOWN" => ("Next", 0x22),
        "UP" => ("Up", 0x26),
        "DOWN" => ("Down", 0x28),
        "LEFT" => ("Left", 0x25),
        "RIGHT" => ("Right", 0x27),
        _ if name.len() == 1
            && name
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) =>
        {
            return Ok(Key {
                x11: name.to_ascii_lowercase(),
                windows: u16::from(name.as_bytes()[0]),
            });
        }
        _ => {
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok())
                && (1..=12).contains(&n)
                && name == format!("F{n}")
            {
                return Ok(Key {
                    x11: name.into(),
                    windows: 0x6f + n,
                });
            }
            return Err("unsupported key name; use the names in the tool schema".into());
        }
    };
    Ok(Key {
        x11: x11.into(),
        windows,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub geometry: Geometry,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    fn point(self, point: Point) -> Result<(i32, i32), String> {
        if point.x >= self.width || point.y >= self.height {
            return Err(format!(
                "coordinate outside {}x{} screenshot",
                self.width, self.height
            ));
        }
        let x = u64::from(point.x) * u64::from(self.geometry.width) / u64::from(self.width);
        let y = u64::from(point.y) * u64::from(self.geometry.height) / u64::from(self.height);
        Ok((
            (i64::from(self.geometry.x) + x as i64) as i32,
            (i64::from(self.geometry.y) + y as i64) as i32,
        ))
    }
}

#[derive(Default)]
pub struct Desktop {
    frame: Option<Frame>,
}

pub struct Output {
    pub completed: usize,
    pub error: Option<String>,
    pub screenshot: Result<Screenshot, String>,
}

impl Desktop {
    pub fn invalidate(&mut self) {
        self.frame = None;
    }

    pub async fn capture(&mut self) -> Result<Screenshot, String> {
        self.frame = None;
        let before = platform::geometry().await.ok();
        let image = screenshot::capture().await?;
        if let Some(geometry) = before
            && image.source_width == geometry.width
            && image.source_height == geometry.height
            && platform::geometry().await.ok() == Some(geometry)
        {
            self.frame = Some(Frame {
                geometry,
                width: image.width,
                height: image.height,
            });
        }
        Ok(image)
    }

    async fn batch(&mut self, actions: Vec<Action>, cancel: &CancellationToken) -> Output {
        let mut completed = 0;
        let result = async {
            validate(&actions, self.frame)?;
            let deadline = time::Instant::now() + Duration::from_secs(30);
            for action in actions {
                if cancel.is_cancelled() {
                    return Err("batch cancelled".into());
                }
                if time::Instant::now() >= deadline {
                    return Err("batch exceeded 30 seconds".into());
                }
                if let Action::Wait { ms } = action {
                    tokio::select! {
                        _ = cancel.cancelled() => return Err("batch cancelled".into()),
                        _ = time::sleep(Duration::from_millis(ms)) => {}
                    }
                } else {
                    let geometry = platform::geometry().await?;
                    let frame = self.frame.unwrap_or(Frame {
                        geometry,
                        width: geometry.width,
                        height: geometry.height,
                    });
                    if frame.geometry != geometry {
                        return Err(
                            "desktop geometry changed; inspect the new screenshot before retrying"
                                .into(),
                        );
                    }
                    if cancel.is_cancelled() {
                        return Err("batch cancelled".into());
                    }
                    // Do not drop an action midway through key/button cleanup.
                    platform::execute(action, frame).await?;
                }
                completed += 1;
            }
            Ok(())
        }
        .await;
        let error = result.err().map(|error: String| format!("{error}; {completed} actions completed; the failing action may have partially executed. Do not blindly replay the batch."));
        let screenshot = self.capture().await;
        Output {
            completed,
            error,
            screenshot,
        }
    }
}

/// The worker retains the desktop lock while finishing input cleanup, even if
/// the MCP handler is dropped. Cancellation prevents subsequent actions.
pub async fn execute(
    desktop: Arc<Mutex<Desktop>>,
    args: ComputerArgs,
    cancel: CancellationToken,
) -> Result<Output, String> {
    let cancel = cancel.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    tokio::spawn(async move {
        let mut desktop = tokio::select! {
            _ = cancel.cancelled() => return Err("batch cancelled before execution".into()),
            desktop = desktop.lock() => desktop,
        };
        Ok(desktop.batch(args.actions, &cancel).await)
    })
    .await
    .map_err(|error| format!("desktop worker failed: {error}"))?
}

fn validate(actions: &[Action], frame: Option<Frame>) -> Result<(), String> {
    if actions.len() > 32 {
        return Err("a batch may contain at most 32 actions".into());
    }
    for action in actions {
        let mut points = Vec::new();
        let modifiers = match action {
            Action::Move { x, y } => {
                points.push(Point { x: *x, y: *y });
                None
            }
            Action::Click {
                x,
                y,
                count,
                modifiers,
                ..
            } => {
                if !(1..=3).contains(count) {
                    return Err("click count must be 1..3".into());
                }
                points.push(Point { x: *x, y: *y });
                Some(modifiers)
            }
            Action::Scroll {
                x,
                y,
                amount,
                modifiers,
                ..
            } => {
                if !(1..=100).contains(amount) {
                    return Err("scroll amount must be 1..100 wheel steps".into());
                }
                points.push(Point { x: *x, y: *y });
                Some(modifiers)
            }
            Action::Drag {
                from,
                to,
                modifiers,
                ..
            } => {
                points.extend([*from, *to]);
                Some(modifiers)
            }
            Action::Key { keys } => {
                if keys.is_empty() || keys.len() > 8 {
                    return Err("a key chord must contain 1..8 keys".into());
                }
                for (i, name) in keys.iter().enumerate() {
                    key(name)?;
                    if keys[..i].contains(name) {
                        return Err("duplicate key in chord".into());
                    }
                }
                None
            }
            Action::Type { text } => {
                if text.len() > 8192 || text.contains('\0') {
                    return Err("text must be at most 8192 bytes and contain no NUL".into());
                }
                None
            }
            Action::Wait { ms } => {
                if *ms > 5000 {
                    return Err("wait must be at most 5000 ms".into());
                }
                None
            }
        };
        if let Some(modifiers) = modifiers {
            for (i, modifier) in modifiers.iter().enumerate() {
                if modifiers[..i].contains(modifier) {
                    return Err("duplicate modifier".into());
                }
            }
        }
        for point in points {
            frame.ok_or("capture a screenshot first; no usable screenshot-to-desktop mapping is available")?.point(point)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_scaled_screenshot_with_negative_desktop_origin() {
        let frame = Frame {
            geometry: Geometry {
                x: -1920,
                y: -100,
                width: 3840,
                height: 2160,
            },
            width: 1920,
            height: 1080,
        };
        assert_eq!(frame.point(Point { x: 100, y: 200 }).unwrap(), (-1720, 300));
        assert!(frame.point(Point { x: 1920, y: 0 }).is_err());
        assert!(validate(&[Action::Move { x: 1, y: 1 }], None).is_err());
        assert!(
            validate(
                &[Action::Key {
                    keys: vec!["CTRL".into(), "A".into()]
                }],
                None
            )
            .is_ok()
        );
        assert!(
            validate(
                &[
                    Action::Type { text: "ok".into() },
                    Action::Wait { ms: 5001 }
                ],
                None
            )
            .is_err()
        );
    }
}
