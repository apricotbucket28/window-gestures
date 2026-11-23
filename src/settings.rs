use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Modifier {
    #[default]
    Win,
    Alt,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WindowPermissions {
    pub(crate) move_maximized: bool,
    pub(crate) resize_maximized: bool,
}

impl Default for WindowPermissions {
    fn default() -> Self {
        Self {
            move_maximized: true,
            resize_maximized: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    version: u32,
    #[serde(default)]
    pub(crate) first_run_shown: bool,
    pub(crate) modifier: Modifier,
    pub(crate) snapping: bool,
    pub(crate) tiling: bool,
    pub(crate) excluded_apps: Vec<String>,
    #[serde(default)]
    pub(crate) window_permissions: WindowPermissions,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            first_run_shown: false,
            modifier: Modifier::Win,
            snapping: true,
            tiling: true,
            excluded_apps: Vec::new(),
            window_permissions: WindowPermissions::default(),
        }
    }
}

pub(crate) enum ReadSettings {
    Found(Settings),
    Missing,
    Invalid(String),
}

pub(crate) fn config_path() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))?;
    Ok(PathBuf::from(local)
        .join("WindowGestures")
        .join("settings.toml"))
}

impl Settings {
    pub(crate) fn mark_first_run_shown(path: &Path) -> Result<(), Box<dyn Error>> {
        let ReadSettings::Found(mut settings) = Self::read(path)? else {
            return Err("The settings file is missing or invalid.".into());
        };
        if !settings.first_run_shown {
            settings.first_run_shown = true;
            settings.save(path)?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != SCHEMA_VERSION {
            return Err(format!("Settings version {} is unsupported.", self.version));
        }
        for name in &self.excluded_apps {
            Self::validate_excluded_app(name)?;
        }
        Ok(())
    }

    pub(crate) fn validate_excluded_app(name: &str) -> Result<(), String> {
        if name.is_empty()
            || name.trim() != name
            || matches!(name, "." | "..")
            || name
                .chars()
                .any(|ch| ch.is_control() || "\\/:*?\"<>|".contains(ch))
            || name.encode_utf16().count() > 255
        {
            return Err(format!(
                "Excluded applications must be executable file names, such as notepad.exe. Invalid value: {name:?}"
            ));
        }
        Ok(())
    }

    pub(crate) fn read(path: &Path) -> io::Result<ReadSettings> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ReadSettings::Missing);
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Ok(ReadSettings::Invalid(error.to_string()));
            }
            Err(error) => return Err(error),
        };
        match Self::parse(&text) {
            Ok(settings) => Ok(ReadSettings::Found(settings)),
            Err(error) => Ok(ReadSettings::Invalid(error)),
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let settings: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        settings.validate()?;
        Ok(settings)
    }

    pub(crate) fn save_if_current(
        &self,
        path: &Path,
        baseline: &Self,
    ) -> Result<(), Box<dyn Error>> {
        if !matches!(Self::read(path)?, ReadSettings::Found(ref current) if current == baseline) {
            return Err("The settings file changed outside the app. Restart Window Gestures to load it before saving here.".into());
        }
        self.save(path)
            .map_err(|error| format!("Could not save {}.\n\n{error}", path.display()).into())
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        self.write_temp(path)?.persist(path)?;
        Ok(())
    }

    pub(crate) fn create(&self, path: &Path) -> Result<bool, Box<dyn Error>> {
        match self.write_temp(path)?.persist_noclobber(path) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn write_temp(&self, path: &Path) -> Result<NamedTempFile, Box<dyn Error>> {
        self.validate()?;
        let parent = path
            .parent()
            .ok_or("The settings path has no parent directory.")?;
        fs::create_dir_all(parent)?;
        let mut new_file = NamedTempFile::new_in(parent)?;
        new_file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        new_file.as_file().sync_all()?;
        Ok(new_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_replace_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        assert!(matches!(Settings::read(&path), Ok(ReadSettings::Missing)));
        assert!(Settings::default().create(&path).unwrap());
        let new = Settings {
            modifier: Modifier::Alt,
            tiling: false,
            excluded_apps: vec!["編集.exe".into()],
            window_permissions: WindowPermissions {
                move_maximized: false,
                resize_maximized: true,
            },
            ..Default::default()
        };
        new.save(&path).unwrap();
        assert!(!Settings::default().create(&path).unwrap());
        assert!(
            matches!(Settings::read(&path), Ok(ReadSettings::Found(settings)) if settings == new)
        );
    }

    #[test]
    fn guide_dismissal_persists_with_the_latest_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        assert!(Settings::default().create(&path).unwrap());
        assert!(
            matches!(Settings::read(&path), Ok(ReadSettings::Found(settings)) if !settings.first_run_shown)
        );

        let mut edited = Settings {
            modifier: Modifier::Alt,
            tiling: false,
            ..Settings::default()
        };
        edited.save(&path).unwrap();
        Settings::mark_first_run_shown(&path).unwrap();
        edited.first_run_shown = true;
        assert!(
            matches!(Settings::read(&path), Ok(ReadSettings::Found(settings)) if settings == edited)
        );
    }

    #[test]
    fn invalid_or_unsupported_settings_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let existing = "version = 2\nmodifier = 'win'\nsnapping = true\nexcluded_apps = []\n";
        fs::write(&path, existing).unwrap();
        assert!(matches!(
            Settings::read(&path),
            Ok(ReadSettings::Invalid(_))
        ));
        let invalid = Settings {
            excluded_apps: vec!["C:\\Example.exe".into()],
            ..Default::default()
        };
        assert!(invalid.save(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), existing);
    }
    #[test]
    fn saving_a_stale_draft_preserves_the_settings_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let initial = Settings::default();
        let mut external = initial.clone();
        external.modifier = Modifier::Alt;
        initial.save(&path).unwrap();
        external.save(&path).unwrap();
        assert!(initial.save_if_current(&path, &initial).is_err());
        assert!(
            matches!(Settings::read(&path), Ok(ReadSettings::Found(current)) if current == external)
        );
    }
}
