use std::path::Path;

use windows::Win32::Foundation::HANDLE;

use super::GuidePreferences;
use super::host::{EditorHost, GuideHost};
use crate::platform::{AppResult, show_error};
use crate::settings::Settings;

pub(in crate::platform) struct WindowController<'a, Wake> {
    settings_path: &'a Path,
    settings: Settings,
    wake: Wake,
    guide: Option<OpenGuide>,
    editor: Option<EditorHost>,
    guide_dismissed: bool,
}

struct OpenGuide {
    host: GuideHost,
    intent: GuideIntent,
}

#[derive(Default)]
struct GuideIntent {
    open_editor: bool,
    report_errors: bool,
}

impl<'a, Wake: Fn() + Clone + Send + 'static> WindowController<'a, Wake> {
    pub(in crate::platform) fn new(
        settings_path: &'a Path,
        settings: Settings,
        wake: Wake,
    ) -> Self {
        Self {
            settings_path,
            settings,
            wake,
            editor: None,
            guide: None,
            guide_dismissed: false,
        }
    }

    pub(in crate::platform) fn handles(&self) -> impl Iterator<Item = HANDLE> + '_ {
        self.editor
            .iter()
            .map(|editor| editor.process.wait_handle())
            .chain(
                self.guide
                    .iter()
                    .map(|guide| guide.host.process.wait_handle()),
            )
    }

    pub(in crate::platform) fn show_welcome(&mut self) {
        if self.welcome_pending() {
            self.show_guide(GuideIntent::default());
        }
    }

    pub(in crate::platform) fn open_settings(&mut self) {
        if let Some(editor) = &self.editor {
            editor.process.show();
        } else if !self.welcome_pending()
            || !self.show_guide(GuideIntent {
                open_editor: true,
                report_errors: false,
            })
        {
            self.start_editor();
        }
    }

    pub(in crate::platform) fn open_guide(&mut self) {
        self.show_guide(GuideIntent {
            open_editor: false,
            report_errors: true,
        });
    }

    pub(in crate::platform) fn poll(&mut self, apply: impl FnOnce(&Settings)) {
        // Check the worker status first so we drain any final Settings commit.
        if let Some(editor) = &self.editor {
            let finished = editor.process.is_finished();
            if let Some(saved) = editor.take_latest_commit() {
                apply(&saved);
                if let Some(guide) = &self.guide {
                    let current = GuidePreferences::from(&self.settings);
                    let updated = GuidePreferences::from(&saved);
                    if updated != current {
                        guide.host.update(updated);
                    }
                }
                self.settings = saved;
            }
            if finished {
                let editor = self.editor.take().expect("finished Settings process");
                if let Err(error) = editor.process.finish() {
                    show_error(&error.to_string());
                }
                self.save_welcome_if_ready();
            }
        }
        if self
            .guide
            .as_ref()
            .is_some_and(|guide| guide.host.process.is_finished())
        {
            let guide = self.guide.take().expect("finished Quick guide process");
            let result = guide.host.process.finish();
            if self.finish_guide(guide.intent, result) {
                self.start_editor();
            }
        }
    }

    fn welcome_pending(&self) -> bool {
        !self.settings.first_run_shown && !self.guide_dismissed
    }

    fn start_editor(&mut self) {
        match EditorHost::start(self.settings_path, &self.settings, self.wake.clone()) {
            Ok(editor) => self.editor = Some(editor),
            Err(error) => show_error(&error.to_string()),
        }
    }

    fn show_guide(&mut self, intent: GuideIntent) -> bool {
        if let Some(guide) = &mut self.guide {
            guide.intent.open_editor |= intent.open_editor;
            guide.intent.report_errors |= intent.report_errors;
            guide.host.process.show();
            return true;
        }
        match GuideHost::start(GuidePreferences::from(&self.settings)) {
            Ok(host) => {
                self.guide = Some(OpenGuide { host, intent });
                true
            }
            Err(error) => {
                if intent.report_errors {
                    show_error(&error.to_string());
                }
                false
            }
        }
    }

    fn finish_guide(&mut self, intent: GuideIntent, result: AppResult<()>) -> bool {
        match result {
            Ok(()) => {
                self.guide_dismissed = true;
                self.save_welcome_if_ready();
            }
            Err(error) if intent.report_errors => show_error(&error.to_string()),
            Err(_) => (),
        }
        intent.open_editor
    }

    fn save_welcome_if_ready(&mut self) {
        // Wait for the editor to close before writing so its baseline stays current.
        if self.editor.is_none()
            && self.guide_dismissed
            && !self.settings.first_run_shown
            && Settings::mark_first_run_shown(self.settings_path).is_ok()
        {
            self.settings.first_run_shown = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_guide_releases_the_settings_request_and_keeps_the_welcome_pending() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let settings = Settings::default();
        assert!(settings.create(&path).unwrap());
        let original = std::fs::read(&path).unwrap();
        let mut windows = WindowController::new(&path, settings, || {});
        let intent = GuideIntent {
            open_editor: true,
            report_errors: false,
        };

        assert!(windows.finish_guide(intent, Err("Guide failed".into())));
        assert!(windows.welcome_pending());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
