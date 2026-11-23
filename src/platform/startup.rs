use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::MoveFileW;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::UI::Shell::{
    FOLDERID_Startup, IShellLinkW, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath, ShellLink,
};
use windows::core::{HSTRING, Interface};

use super::AppResult;
use super::com::{Apartment, TaskMem};

// Keep this name in sync with the installer's uninstall entry.
const SHORTCUT_NAME: &str = "Window Gestures.lnk";

pub(super) struct StartupShortcut {
    folder: PathBuf,
    enabled: bool,
}

impl StartupShortcut {
    pub(super) fn for_current_user() -> AppResult<Self> {
        let _apartment = Apartment::initialize()?;
        // SAFETY: The Shell returns a COM task allocation with a NUL-terminated path.
        let folder = unsafe {
            let wide = SHGetKnownFolderPath(&FOLDERID_Startup, KF_FLAG_DONT_VERIFY, None)?;
            let _allocation = TaskMem::from_raw(wide.0);
            PathBuf::from(OsString::from_wide(wide.as_wide()))
        };
        let enabled = shortcut_exists(&folder.join(SHORTCUT_NAME))?;
        Ok(Self { folder, enabled })
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn save_with(
        &mut self,
        enabled: bool,
        save_settings: impl FnOnce() -> AppResult<()>,
    ) -> AppResult<()> {
        if enabled == self.enabled {
            return save_settings();
        }
        let path = self.folder.join(SHORTCUT_NAME);
        if shortcut_exists(&path)? != self.enabled {
            return Err(
                "The startup shortcut changed outside the app. Reopen Settings before saving."
                    .into(),
            );
        }

        // Stage the new link or keep the previous one for rollback on a failed
        // settings save. The staging folder is on the same filesystem.
        let change = || -> AppResult<_> {
            fs::create_dir_all(&self.folder)?;
            let temporary = tempfile::tempdir_in(&self.folder)?;
            let staged = temporary.path().join(SHORTCUT_NAME);
            if enabled {
                create_shortcut(&staged, &std::env::current_exe()?)?;
                move_without_replacing(&staged, &path)?;
            } else {
                move_without_replacing(&path, &staged)?;
            }
            Ok((temporary, staged))
        };
        let (temporary, staged) =
            change().map_err(|error| format!("Could not change Start at sign-in.\n\n{error}"))?;
        if let Err(error) = save_settings() {
            let restored = if enabled {
                fs::remove_file(&path).map_err(Into::into)
            } else {
                move_without_replacing(&staged, &path)
            };
            if let Err(restore_error) = restored {
                let backup = if enabled {
                    String::new()
                } else {
                    let path = temporary.keep().join(SHORTCUT_NAME);
                    format!(
                        "\n\nThe previous shortcut is available at {}.",
                        path.display()
                    )
                };
                return Err(format!(
                    "{error}\n\nCould not restore the startup shortcut. Reopen Settings to check its state.\n\n{restore_error}{backup}"
                )
                .into());
            }
            return Err(error);
        }
        self.enabled = enabled;
        Ok(())
    }
}

fn shortcut_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(io::Error::other("The startup shortcut path is occupied.")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn move_without_replacing(source: &Path, destination: &Path) -> AppResult<()> {
    let source = HSTRING::from(source.as_os_str());
    let destination = HSTRING::from(destination.as_os_str());
    // SAFETY: Both paths are null-terminated and live through the call.
    unsafe { MoveFileW(&source, &destination)? };
    Ok(())
}

fn create_shortcut(path: &Path, target: &Path) -> AppResult<()> {
    let _apartment = Apartment::initialize()?;
    let target = HSTRING::from(target.as_os_str());
    let path = HSTRING::from(path.as_os_str());
    // SAFETY: COM is initialized on this thread. Both strings live through the calls.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&target)?;
        link.cast::<IPersistFile>()?.Save(&path, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::STGM_READ;

    #[test]
    fn saves_a_launchable_shortcut_and_rolls_back_failed_settings_saves() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("Startup");
        let path = folder.join(SHORTCUT_NAME);
        let mut startup = StartupShortcut {
            folder,
            enabled: false,
        };
        let failed_save = || Err("Settings save failed.".into());

        assert!(startup.save_with(true, failed_save).is_err());
        assert!(!path.exists());

        startup.save_with(true, || Ok(())).unwrap();
        let saved_shortcut = fs::read(&path).unwrap();
        {
            let _apartment = Apartment::initialize().unwrap();
            let file = HSTRING::from(path.as_os_str());
            let mut target = vec![0u16; 32768];
            // SAFETY: COM is initialized, and the buffers live through the calls.
            unsafe {
                let link: IShellLinkW =
                    CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                link.cast::<IPersistFile>()
                    .unwrap()
                    .Load(&file, STGM_READ)
                    .unwrap();
                link.GetPath(&mut target, std::ptr::null_mut(), 0).unwrap();
            }
            let length = target.iter().position(|&code| code == 0).unwrap();
            let target = PathBuf::from(OsString::from_wide(&target[..length]));
            assert_eq!(
                target.canonicalize().unwrap(),
                std::env::current_exe().unwrap().canonicalize().unwrap()
            );
        }

        assert!(startup.save_with(false, failed_save).is_err());
        assert_eq!(fs::read(&path).unwrap(), saved_shortcut);
        startup.save_with(false, || Ok(())).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn retains_the_previous_shortcut_when_rollback_cannot_replace_an_external_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut startup = StartupShortcut {
            folder: directory.path().to_owned(),
            enabled: false,
        };
        let path = startup.folder.join(SHORTCUT_NAME);
        startup.save_with(true, || Ok(())).unwrap();
        let original = fs::read(&path).unwrap();

        let error = startup
            .save_with(false, || {
                fs::write(&path, b"external shortcut")?;
                Err("Settings save failed.".into())
            })
            .unwrap_err();

        let backup = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join(SHORTCUT_NAME);
        assert_eq!(fs::read(&backup).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), b"external shortcut");
        assert!(error.to_string().contains(&backup.display().to_string()));
    }
}
