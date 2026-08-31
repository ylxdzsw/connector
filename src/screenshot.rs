#[cfg(unix)]
#[path = "screenshot/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "screenshot/windows.rs"]
mod platform;

pub use platform::{Screenshot, capture};
