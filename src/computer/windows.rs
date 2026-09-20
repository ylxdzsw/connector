use std::{ffi::c_void, mem::size_of, thread, time::Duration};

use super::{Action, Button, Direction, Frame, Geometry, Modifier, Point};

const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;

const DESKTOP_READOBJECTS: u32 = 0x0001;
const DESKTOP_ENUMERATE: u32 = 0x0040;
const UOI_NAME: i32 = 2;

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;

const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const WHEEL_DELTA: i32 = 120;
const DRAG_STEPS: i64 = 20;

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputData {
    mouse: MouseInput,
    keyboard: KeyboardInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    input_type: u32,
    data: InputData,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetLastError"]
    fn get_last_error() -> u32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetSystemMetrics(index: i32) -> i32;
    fn OpenInputDesktop(flags: u32, inherit: i32, desired_access: u32) -> *mut c_void;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
    fn GetUserObjectInformationW(
        object: *mut c_void,
        index: i32,
        information: *mut c_void,
        length: u32,
        returned_length: *mut u32,
    ) -> i32;
    fn SetThreadDpiAwarenessContext(context: isize) -> isize;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
}

pub async fn geometry() -> Result<Geometry, String> {
    tokio::task::spawn_blocking(|| {
        let _dpi = DpiScope::enter()?;
        let _desktop = InputDesktop::open()?;
        query_geometry()
    })
    .await
    .map_err(|error| format!("desktop geometry task failed: {error}"))?
}

pub async fn execute(action: Action, frame: Frame) -> Result<(), String> {
    tokio::task::spawn_blocking(move || execute_blocking(action, frame))
        .await
        .map_err(|error| format!("desktop input task failed: {error}"))?
}

fn execute_blocking(action: Action, frame: Frame) -> Result<(), String> {
    let _dpi = DpiScope::enter()?;
    let _desktop = InputDesktop::open()?;
    let actual = query_geometry()?;
    if actual != frame.geometry {
        return Err("desktop geometry changed; refusing to use stale coordinates".into());
    }

    let mut pressed = InputGuard::default();
    let result = (|| {
        match action {
            Action::Move { x, y } => move_to(frame.geometry, frame.point(Point { x, y })?)?,
            Action::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => {
                press_modifiers(&mut pressed, &modifiers)?;
                move_to(frame.geometry, frame.point(Point { x, y })?)?;
                click(&mut pressed, button, count)?;
            }
            Action::Scroll {
                x,
                y,
                direction,
                amount,
                modifiers,
            } => {
                press_modifiers(&mut pressed, &modifiers)?;
                move_to(frame.geometry, frame.point(Point { x, y })?)?;
                scroll(direction, amount)?;
            }
            Action::Drag {
                from,
                to,
                button,
                modifiers,
            } => {
                press_modifiers(&mut pressed, &modifiers)?;
                drag(
                    &mut pressed,
                    frame.geometry,
                    frame.point(from)?,
                    frame.point(to)?,
                    button,
                )?;
            }
            Action::Key { keys } => {
                for name in keys {
                    pressed.key_down(super::key(&name)?.windows)?;
                }
            }
            Action::Type { text } => type_text(&text, &mut pressed)?,
            Action::Wait { .. } => unreachable!("wait handled by batch executor"),
        }
        Ok(())
    })();
    let cleanup = pressed.release_all();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(format!("{error}; input release also failed: {cleanup}")),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn query_geometry() -> Result<Geometry, String> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(win_error(
            "GetSystemMetrics returned an empty virtual desktop",
        ));
    }
    Ok(Geometry {
        x,
        y,
        width: width as u32,
        height: height as u32,
    })
}

fn move_to(geometry: Geometry, point: (i32, i32)) -> Result<(), String> {
    let (x, y) = absolute_coordinates(geometry, point)?;
    send(Input {
        input_type: INPUT_MOUSE,
        data: InputData {
            mouse: MouseInput {
                dx: x,
                dy: y,
                mouse_data: 0,
                flags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                extra_info: 0,
            },
        },
    })
}

