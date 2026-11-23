use std::io::{self, BufReader, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, GetCursorPos, IsIconic, SW_RESTORE, SetForegroundWindow,
    ShowWindowAsync,
};

use super::WindowKind;
use super::placement::find_window;
use super::protocol::{Frames, GuidePreferences, encode, parse_settings};
use crate::platform::AppResult;
use crate::settings::Settings;

pub(super) struct EditorHost {
    pub(super) process: WindowProcess,
    commits: Receiver<Settings>,
}

impl EditorHost {
    pub(super) fn start(
        path: &Path,
        baseline: &Settings,
        wake: impl Fn() + Send + 'static,
    ) -> AppResult<Self> {
        let baseline = encode(baseline)?;
        let (committed, commits) = mpsc::channel();
        let process = WindowProcess::start(
            WindowKind::Settings,
            Some(path),
            move |mut child, mut input| {
                let sent = input.write_all(baseline.as_bytes());
                drop(input);
                let output = child.stdout.take().expect("Settings uses a stdout pipe");
                thread::scope(|scope| {
                    let reader = scope.spawn(move || -> io::Result<()> {
                        let mut frames = Frames::new(BufReader::new(output));
                        while let Some(settings) = frames.next(parse_settings)? {
                            if committed.send(settings).is_err() {
                                break;
                            }
                            wake();
                        }
                        Ok(())
                    });
                    let output = child.wait_with_output()?;
                    let read = reader.join().map_err(|_| {
                        io::Error::other("The Settings reader stopped unexpectedly.")
                    })?;
                    if output.status.success() {
                        sent?;
                        read?;
                    }
                    Ok(output)
                })
            },
        )?;
        Ok(Self { process, commits })
    }

    pub(super) fn take_latest_commit(&self) -> Option<Settings> {
        self.commits.try_iter().last()
    }
}

pub(super) struct GuideHost {
    updates: Sender<GuidePreferences>,
    pub(super) process: WindowProcess,
}

impl GuideHost {
    pub(super) fn start(initial: GuidePreferences) -> AppResult<Self> {
        let baseline = encode(&initial)?;
        let (updates, incoming) = mpsc::channel::<GuidePreferences>();
        let process =
            WindowProcess::start(WindowKind::QuickGuide, None, move |child, mut input| {
                let sent = input.write_all(baseline.as_bytes());
                if sent.is_ok() {
                    // Keep pipe writes off the tray thread, including while the guide is closing.
                    thread::Builder::new()
                        .name("quick-guide-updates".into())
                        .spawn(move || {
                            for preferences in incoming {
                                let Ok(frame) = encode(&preferences) else {
                                    break;
                                };
                                if input.write_all(frame.as_bytes()).is_err() {
                                    break;
                                }
                            }
                        })?;
                } else {
                    drop(input);
                }
                let output = child.wait_with_output()?;
                if output.status.success() {
                    sent?;
                }
                Ok(output)
            })?;
        Ok(Self { updates, process })
    }

    pub(super) fn update(&self, preferences: GuidePreferences) {
        let _ = self.updates.send(preferences);
    }
}

pub(super) struct WindowProcess {
    kind: WindowKind,
    pid: u32,
    keep_alive: Option<OwnedHandle>,
    worker: Option<JoinHandle<io::Result<Output>>>,
}

impl WindowProcess {
    fn start(
        kind: WindowKind,
        settings_path: Option<&Path>,
        run: impl FnOnce(Child, ChildStdin) -> io::Result<Output> + Send + 'static,
    ) -> AppResult<Self> {
        // Reactor owns the WinUI loop. WinUI cannot start a second Application in
        // the same process after that loop exits, so each window uses a fresh host.
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(kind.argument()).args(settings_path);
        let mut pointer = POINT::default();
        // SAFETY: Capture the pointer location before WinUI starts.
        if unsafe { GetCursorPos(&mut pointer) }.is_ok() {
            command.args([pointer.x.to_string(), pointer.y.to_string()]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        // SAFETY: This child displays the window requested from the tray or Setup.
        let _ = unsafe { AllowSetForegroundWindow(pid) };
        let input = child.stdin.take().expect("UI hosts use a stdin pipe");
        let keep_alive = input.as_handle().try_clone_to_owned()?;
        let worker = thread::Builder::new()
            .name("ui-window-host".into())
            .spawn(move || run(child, input))?;
        Ok(Self {
            kind,
            pid,
            keep_alive: Some(keep_alive),
            worker: Some(worker),
        })
    }

    pub(super) fn show(&self) {
        if let Some(window) = find_window(self.pid, self.kind.title()) {
            // SAFETY: We found the window in this child process. Restoring
            // asynchronously lets the tray continue even if WinUI is busy.
            unsafe {
                if IsIconic(window).as_bool() {
                    let _ = ShowWindowAsync(window, SW_RESTORE);
                }
                let _ = SetForegroundWindow(window);
            }
        }
    }

    pub(super) fn wait_handle(&self) -> HANDLE {
        HANDLE(
            self.worker
                .as_ref()
                .expect("active UI worker")
                .as_raw_handle(),
        )
    }

    pub(super) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub(super) fn finish(mut self) -> AppResult<()> {
        let title = self.kind.title();
        let Output { status, stderr, .. } =
            self.worker
                .take()
                .expect("active UI worker")
                .join()
                .map_err(|_| format!("The {title} host stopped unexpectedly."))??;
        if !status.success() {
            let details = String::from_utf8_lossy(&stderr);
            let details = details.trim();
            return Err(if details.is_empty() {
                format!("{title} stopped. {status}")
            } else {
                format!("{title} stopped.\n\n{details}")
            }
            .into());
        }
        Ok(())
    }
}

impl Drop for WindowProcess {
    fn drop(&mut self) {
        // Closing the pipe tells the child to close on its UI thread.
        drop(self.keep_alive.take());
        if let Some(worker) = &self.worker {
            // SAFETY: The worker handle remains live. Bound the wait if WinUI is unresponsive.
            unsafe { WaitForSingleObject(HANDLE(worker.as_raw_handle()), 1000) };
        }
    }
}
