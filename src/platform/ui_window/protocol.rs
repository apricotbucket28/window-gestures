use std::io::{self, BufRead};

use serde::{Deserialize, Serialize};

use crate::settings::{Modifier, Settings};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct GuidePreferences {
    pub(crate) modifier: Modifier,
    pub(crate) tiling: bool,
}

impl From<&Settings> for GuidePreferences {
    fn from(settings: &Settings) -> Self {
        Self {
            modifier: settings.modifier,
            tiling: settings.tiling,
        }
    }
}

pub(super) fn encode(value: &impl Serialize) -> Result<String, toml::ser::Error> {
    // A NUL delimiter lets both processes exchange snapshots without closing the pipe.
    let mut frame = toml::to_string(value)?;
    frame.push('\0');
    Ok(frame)
}

pub(super) fn parse_settings(text: &str) -> io::Result<Settings> {
    Settings::parse(text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) fn parse_preferences(text: &str) -> io::Result<GuidePreferences> {
    toml::from_str(text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(super) struct Frames<R> {
    input: R,
    buffer: Vec<u8>,
}

impl<R: BufRead> Frames<R> {
    pub(super) fn new(input: R) -> Self {
        Self {
            input,
            buffer: Vec::new(),
        }
    }

    pub(super) fn next<T>(
        &mut self,
        decode: impl FnOnce(&str) -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        self.buffer.clear();
        if self.input.read_until(0, &mut self.buffer)? == 0 {
            return Ok(None);
        }
        if self.buffer.pop() != Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete Settings update.",
            ));
        }
        decode(self.text()?).map(Some)
    }

    pub(super) fn editor_baseline(mut self) -> io::Result<(Settings, bool)> {
        // A standalone Settings process can also receive a single TOML document to EOF.
        self.input.read_until(0, &mut self.buffer)?;
        let has_parent = self.buffer.last() == Some(&0);
        if has_parent {
            self.buffer.pop();
        }
        Ok((parse_settings(self.text()?)?, has_parent))
    }

    fn text(&self) -> io::Result<&str> {
        std::str::from_utf8(&self.buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_each_complete_settings_commit() {
        let first = Settings::default();
        let mut second = first.clone();
        second.modifier = Modifier::Alt;
        second.excluded_apps.push("編集.exe".into());
        let frames = encode(&first).unwrap() + &encode(&second).unwrap();
        let mut commits = Frames::new(frames.as_bytes());
        assert_eq!(commits.next(parse_settings).unwrap(), Some(first.clone()));
        assert_eq!(commits.next(parse_settings).unwrap(), Some(second));
        assert_eq!(commits.next(parse_settings).unwrap(), None);

        let incomplete = &frames.as_bytes()[..frames.len() - 1];
        let mut commits = Frames::new(incomplete);
        assert_eq!(commits.next(parse_settings).unwrap(), Some(first));
        let error = commits.next(parse_settings).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn rejects_a_settings_snapshot_that_the_file_loader_would_reject() {
        let frame = encode(&Settings::default())
            .unwrap()
            .replace("version = 1", "version = 2");
        let error = Frames::new(frame.as_bytes())
            .next(parse_settings)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