fn absolute_coordinates(geometry: Geometry, point: (i32, i32)) -> Result<(i32, i32), String> {
    let right = i64::from(geometry.x) + i64::from(geometry.width) - 1;
    let bottom = i64::from(geometry.y) + i64::from(geometry.height) - 1;
    let x = i64::from(point.0);
    let y = i64::from(point.1);
    if x < i64::from(geometry.x) || x > right || y < i64::from(geometry.y) || y > bottom {
        return Err("desktop coordinate is outside the virtual desktop".into());
    }
    // Target pixel centers: Windows maps the 65536-unit range onto the desktop.
    let x = ((x - i64::from(geometry.x)) * 65_536 + 32_768) / i64::from(geometry.width);
    let y = ((y - i64::from(geometry.y)) * 65_536 + 32_768) / i64::from(geometry.height);
    let (x, y) = (x.min(65_535) as i32, y.min(65_535) as i32);
    Ok((x, y))
}

fn press_modifiers(pressed: &mut InputGuard, modifiers: &[Modifier]) -> Result<(), String> {
    for modifier in modifiers {
        pressed.key_down(super::key(modifier.name())?.windows)?;
    }
    Ok(())
}

fn click(pressed: &mut InputGuard, button: Button, count: u8) -> Result<(), String> {
    for index in 0..count {
        pressed.button_down(button)?;
        pressed.release_last()?;
        if index + 1 != count {
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

fn scroll(direction: Direction, amount: u32) -> Result<(), String> {
    let (flags, delta) = match direction {
        Direction::Up => (
            MOUSEEVENTF_WHEEL,
            i64::from(amount) * i64::from(WHEEL_DELTA),
        ),
        Direction::Down => (
            MOUSEEVENTF_WHEEL,
            -i64::from(amount) * i64::from(WHEEL_DELTA),
        ),
        Direction::Left => (
            MOUSEEVENTF_HWHEEL,
            -i64::from(amount) * i64::from(WHEEL_DELTA),
        ),
        Direction::Right => (
            MOUSEEVENTF_HWHEEL,
            i64::from(amount) * i64::from(WHEEL_DELTA),
        ),
    };
    send(Input {
        input_type: INPUT_MOUSE,
        data: InputData {
            mouse: MouseInput {
                dx: 0,
                dy: 0,
                mouse_data: delta as u32,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    })
}

fn drag(
    pressed: &mut InputGuard,
    geometry: Geometry,
    from: (i32, i32),
    to: (i32, i32),
    button: Button,
) -> Result<(), String> {
    move_to(geometry, from)?;
    pressed.button_down(button)?;
    let from_x = i64::from(from.0);
    let from_y = i64::from(from.1);
    let delta_x = i64::from(to.0) - from_x;
    let delta_y = i64::from(to.1) - from_y;
    for step in 1..=DRAG_STEPS {
        thread::sleep(Duration::from_millis(200 / DRAG_STEPS as u64));
        let point = (
            (from_x + delta_x * step / DRAG_STEPS) as i32,
            (from_y + delta_y * step / DRAG_STEPS) as i32,
        );
        move_to(geometry, point)?;
    }
    pressed.release_last()
}

fn type_text(text: &str, pressed: &mut InputGuard) -> Result<(), String> {
    for unit in text.encode_utf16() {
        pressed.unicode_down(unit)?;
        pressed.release_last()?;
    }
    Ok(())
}

fn keyboard_input(virtual_key: u16, scan_code: u16, flags: u32) -> Input {
    Input {
        input_type: INPUT_KEYBOARD,
        data: InputData {
            keyboard: KeyboardInput {
                virtual_key,
                scan_code,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn send(input: Input) -> Result<(), String> {
    let inserted = unsafe { SendInput(1, &input, size_of::<Input>() as i32) };
    if inserted != 1 {
        return Err(format!(
            "SendInput inserted {inserted}/1 input events (Windows error {})",
            unsafe { get_last_error() }
        ));
    }
    Ok(())
}

fn key_input(key: u16, key_up: bool) -> Input {
    let extended = matches!(key, 0x21..=0x28 | 0x2d | 0x2e | 0x5b);
    keyboard_input(
        key,
        0,
        (if key_up { KEYEVENTF_KEYUP } else { 0 }) | u32::from(extended),
    )
}

fn mouse_button_input(button: Button, button_up: bool) -> Input {
    let flags = match (button, button_up) {
        (Button::Left, false) => MOUSEEVENTF_LEFTDOWN,
        (Button::Left, true) => MOUSEEVENTF_LEFTUP,
        (Button::Right, false) => MOUSEEVENTF_RIGHTDOWN,
        (Button::Right, true) => MOUSEEVENTF_RIGHTUP,
        (Button::Middle, false) => MOUSEEVENTF_MIDDLEDOWN,
        (Button::Middle, true) => MOUSEEVENTF_MIDDLEUP,
    };
    Input {
        input_type: INPUT_MOUSE,
        data: InputData {
            mouse: MouseInput {
                dx: 0,
                dy: 0,
                mouse_data: 0,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

#[derive(Default)]
struct InputGuard(Vec<Input>);

impl InputGuard {
    fn press(&mut self, down: Input, up: Input) -> Result<(), String> {
        send(down)?;
        self.0.push(up);
        Ok(())
    }

    fn release_last(&mut self) -> Result<(), String> {
        if let Some(up) = self.0.last() {
            send(*up)?;
            self.0.pop();
        }
        Ok(())
    }

    fn key_down(&mut self, key: u16) -> Result<(), String> {
        self.press(key_input(key, false), key_input(key, true))
    }

    fn button_down(&mut self, button: Button) -> Result<(), String> {
        self.press(
            mouse_button_input(button, false),
            mouse_button_input(button, true),
        )
    }

    fn unicode_down(&mut self, unit: u16) -> Result<(), String> {
        self.press(
            keyboard_input(0, unit, KEYEVENTF_UNICODE),
            keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        )
    }

    fn release_all(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for up in std::mem::take(&mut self.0).into_iter().rev() {
            if let Err(error) = send(up) {
                self.0.push(up);
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

struct DpiScope(isize);

impl DpiScope {
    fn enter() -> Result<Self, String> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous == 0 {
            return Err(win_error("SetThreadDpiAwarenessContext"));
        }
        Ok(Self(previous))
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        let _ = unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

struct InputDesktop(*mut c_void);

impl InputDesktop {
    fn open() -> Result<Self, String> {
        let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS | DESKTOP_ENUMERATE) };
        if desktop.is_null() {
            return Err(win_error("OpenInputDesktop"));
        }
        let guard = Self(desktop);
        guard.verify()?;
        Ok(guard)
    }

    fn verify(&self) -> Result<(), String> {
        let mut name = [0u16; 256];
        let mut returned = 0u32;
        let ok = unsafe {
            GetUserObjectInformationW(
                self.0,
                UOI_NAME,
                name.as_mut_ptr().cast(),
                (name.len() * size_of::<u16>()) as u32,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(win_error("GetUserObjectInformationW"));
        }
        let units = (returned as usize / size_of::<u16>()).min(name.len());
        let desktop_name = String::from_utf16_lossy(&name[..units])
            .trim_end_matches('\0')
            .to_owned();
        if desktop_name.is_empty() {
            return Err("the interactive input desktop has no name".into());
        }
        if desktop_name.eq_ignore_ascii_case("winlogon")
            || desktop_name.eq_ignore_ascii_case("screen-saver")
            || desktop_name.eq_ignore_ascii_case("screensaver")
        {
            return Err("the interactive desktop is locked".into());
        }
        Ok(())
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { CloseDesktop(self.0) };
        }
    }
}

fn win_error(operation: &str) -> String {
    format!("{operation} failed with Windows error {}", unsafe {
        get_last_error()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_abi_and_virtual_desktop_pixel_mapping() {
        assert_eq!(
            size_of::<Input>(),
            if size_of::<usize>() == 8 { 40 } else { 28 }
        );
        let geometry = Geometry {
            x: -1920,
            y: -200,
            width: 5760,
            height: 2160,
        };
        for point in [(-1920, -200), (-1919, -199), (0, 0), (3839, 1959)] {
            let (x, y) = absolute_coordinates(geometry, point).unwrap();
            assert_eq!(
                i64::from(x) * i64::from(geometry.width) / 65_536,
                i64::from(point.0 - geometry.x)
            );
            assert_eq!(
                i64::from(y) * i64::from(geometry.height) / 65_536,
                i64::from(point.1 - geometry.y)
            );
        }
    }
}
