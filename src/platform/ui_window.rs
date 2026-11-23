mod controller;
mod host;
mod placement;
mod protocol;
mod quick_guide;
mod session;
mod settings_page;

use std::ffi::OsStr;
use std::io;
use std::rc::Rc;

use windows::Win32::Foundation::HWND;
use windows_reactor::{App, WindowPolicy};

use crate::geometry::Point;

use super::AppResult;
use placement::WindowPlacement;
use protocol::{Frames, parse_preferences};
use quick_guide::QuickGuidePage;
use settings_page::SettingsPage;

pub(super) use controller::WindowController;
use protocol::GuidePreferences;
use session::SettingsSession;

pub(crate) const SETTINGS_TITLE: &str = "Settings";
pub(crate) const QUICK_GUIDE_TITLE: &str = "Window Gestures quick guide";

#[derive(Clone, Copy)]
pub(super) enum WindowKind {
    Settings,
    QuickGuide,
}

impl WindowKind {
    pub(super) fn from_argument(argument: Option<&OsStr>) -> Option<Self> {
        let argument = argument?;
        [Self::Settings, Self::QuickGuide]
            .into_iter()
            .find(|kind| argument == OsStr::new(kind.argument()))
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Settings => "--settings",
            Self::QuickGuide => "--quick-guide",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Settings => SETTINGS_TITLE,
            Self::QuickGuide => QUICK_GUIDE_TITLE,
        }
    }
}

pub(super) fn run(kind: WindowKind) -> AppResult<()> {
    let mut arguments = std::env::args_os().skip(2);
    let settings_path = match kind {
        WindowKind::Settings => Some(arguments.next().ok_or("The Settings path is missing.")?),
        WindowKind::QuickGuide => None,
    };
    let launch_point = arguments.next().zip(arguments.next()).and_then(|(x, y)| {
        Some(Point {
            x: x.to_str()?.parse().ok()?,
            y: y.to_str()?.parse().ok()?,
        })
    });
    // Keep the native icon handles alive until Reactor closes the window.
    let placement = Rc::new(WindowPlacement::new(launch_point));
    let setup = Rc::clone(&placement);
    let policy = WindowPolicy::new().before_show(move |window| setup.initialize(HWND(window)));

    // Release the stdin lock before starting the UI loop and its reader.
    if let Some(path) = settings_path {
        let (settings, has_parent) = Frames::new(io::stdin().lock()).editor_baseline()?;
        let session = SettingsSession::new(path.into(), settings, has_parent);
        App::run_component_with_policy::<SettingsPage>(session, policy)?;
    } else {
        let preferences = Frames::new(io::stdin().lock())
            .next(parse_preferences)?
            .ok_or("The Quick guide needs a Window Gestures session.")?;
        App::run_component_with_policy::<QuickGuidePage>(preferences, policy)?;
    }
    Ok(())
}
