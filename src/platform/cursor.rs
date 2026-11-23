use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSORINFO, DefWindowProcW, GCLP_HCURSOR, GetClassLongPtrW, GetCursorInfo, HCURSOR,
    HWND_TOPMOST, IDC_HAND, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    IsWindowVisible, LoadCursorW, SW_HIDE, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSENDCHANGING,
    SWP_SHOWWINDOW, SetClassLongPtrW, SetCursor, SetWindowPos, ShowWindow, WM_ERASEBKGND,
    WM_MOUSEMOVE, WM_NCPAINT, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, Result, w};

use super::owned_window::OwnedWindow;

use crate::geometry::{Point, ResizeDirection};

const CLASS: PCWSTR = w!("WindowGesturesCursor");
const HOVER_RADIUS: i32 = 24;
const DRAG_RADIUS: i32 = 128;

fn within(point: Point, center: Point, radius: i32) -> bool {
    point.x.abs_diff(center.x) <= radius as u32 && point.y.abs_diff(center.y) <= radius as u32
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorShape {
    Ready,
    Move,
    Resize(ResizeDirection),
}

impl CursorShape {
    fn radius(self) -> i32 {
        if self == Self::Ready {
            HOVER_RADIUS
        } else {
            DRAG_RADIUS
        }
    }
}

pub(super) struct CursorOverlay {
    popup: OwnedWindow,
    shown: Option<(Point, CursorShape, HCURSOR)>,
    previous: Option<HCURSOR>,
}

impl CursorOverlay {
    pub(super) fn create(instance: HINSTANCE) -> Result<Self> {
        let class = WNDCLASSW {
            hInstance: instance,
            lpfnWndProc: Some(window_proc),
            lpszClassName: CLASS,
            ..Default::default()
        };
        // SAFETY: The class name and window procedure are static.
        let popup = unsafe {
            OwnedWindow::create(
                &class,
                PCWSTR::null(),
                WS_POPUP,
                WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            )?
        };
        Ok(Self {
            popup,
            shown: None,
            previous: None,
        })
    }

    pub(super) fn owns_point(&self, point: Point) -> bool {
        self.shown
            .is_some_and(|(at, shape, _)| within(point, at, shape.radius()))
    }

    pub(super) fn show(&mut self, point: Point, shape: CursorShape) {
        if self
            .shown
            .is_some_and(|(at, old, _)| old == shape && within(point, at, HOVER_RADIUS))
        {
            return;
        }
        // SAFETY: Shared system cursors remain valid for the lifetime of our
        // window. Only its owning thread stores and reads the cursor handle.
        unsafe {
            let cursor = if let Some((_, old, cursor)) = self.shown
                && old == shape
            {
                cursor
            } else {
                let name = match shape {
                    CursorShape::Ready => IDC_HAND,
                    CursorShape::Move => IDC_SIZEALL,
                    CursorShape::Resize(ResizeDirection::Horizontal) => IDC_SIZEWE,
                    CursorShape::Resize(ResizeDirection::Vertical) => IDC_SIZENS,
                    CursorShape::Resize(ResizeDirection::NorthwestSoutheast) => IDC_SIZENWSE,
                    CursorShape::Resize(ResizeDirection::NortheastSouthwest) => IDC_SIZENESW,
                };
                let Ok(cursor) = LoadCursorW(None, name) else {
                    return;
                };
                SetClassLongPtrW(self.popup.hwnd, GCLP_HCURSOR, cursor.0 as isize);
                cursor
            };
            if self.shown.is_none() {
                let mut info = CURSORINFO {
                    cbSize: size_of::<CURSORINFO>() as u32,
                    ..Default::default()
                };
                self.previous = GetCursorInfo(&mut info).ok().map(|()| info.hCursor);
            }
            // During a drag keep a wide margin around the pointer so queued
            // mouse events continue to hit this window while it follows along.
            let radius = shape.radius();
            let diameter = radius * 2 + 1;
            if SetWindowPos(
                self.popup.hwnd,
                Some(HWND_TOPMOST),
                point.x.saturating_sub(radius),
                point.y.saturating_sub(radius),
                diameter,
                diameter,
                SWP_NOACTIVATE | SWP_NOREDRAW | SWP_NOSENDCHANGING | SWP_SHOWWINDOW,
            )
            .is_ok()
            {
                self.shown = Some((point, shape, cursor));
                SetCursor(Some(cursor));
            } else {
                self.hide();
            }
        }
    }

    pub(super) fn hide(&mut self) {
        if self.shown.take().is_some() {
            // SAFETY: Restore the cursor while our overlay can still set its shape.
            unsafe {
                if let Some(cursor) = self.previous.take() {
                    SetCursor((!cursor.is_invalid()).then_some(cursor));
                }
                let _ = ShowWindow(self.popup.hwnd, SW_HIDE);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn verify_hit_testing(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetCursor, HTCLIENT, SendMessageW, WM_NCHITTEST, WM_SETCURSOR,
        };

        let point = Point { x: 1, y: 1 };
        let horizontal = CursorShape::Resize(ResizeDirection::Horizontal);
        for (point, shape, expected) in [
            (point, CursorShape::Ready, IDC_HAND),
            (point, horizontal, IDC_SIZEWE),
            (Point { x: 100, y: 100 }, horizontal, IDC_SIZEWE),
        ] {
            self.show(point, shape);
            // Hit-test the overlay directly so other windows on the desktop cannot
            // affect the result. WM_NCHITTEST packs the screen coordinates into two words.
            let coords = LPARAM((point.x | (point.y << 16)) as isize);
            // SAFETY: Query and send messages to the overlay on its owning thread.
            unsafe {
                assert!(IsWindowVisible(self.popup.hwnd).as_bool());
                let hit = SendMessageW(self.popup.hwnd, WM_NCHITTEST, None, Some(coords));
                assert_eq!(hit, LRESULT(HTCLIENT as isize));
                SendMessageW(
                    self.popup.hwnd,
                    WM_SETCURSOR,
                    Some(WPARAM(self.popup.hwnd.0 as usize)),
                    Some(LPARAM((HTCLIENT | (WM_MOUSEMOVE << 16)) as isize)),
                );
                assert_eq!(GetCursor(), LoadCursorW(None, expected).unwrap());
            }
        }
        self.hide();
        // SAFETY: Query the overlay's visibility before its window is destroyed.
        assert!(!unsafe { IsWindowVisible(self.popup.hwnd) }.as_bool());
    }
}

impl Drop for CursorOverlay {
    fn drop(&mut self) {
        self.hide();
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: This popup has no painted content. Win32 supplies the class
    // cursor when the pointer enters it.
    unsafe {
        match message {
            WM_ERASEBKGND => return LRESULT(1),
            WM_NCPAINT => return LRESULT(0),
            WM_PAINT => {
                let _ = ValidateRect(Some(window), None);
                return LRESULT(0);
            }
            WM_MOUSEMOVE if IsWindowVisible(window).as_bool() => {
                let cursor = GetClassLongPtrW(window, GCLP_HCURSOR);
                if cursor != 0 {
                    SetCursor(Some(HCURSOR(std::ptr::with_exposed_provenance_mut(cursor))));
                }
            }
            _ => (),
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}
