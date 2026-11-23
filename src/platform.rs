mod actions;
mod com;
mod cursor;
mod exclusions;
mod hook;
mod movement;
mod owned_window;
mod process;
mod startup;
mod tile_preview;
mod tray;
mod ui_window;
mod window;
mod worker;

use std::error::Error;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HINSTANCE, HWND,
    SetLastError,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, HICON, IDYES, IMAGE_FLAGS, IMAGE_ICON, LoadImageW, MB_DEFBUTTON2, MB_ICONERROR,
    MB_ICONWARNING, MB_OK, MB_YESNO, MessageBoxW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
};
use windows::core::{HSTRING, PCWSTR, w};

use self::hook::InputHooks;
use self::tray::Tray;
use crate::settings::{ReadSettings, Settings, config_path};

type AppResult<T> = Result<T, Box<dyn Error>>;

const APP_TITLE: PCWSTR = w!("Window Gestures");

// Matches IDI_APP_ICON in app.rc.
const APP_ICON: u16 = 1;

struct AppIcon(HICON);

impl AppIcon {
    fn load(window: HWND, small: bool) -> windows::core::Result<Self> {
        let (width, height) = if small {
            (SM_CXSMICON, SM_CYSMICON)
        } else {
            (SM_CXICON, SM_CYICON)
        };
        // SAFETY: The integer resource ID refers to an icon in this executable.
        unsafe {
            let module = HINSTANCE(GetModuleHandleW(None)?.0);
            let dpi = GetDpiForWindow(window);
            LoadImageW(
                Some(module),
                PCWSTR(APP_ICON as usize as *const u16),
                IMAGE_ICON,
                GetSystemMetricsForDpi(width, dpi),
                GetSystemMetricsForDpi(height, dpi),
                IMAGE_FLAGS::default(),
            )
            .map(|image| Self(HICON(image.0)))
        }
    }
}

impl Drop for AppIcon {
    fn drop(&mut self) {
        // SAFETY: LoadImageW without LR_SHARED returns an owned icon.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

pub(super) fn run_entry_point() -> ExitCode {
    let argument = std::env::args_os().nth(1);
    let window = ui_window::WindowKind::from_argument(argument.as_deref());
    let result = match window {
        Some(window) => ui_window::run(window),
        None => run(argument.is_some_and(|arg| arg == "--show-welcome")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            if window.is_none() {
                show_error(&error.to_string());
            }
            ExitCode::FAILURE
        }
    }
}

fn run(show_welcome: bool) -> AppResult<()> {
    let Some(_instance) = Instance::acquire()? else {
        return Ok(());
    };
    let path = config_path()?;
    let Some(settings) = load_settings(&path)? else {
        return Ok(());
    };
    let tray = Tray::create()?;
    let (commands, requests) = worker::channel();
    let (worker_done, done) = channel();

    let notifications = tray.notifications();
    let worker = thread::Builder::new()
        .name("window-worker".into())
        .spawn(move || {
            let restored =
                movement::run(requests, |error| notifications.report_action_failed(error));
            let _ = worker_done.send(restored);
        })?;

    let notifications = tray.notifications();
    let hook_result = InputHooks::start(commands.clone(), settings.clone(), move |request| {
        notifications.post(request)
    })
    .and_then(|hooks| {
        let session = tray.run(&hooks, &path, settings, show_welcome);
        let stopped = hooks.stop();
        session.and(stopped)
    });

    commands.close();
    drop(commands);
    // A foreign window can stop responding partway through a style change.
    // Bound shutdown even if that window procedure never returns.
    let restored = done.recv_timeout(Duration::from_secs(1));
    hook_result?;
    match restored {
        Ok(restored) => {
            worker.join().map_err(|_| "The window worker stopped unexpectedly.")?;
            if restored { Ok(()) } else { Err("Some windows could not be restored after using Borderless.".into()) }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("A window stopped responding during shutdown. Its frame may need to be restored by reopening that window.".into()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("The window worker stopped unexpectedly.".into()),
    }
}

fn load_settings(path: &Path) -> AppResult<Option<Settings>> {
    let defaults = Settings::default();
    let save_error = |error| format!("Could not save {}.\n\n{error}", path.display());
    loop {
        match Settings::read(path)
            .map_err(|error| format!("Could not read {}.\n\n{error}", path.display()))?
        {
            ReadSettings::Found(settings) => return Ok(Some(settings)),
            ReadSettings::Missing => {
                if defaults.create(path).map_err(save_error)? {
                    return Ok(Some(defaults));
                }
            }
            ReadSettings::Invalid(error) => {
                let prompt = HSTRING::from(format!(
                    "Could not load settings from {}.\n\n{error}\n\nReplace this file with default settings? Choose No to keep the file and exit.",
                    path.display()
                ));
                // SAFETY: The strings remain valid until the dialog returns.
                let answer = unsafe {
                    MessageBoxW(
                        None,
                        &prompt,
                        APP_TITLE,
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    )
                };
                if answer != IDYES {
                    return Ok(None);
                }
                defaults.save(path).map_err(save_error)?;
                return Ok(Some(defaults));
            }
        }
    }
}

pub(super) fn show_error(message: &str) {
    let message = HSTRING::from(message);
    // SAFETY: The strings remain valid until MessageBoxW returns.
    unsafe { MessageBoxW(None, &message, APP_TITLE, MB_OK | MB_ICONERROR) };
}

struct Instance(HANDLE);

impl Instance {
    fn acquire() -> windows::core::Result<Option<Self>> {
        // SAFETY: The name is a static, NUL-terminated string. The mutex is used
        // as a per-session lifetime marker and is never held by a thread.
        let (handle, already_running) = unsafe {
            SetLastError(ERROR_SUCCESS);
            let handle = CreateMutexW(
                None,
                false,
                w!(r"Local\{76E9A834-960A-4A42-9E34-CE5B439FA183}"),
            )?;
            (handle, GetLastError() == ERROR_ALREADY_EXISTS)
        };
        let instance = Self(handle);
        if already_running {
            return Ok(None);
        }
        Ok(Some(instance))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: This guard owns the CreateMutexW handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
