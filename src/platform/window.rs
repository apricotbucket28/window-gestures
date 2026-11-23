use std::collections::HashSet;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GW_HWNDPREV, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor,
    GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    IsZoomed, MINMAXINFO, SM_CXMAXTRACK, SM_CXMINTRACK, SM_CYMAXTRACK, SM_CYMINTRACK,
    SMTO_ABORTIFHUNG, SMTO_BLOCK, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    SendMessageTimeoutW, SetWindowPos, WM_GETMINMAXINFO, WM_NULL, WS_CAPTION, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_MAXIMIZE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
    WindowFromPoint,
};
use windows::core::BOOL;

use crate::geometry::{DragMode, Point, Rect, SizeLimits, WindowFrame};
use crate::settings::WindowPermissions;

const WINDOW_TIMEOUT_MS: u32 = 40;

impl From<POINT> for Point {
    fn from(point: POINT) -> Self {
        Self {
            x: point.x,
            y: point.y,
        }
    }
}

impl From<Point> for POINT {
    fn from(point: Point) -> Self {
        Self {
            x: point.x,
            y: point.y,
        }
    }
}

impl From<RECT> for Rect {
    fn from(rect: RECT) -> Self {
        Self::from_edges(rect.left, rect.top, rect.right, rect.bottom)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct WindowTarget {
    // This is an opaque OS identifier, transported as an integer across threads.
    handle: usize,
    process_id: u32,
    thread_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GestureAccess {
    Standard,
    MoveOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DragState {
    Normal,
    Maximized,
}

#[derive(Clone, Copy)]
struct WindowStyle(u32);

impl WindowStyle {
    fn read(window: HWND) -> Self {
        // SAFETY: This only reads the style of the opaque handle.
        Self(unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32)
    }

    fn has_caption(self) -> bool {
        self.0 & WS_CAPTION.0 == WS_CAPTION.0
    }

    fn supports_standard_gestures(self) -> bool {
        self.has_caption() || self.0 & WS_SYSMENU.0 != 0 || self.is_sizable()
    }

    fn is_sizable(self) -> bool {
        self.0 & WS_THICKFRAME.0 != 0
    }

    fn is_maximized(self) -> bool {
        self.0 & WS_MAXIMIZE.0 != 0
    }

    fn can_maximize(self) -> bool {
        self.supports_standard_gestures() && self.0 & WS_MAXIMIZEBOX.0 != 0
    }
}

impl WindowTarget {
    pub(super) fn process_id(&self) -> u32 {
        self.process_id
    }

    pub(super) fn at(point: Point) -> Option<Self> {
        // SAFETY: These HWNDs are opaque, and the PID buffer is valid for the call.
        // WindowFromPoint cannot send WM_NCHITTEST to windows on other threads.
        // The hook hides its own cursor popup before a button hit test.
        unsafe {
            let hit = WindowFromPoint(point.into());
            if hit.is_invalid() {
                return None;
            }
            let window = GetAncestor(hit, GA_ROOT);
            if window.is_invalid()
                || window == GetShellWindow()
                || window == GetDesktopWindow()
                || is_shell_panel(window)
            {
                return None;
            }
            Self::identify(window).filter(|target| target.process_id != std::process::id())
        }
    }

    pub(super) fn gesture_access(&self) -> Option<GestureAccess> {
        // SAFETY: This reads the foreground handle from the current desktop.
        self.gesture_access_with(|| unsafe { GetForegroundWindow() })
    }

    fn gesture_access_with(&self, foreground: impl FnOnce() -> HWND) -> Option<GestureAccess> {
        let style = self.style()?;
        let access = if style.supports_standard_gestures() {
            GestureAccess::Standard
        } else if self.window() == foreground() && !self.belongs_to_shell() {
            GestureAccess::MoveOnly
        } else {
            return None;
        };
        (!is_fullscreen(self.window(), style)).then_some(access)
    }

    pub(super) fn accepts_window_actions(&self) -> bool {
        self.gesture_style()
            .is_some_and(WindowStyle::supports_standard_gestures)
    }

    fn gesture_style(&self) -> Option<WindowStyle> {
        self.style()
            .filter(|&style| !is_fullscreen(self.window(), style))
    }

    fn style(&self) -> Option<WindowStyle> {
        self.still_matches()
            .then(|| WindowStyle::read(self.window()))
    }

    fn belongs_to_shell(&self) -> bool {
        // SAFETY: This reads the shell handle from the current desktop.
        Self::identify(unsafe { GetShellWindow() })
            .is_some_and(|shell| shell.process_id == self.process_id)
    }

    pub(super) fn identify(window: HWND) -> Option<Self> {
        let mut process_id = 0;
        // SAFETY: The HWND is opaque and the PID buffer is valid for the call.
        let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
        (thread_id != 0 && process_id != 0).then_some(Self {
            handle: window.0 as usize,
            process_id,
            thread_id,
        })
    }

    pub(super) fn drag_state(
        &self,
        mode: DragMode,
        permissions: WindowPermissions,
    ) -> Option<DragState> {
        let style = self.gesture_style()?;
        // SAFETY: IsIconic queries the show state of the validated handle.
        if unsafe { IsIconic(self.window()).as_bool() } {
            return None;
        }
        let allowed_when_maximized = match mode {
            DragMode::Move => permissions.move_maximized,
            DragMode::Resize if style.is_sizable() => permissions.resize_maximized,
            DragMode::Resize => return None,
        };
        if style.is_maximized() {
            allowed_when_maximized.then_some(DragState::Maximized)
        } else {
            Some(DragState::Normal)
        }
    }

    pub(super) fn is_maximized(&self) -> bool {
        // SAFETY: IsZoomed queries the show state of the opaque target handle.
        unsafe { IsZoomed(self.window()).as_bool() }
    }

    pub(super) fn root_owner(&self) -> Option<Self> {
        if !self.still_matches() {
            return None;
        }
        let mut window = self.window();
        // SAFETY: The targets are top-level windows. GW_OWNER follows owners of both
        // overlapped windows and popups.
        while let Ok(owner) = unsafe { GetWindow(window, GW_OWNER) } {
            window = owner;
        }
        if window == self.window() {
            Some(*self)
        } else {
            Self::identify(window)
        }
    }

    pub(super) fn is_minimized(&self) -> bool {
        // SAFETY: IsIconic queries the show state of the opaque target handle.
        self.still_matches() && unsafe { IsIconic(self.window()).as_bool() }
    }

    pub(super) fn is_visible(&self) -> bool {
        // SAFETY: IsWindowVisible only reads the visibility style of this opaque handle.
        unsafe { IsWindowVisible(self.window()).as_bool() }
    }

    pub(super) fn is_above(&self, other: Self) -> bool {
        let mut window = other.window();
        let mut visited = HashSet::new();
        while visited.insert(window.0) {
            // SAFETY: GW_HWNDPREV only reads the current top-level z-order.
            let Ok(previous) = (unsafe { GetWindow(window, GW_HWNDPREV) }) else {
                break;
            };
            if previous == self.window() {
                return true;
            }
            window = previous;
        }
        false
    }

    pub(super) fn is_minimize_candidate(&self) -> bool {
        let window = self.window();
        // SAFETY: These calls only read styles and ownership.
        self.still_matches()
            && unsafe {
                GetWindow(window, GW_OWNER).is_err()
                    && GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_MINIMIZEBOX.0 != 0
                    && GetWindowLongPtrW(window, GWL_EXSTYLE) as u32
                        & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0
                        == 0
            }
            && is_visible_on_desktop(window)
    }

    pub(super) fn is_sizable(&self) -> bool {
        self.style().is_some_and(WindowStyle::is_sizable)
    }

    pub(super) fn can_maximize(&self) -> bool {
        self.gesture_style().is_some_and(WindowStyle::can_maximize)
    }

    pub(super) fn window(&self) -> HWND {
        HWND(std::ptr::with_exposed_provenance_mut(self.handle))
    }

    pub(super) fn still_matches(&self) -> bool {
        Self::identify(self.window()) == Some(*self)
    }

    pub(super) fn bounds(&self) -> Option<Rect> {
        self.still_matches()
            .then(|| window_rect(self.window()))
            .flatten()
    }

    pub(super) fn frame(&self) -> Option<WindowFrame> {
        self.still_matches()
            .then(|| window_frame(self.window()))
            .flatten()
    }

    #[cfg(test)]
    pub(super) fn thread_id(&self) -> u32 {
        self.thread_id
    }

    pub(super) fn dpi(&self) -> u32 {
        // SAFETY: The HWND is opaque. A stale handle returns zero.
        unsafe { GetDpiForWindow(self.window()) }.max(96)
    }

    pub(super) fn tracking_limits(&self, mode: DragMode) -> (SizeLimits, SizeLimits) {
        let mut info = MINMAXINFO::default();
        if mode == DragMode::Resize {
            // SAFETY: These are DPI metric queries and the system marshals the
            // MINMAXINFO for WM_GETMINMAXINFO across process boundaries.
            unsafe {
                let dpi = self.dpi();
                info.ptMinTrackSize = POINT {
                    x: GetSystemMetricsForDpi(SM_CXMINTRACK, dpi),
                    y: GetSystemMetricsForDpi(SM_CYMINTRACK, dpi),
                };
                info.ptMaxTrackSize = POINT {
                    x: GetSystemMetricsForDpi(SM_CXMAXTRACK, dpi),
                    y: GetSystemMetricsForDpi(SM_CYMAXTRACK, dpi),
                };
                if self.process_id != std::process::id() && self.still_matches() {
                    let mut limits = info;
                    if SendMessageTimeoutW(
                        self.window(),
                        WM_GETMINMAXINFO,
                        WPARAM(0),
                        LPARAM(&mut limits as *mut MINMAXINFO as isize),
                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                        WINDOW_TIMEOUT_MS,
                        None,
                    )
                    .0 != 0
                    {
                        info = limits;
                    }
                }
            }
        }
        (
            SizeLimits::new(info.ptMinTrackSize.x, info.ptMaxTrackSize.x),
            SizeLimits::new(info.ptMinTrackSize.y, info.ptMaxTrackSize.y),
        )
    }

    pub(super) fn is_responsive(&self) -> bool {
        if !self.still_matches() {
            return false;
        }
        // SAFETY: WM_NULL carries no pointers and the worker has a separate input queue.
        let replied = unsafe {
            SendMessageTimeoutW(
                self.window(),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                WINDOW_TIMEOUT_MS,
                None,
            )
            .0 != 0
        };
        replied && self.still_matches()
    }

    pub(super) fn place(&self, rect: Rect, mode: DragMode) -> bool {
        if !self.is_responsive() {
            return false;
        }
        // Wait for each resize before the worker samples the latest pointer
        // position, so there are no stale sizes queued on the target thread.
        let mut flags = SWP_NOACTIVATE | SWP_NOZORDER;
        if mode == DragMode::Move {
            flags |= SWP_ASYNCWINDOWPOS | SWP_NOSIZE;
        }
        // SAFETY: The preflight checked the window's identity and responsiveness.
        // Positioning preserves activation and z-order.
        unsafe {
            self.still_matches()
                && SetWindowPos(
                    self.window(),
                    None,
                    rect.x.start,
                    rect.y.start,
                    rect.x.length(),
                    rect.y.length(),
                    flags,
                )
                .is_ok()
        }
    }
}

fn is_shell_panel(window: HWND) -> bool {
    const CLASSES: &[&str] = &[
        // Taskbar and classic overflow classes from AltSnap's default exclusions.
        // https://github.com/RamonUnch/AltSnap/blob/main/AltSnap.dni
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "NotifyIconOverflowWindow",
        // Windows 11 flyout classes identified by Windhawk's Taskbar multi-tray mod.
        // https://github.com/ramensoftware/windhawk-mods/blob/main/mods/taskbar-multi-tray.wh.cpp
        "TopLevelWindowForOverflowXamlIsland",
        "ControlCenterWindow",
    ];

    let mut class = [0; 256];
    // SAFETY: The handle is opaque and the buffer is writable for the call.
    let length = unsafe { GetClassNameW(window, &mut class) } as usize;
    CLASSES
        .iter()
        .any(|name| name.encode_utf16().eq(class[..length].iter().copied()))
}

fn is_fullscreen(window: HWND, style: WindowStyle) -> bool {
    // A native maximized window can fill the monitor even with a custom frame.
    if style.is_maximized() {
        return false;
    }
    let (Some(areas), Some(outer)) = (Monitor::for_window(window).areas(), window_rect(window))
    else {
        return false;
    };
    let screen = areas.screen;
    outer == screen
        || (outer.x.start <= screen.x.start
            && outer.x.end >= screen.x.end
            && outer.y.start <= screen.y.start
            && outer.y.end >= screen.y.end
            && visible_frame(window, outer) == screen)
}

pub(super) fn window_rect(window: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    // SAFETY: The HWND is opaque and the RECT is a writable output parameter.
    unsafe { GetWindowRect(window, &mut rect) }.ok()?;
    let rect = Rect::from(rect);
    rect.has_area().then_some(rect)
}

fn visible_frame(window: HWND, fallback: Rect) -> Rect {
    let mut rect = RECT::default();
    // SAFETY: The requested attribute writes one RECT into the supplied buffer.
    if unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    {
        let visible = Rect::from(rect);
        if visible.has_area() {
            return visible;
        }
    }
    fallback
}

pub(super) fn window_frame(window: HWND) -> Option<WindowFrame> {
    let outer = window_rect(window)?;
    Some(WindowFrame {
        outer,
        visible: visible_frame(window, outer),
    })
}

pub(super) fn work_area(point: Point) -> Option<Rect> {
    Some(Monitor::at(point).areas()?.work)
}

pub(super) fn window_work_area(window: HWND) -> Option<Rect> {
    Some(Monitor::for_window(window).areas()?.work)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Monitor(HMONITOR);

pub(super) struct MonitorAreas {
    pub(super) screen: Rect,
    pub(super) work: Rect,
}

impl Monitor {
    pub(super) fn at(point: Point) -> Self {
        // SAFETY: The point is in physical desktop coordinates.
        Self(unsafe { MonitorFromPoint(point.into(), MONITOR_DEFAULTTONEAREST) })
    }

    pub(super) fn for_window(window: HWND) -> Self {
        // SAFETY: MonitorFromWindow queries the opaque window handle.
        Self(unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) })
    }

    pub(super) fn areas(self) -> Option<MonitorAreas> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: The MONITORINFO structure has the documented size and is writable.
        let found = unsafe { GetMonitorInfoW(self.0, &mut info) };
        found.as_bool().then(|| MonitorAreas {
            screen: info.rcMonitor.into(),
            work: info.rcWork.into(),
        })
    }
}

fn is_visible_on_desktop(window: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: HWND is opaque. DWM writes a DWORD to the supplied buffer.
    unsafe {
        IsWindowVisible(window).as_bool()
            && !IsIconic(window).as_bool()
            && DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
            .is_ok()
            && cloaked == 0
    }
}

pub(super) fn other_windows(excluded: WindowTarget) -> windows::core::Result<Vec<WindowTarget>> {
    struct Enumeration {
        excluded: HWND,
        windows: Vec<WindowTarget>,
    }

    unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
        // SAFETY: EnumWindows invokes this callback synchronously with our state pointer.
        let state = unsafe { &mut *(data.0 as *mut Enumeration) };
        if window != state.excluded
            && let Some(target) = WindowTarget::identify(window)
            && target.process_id() != std::process::id()
        {
            state.windows.push(target);
        }
        true.into()
    }

    let mut state = Enumeration {
        excluded: excluded.window(),
        windows: Vec::new(),
    };
    // SAFETY: The enumeration is synchronous and the state outlives every callback.
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM(&mut state as *mut Enumeration as isize),
        )
    }?;
    Ok(state.windows)
}

