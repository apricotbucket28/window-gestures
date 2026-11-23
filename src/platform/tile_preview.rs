use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_HIGHLIGHT, GetSysColorBrush, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE, PeekMessageW,
    SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, Result, w};

use super::owned_window::OwnedWindow;

use crate::geometry::Rect;

const CLASS: PCWSTR = w!("WindowGesturesTilePreview");

pub(super) struct TilePreview {
    popup: OwnedWindow,
    shown: Option<Rect>,
}

impl TilePreview {
    pub(super) fn create() -> Result<Self> {
        // SAFETY: The class name and window procedure are static. The system brush is shared.
        unsafe {
            let class = WNDCLASSW {
                hInstance: HINSTANCE(GetModuleHandleW(None)?.0),
                lpfnWndProc: Some(window_proc),
                lpszClassName: CLASS,
                hbrBackground: GetSysColorBrush(COLOR_HIGHLIGHT),
                ..Default::default()
            };
            let popup = OwnedWindow::create(
                &class,
                PCWSTR::null(),
                WS_POPUP,
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            )?;
            SetLayeredWindowAttributes(popup.hwnd, COLORREF(0), 74, LWA_ALPHA)?;
            Ok(Self { popup, shown: None })
        }
    }

    pub(super) fn show(&mut self, rect: Rect) {
        if self.shown == Some(rect) {
            return;
        }
        // SAFETY: Position and paint our own click-through window without activating it.
        unsafe {
            if SetWindowPos(
                self.popup.hwnd,
                Some(HWND_TOPMOST),
                rect.x.start,
                rect.y.start,
                rect.x.length(),
                rect.y.length(),
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .is_ok()
            {
                self.shown = Some(rect);
                let _ = UpdateWindow(self.popup.hwnd);
            } else {
                self.hide();
            }
        }
    }

    pub(super) fn hide(&mut self) {
        if self.shown.take().is_some() {
            // SAFETY: This thread owns the preview window.
            unsafe {
                let _ = ShowWindow(self.popup.hwnd, SW_HIDE);
            }
        }
    }

    pub(super) fn pump(&self) {
        // SAFETY: Dispatch only messages from this thread for its preview window.
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, Some(self.popup.hwnd), 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&message);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn verify_window(&mut self, rect: Rect) {
        use windows::Win32::Foundation::{POINT, RECT};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowRect, IsWindowVisible, WindowFromPoint,
        };

        // SAFETY: Query the fixture's geometry and normal desktop hit testing.
        unsafe {
            let original_foreground = GetForegroundWindow();
            self.show(rect);
            let mut bounds = RECT::default();
            GetWindowRect(self.popup.hwnd, &mut bounds).unwrap();
            assert_eq!(Rect::from(bounds), rect);
            assert!(IsWindowVisible(self.popup.hwnd).as_bool());
            let point = POINT {
                x: rect.x.start + 1,
                y: rect.y.start + 1,
            };
            assert_ne!(WindowFromPoint(point), self.popup.hwnd);
            assert_eq!(GetForegroundWindow(), original_foreground);
            self.hide();
            assert!(!IsWindowVisible(self.popup.hwnd).as_bool());
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: Forward the original window message to the default procedure.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}
