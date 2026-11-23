#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Window Gestures requires Windows.");

#[cfg(windows)]
mod geometry;

#[cfg(windows)]
mod platform;

#[cfg(windows)]
mod settings;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    platform::run_entry_point()
}