pub(super) fn neighboring_frames(target: &WindowTarget) -> Vec<Rect> {
    other_windows(*target)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|target| {
            let window = target.window();
            if !WindowStyle::read(window).has_caption() || !is_visible_on_desktop(window) {
                return None;
            }
            Some(target.frame()?.visible)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::owned_window::OwnedWindow;
    use windows::Win32::Foundation::{HINSTANCE, LRESULT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
    };
    use windows::core::w;

    #[test]
    fn recognizes_the_hidden_icons_flyout_class() {
        unsafe extern "system" fn window_proc(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            // SAFETY: Forward the unchanged Win32 callback arguments.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }

        // SAFETY: The class name and window procedure outlive the hidden fixture.
        let window = unsafe {
            OwnedWindow::create(
                &WNDCLASSW {
                    hInstance: HINSTANCE(GetModuleHandleW(None).unwrap().0),
                    lpszClassName: w!("TopLevelWindowForOverflowXamlIsland"),
                    lpfnWndProc: Some(window_proc),
                    ..Default::default()
                },
                w!(""),
                WS_POPUP,
                WINDOW_EX_STYLE::default(),
            )
        }
        .unwrap();
        assert!(is_shell_panel(window.hwnd));
    }

    #[test]
    fn focused_frameless_window_can_move_at_its_original_size() {
        // SAFETY: Create a hidden fixture from a system class on the test thread.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                None,
                WS_POPUP,
                40,
                60,
                240,
                160,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let target = WindowTarget::identify(window).unwrap();

        assert!(!is_shell_panel(window));
        // Supply the foreground handle so the test does not steal desktop focus.
        assert_eq!(
            target.gesture_access_with(|| window),
            Some(GestureAccess::MoveOnly)
        );
        assert_eq!(target.gesture_access_with(HWND::default), None);
        assert!(!target.accepts_window_actions());
        let permissions = WindowPermissions::default();
        assert_eq!(
            target.drag_state(DragMode::Move, permissions),
            Some(DragState::Normal)
        );
        assert_eq!(target.drag_state(DragMode::Resize, permissions), None);
        assert!(target.place(Rect::from_edges(70, 90, 80, 100), DragMode::Move));
        assert_eq!(target.bounds(), Some(Rect::from_edges(70, 90, 310, 250)));

        // SAFETY: This thread owns the fixture.
        unsafe { DestroyWindow(window) }.unwrap();
    }
}
