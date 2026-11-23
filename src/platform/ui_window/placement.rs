use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, ICON_BIG, ICON_SMALL, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowPos, WM_SETICON,
};
use windows::core::BOOL;

use crate::geometry::{Point, Rect};
use crate::platform::AppIcon;
use crate::platform::window::{window_rect, window_work_area, work_area};

pub(super) struct WindowPlacement {
    launch_point: Option<Point>,
    icons: RefCell<Vec<AppIcon>>,
}

impl WindowPlacement {
    pub(super) fn new(launch_point: Option<Point>) -> Self {
        Self {
            launch_point,
            icons: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn initialize(&self, window: HWND) {
        let current_area = window_work_area(window);
        if let Some(area) = self.launch_point.and_then(work_area).or(current_area) {
            if Some(area) != current_area {
                // Let WinUI adjust to the destination DPI before fitting the window.
                center_window(window, area, false);
            }
            center_window(window, area, true);
        }

        let mut icons = self.icons.borrow_mut();
        for (size, small) in [(ICON_SMALL, true), (ICON_BIG, false)] {
            if let Ok(icon) = AppIcon::load(window, small) {
                // SAFETY: The child owns the window and retains the icon until close.
                unsafe {
                    SendMessageW(
                        window,
                        WM_SETICON,
                        Some(WPARAM(size as usize)),
                        Some(LPARAM(icon.0.0 as isize)),
                    );
                }
                icons.push(icon);
            }
        }
    }
}

fn center_window(window: HWND, area: Rect, fit: bool) {
    // GetWindowRect also works before DWM has rendered the window.
    let Some(bounds) = window_rect(window) else {
        return;
    };
    let width = bounds.x.length();
    let height = bounds.y.length();
    let size = if fit {
        (width.min(area.x.length()), height.min(area.y.length()))
    } else {
        (width, height)
    };
    let destination = area.fit_tile(area, size);
    let mut flags = SWP_NOACTIVATE | SWP_NOZORDER;
    if size == (width, height) {
        flags |= SWP_NOSIZE;
    }
    // SAFETY: This runs on the thread that owns the window.
    let _ = unsafe {
        SetWindowPos(
            window,
            None,
            destination.x.start,
            destination.y.start,
            destination.x.length(),
            destination.y.length(),
            flags,
        )
    };
}

fn window_has_title(window: HWND, title: &str) -> bool {
    let mut caption = [0; 64];
    // SAFETY: The caption buffer is writable and lives through this call.
    let length = unsafe { GetWindowTextW(window, &mut caption) } as usize;
    caption[..length].iter().copied().eq(title.encode_utf16())
}

pub(super) fn find_window(pid: u32, title: &str) -> Option<HWND> {
    struct Search<'a> {
        pid: u32,
        title: &'a str,
        window: Option<HWND>,
    }
    unsafe extern "system" fn find(window: HWND, data: LPARAM) -> BOOL {
        // SAFETY: The synchronous enumeration provides a live HWND and a writable search.
        unsafe {
            let search = &mut *std::ptr::with_exposed_provenance_mut::<Search<'_>>(data.0 as usize);
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid != search.pid {
                return true.into();
            }
            if window_has_title(window, search.title) {
                search.window = Some(window);
                return false.into();
            }
        }
        true.into()
    }
    let mut search = Search {
        pid,
        title,
        window: None,
    };
    // SAFETY: EnumWindows invokes the callback synchronously while search is live.
    unsafe {
        let _ = EnumWindows(
            Some(find),
            LPARAM((&mut search as *mut Search).expose_provenance() as isize),
        );
    }
    search.window
}
