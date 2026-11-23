use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::rc::Rc;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

use super::SETTINGS_TITLE;
use super::placement::find_window;
use super::protocol::encode;
use crate::platform::{AppResult, startup::StartupShortcut};
use crate::settings::Settings;

#[derive(Clone)]
pub(crate) struct SettingsSession(Rc<EditSession>);

impl PartialEq for SettingsSession {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

struct EditSession {
    path: PathBuf,
    committed: RefCell<Settings>,
    has_parent: bool,
    startup: RefCell<Option<StartupShortcut>>,
}

impl SettingsSession {
    pub(super) fn new(path: PathBuf, settings: Settings, has_parent: bool) -> Self {
        Self(Rc::new(EditSession {
            path,
            committed: RefCell::new(settings),
            has_parent,
            startup: RefCell::new(StartupShortcut::for_current_user().ok()),
        }))
    }

    pub(crate) fn has_parent(&self) -> bool {
        self.0.has_parent
    }

    pub(crate) fn wait_for_parent_exit() {
        let _ = io::stdin().read(&mut [0]);
    }

    pub(crate) fn draft(&self) -> Settings {
        self.0.committed.borrow().clone()
    }

    pub(crate) fn start_at_sign_in(&self) -> Option<bool> {
        self.0
            .startup
            .borrow()
            .as_ref()
            .map(StartupShortcut::enabled)
    }

    pub(crate) fn has_changes(&self, draft: &Settings, start_at_sign_in: bool) -> bool {
        draft != &*self.0.committed.borrow()
            || self
                .start_at_sign_in()
                .is_some_and(|enabled| enabled != start_at_sign_in)
    }

    pub(crate) fn save(&self, draft: &Settings, start_at_sign_in: bool) -> AppResult<()> {
        let frame = encode(draft)?;
        let mut committed = self.0.committed.borrow_mut();
        let save_settings = || draft.save_if_current(&self.0.path, &committed);
        if let Some(startup) = self.0.startup.borrow_mut().as_mut() {
            startup.save_with(start_at_sign_in, save_settings)?;
        } else {
            save_settings()?;
        }
        committed.clone_from(draft);
        let mut output = io::stdout().lock();
        output.write_all(frame.as_bytes()).and_then(|()| output.flush()).map_err(|error| {
            format!("The settings file was saved. Restart Window Gestures to load the changes.\n\n{error}")
        })?;
        Ok(())
    }

    pub(crate) fn open_file(&self) -> AppResult<()> {
        let target = HSTRING::from(self.0.path.as_os_str());
        // SAFETY: The path and verb remain valid through the call.
        let result = unsafe {
            ShellExecuteW(
                find_window(std::process::id(), SETTINGS_TITLE),
                w!("open"),
                &target,
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as usize <= 32 {
            return Err(format!(
                "Open this settings file in a text editor.\n\n{}",
                self.0.path.display()
            )
            .into());
        }
        Ok(())
    }
}
