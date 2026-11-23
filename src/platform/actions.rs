use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_SUCCESS, GetLastError, HWND, SetLastError};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsIconic,
    IsZoomed, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE,
    SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindowAsync, WS_CAPTION, WS_EX_TOPMOST, WS_SYSMENU, WS_THICKFRAME,
};
use windows::core::{Error, HSTRING};

use super::com::{Apartment, TaskMem};

use super::process::ProcessImage;
use super::window::{WindowTarget, other_windows, window_work_area};
use crate::geometry::DragMode;

#[derive(Clone, Copy)]
pub(super) enum WindowAction {
    AlwaysOnTop,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "hidden from the window actions menu")
    )]
    Borderless,
    Center,
    Minimize,
    MaximizeRestore,
    MinimizeOthers,
    OpenFileLocation,
}

const FRAME: u32 = WS_CAPTION.0 | WS_THICKFRAME.0;
const WINDOW_CLOSED: &str = "The selected window closed before the action could run.";
const RESTORE_WINDOW_WAIT: Duration = Duration::from_millis(250);
const RESTORE_BATCH_WAIT: Duration = Duration::from_millis(750);
const RESTORE_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(super) const RESTORE_ORDER_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Default)]
pub(super) struct WindowActions {
    removed_frames: HashMap<WindowTarget, u32>,
    minimized: Option<MinimizedWindows>,
    restore_order: Option<RestoreOrder>,
}

pub(super) struct WindowMenuState {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "hidden from the window actions menu")
    )]
    pub(super) borderless: bool,
    pub(super) other_windows_minimized: bool,
}

impl WindowActions {
    pub(super) fn menu_state(&mut self, target: WindowTarget) -> WindowMenuState {
        WindowMenuState {
            borderless: self.removed_frames.contains_key(&target),
            other_windows_minimized: target.root_owner().is_some_and(|anchor| {
                Self::restorable_batch(&mut self.minimized, anchor).is_some()
            }),
        }
    }

    fn restorable_batch(
        minimized: &mut Option<MinimizedWindows>,
        anchor: WindowTarget,
    ) -> Option<&mut MinimizedWindows> {
        minimized.take_if(|batch| {
            batch.windows.retain_mut(MinimizedWindow::refresh);
            batch.windows.is_empty()
        });
        minimized
            .as_mut()
            .filter(|batch| batch.can_restore_from(anchor))
    }

    pub(super) fn apply(
        &mut self,
        target: WindowTarget,
        action: WindowAction,
    ) -> Result<(), String> {
        self.removed_frames
            .retain(|target, _| target.still_matches());
        if !target.still_matches() {
            return Err(WINDOW_CLOSED.into());
        }
        if !matches!(action, WindowAction::OpenFileLocation) && !target.is_responsive() {
            return Err(
                "The selected window did not respond. It may be busy or protected by Windows."
                    .into(),
            );
        }
        let window = target.window();
        // SAFETY: The target identity was validated. Position and show-state
        // changes are posted to the target thread without passing any pointers.
        let changed = match action {
            WindowAction::AlwaysOnTop => return toggle_topmost(target),
            WindowAction::OpenFileLocation => return open_file_location(target),
            WindowAction::Borderless => self.toggle_frame(target),
            WindowAction::Center => center(target),
            WindowAction::Minimize => unsafe { ShowWindowAsync(window, SW_MINIMIZE).as_bool() },
            WindowAction::MinimizeOthers => {
                return self.toggle_other_windows(target, other_windows);
            }
            WindowAction::MaximizeRestore => unsafe {
                let command = if IsZoomed(window).as_bool() || IsIconic(window).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                ShowWindowAsync(window, command).as_bool()
            },
        };
        changed.then_some(()).ok_or_else(|| {
            "Could not change the selected window. It may have closed or blocked the action.".into()
        })
    }

    fn toggle_other_windows(
        &mut self,
        target: WindowTarget,
        candidates: impl FnOnce(WindowTarget) -> windows::core::Result<Vec<WindowTarget>>,
    ) -> Result<(), String> {
        let anchor = target.root_owner().ok_or(WINDOW_CLOSED)?;
        let result = if let Some(batch) = Self::restorable_batch(&mut self.minimized, anchor) {
            let order = self
                .restore_order
                .get_or_insert_with(|| RestoreOrder::new(anchor));
            order.anchor = anchor;
            batch.restore(order, target)
        } else {
            let candidates = candidates(anchor)
                .map_err(|error| format!("Could not list other windows.\n\n{error}"))?;
            if !target.still_matches() || !anchor.still_matches() {
                return Err(WINDOW_CLOSED.into());
            }
            self.restore_order = None;
            focus_window(target);
            let batch = self.minimized.get_or_insert_with(|| MinimizedWindows {
                anchor,
                windows: Vec::new(),
            });
            batch.anchor = anchor;
            batch.minimize(candidates)
        };
        self.minimized.take_if(|batch| batch.windows.is_empty());
        result
    }

    pub(super) fn advance_restore_order(&mut self) -> bool {
        self.restore_order.take_if(|order| !order.advance());
        self.restore_order.is_some()
    }

    fn toggle_frame(&mut self, target: WindowTarget) -> bool {
        let window = target.window();
        if let Some(&bits) = self.removed_frames.get(&target) {
            let restored = restore_frame(target, bits);
            if restored {
                self.removed_frames.remove(&target);
            }
            return restored;
        }

        // SAFETY: Read the target's Win32 style. Keep its system-menu style so
        // it remains eligible for the window actions menu.
        let style = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
        if style & WS_SYSMENU.0 == 0 || style & FRAME == 0 {
            return false;
        }
        if !write_style(window, style & !FRAME) {
            return false;
        }

        // Keep the record even if the subsequent frame refresh fails.
        self.removed_frames.insert(target, style & FRAME);
        refresh_frame(target)
    }

    pub(super) fn restore_all(self) -> bool {
        let mut restored = true;
        for (target, bits) in self.removed_frames {
            if target.still_matches() && !(target.is_responsive() && restore_frame(target, bits)) {
                restored = false;
            }
        }
        restored
    }
}

struct MinimizedWindows {
    anchor: WindowTarget,
    windows: Vec<MinimizedWindow>,
}

impl MinimizedWindows {
    fn can_restore_from(&self, anchor: WindowTarget) -> bool {
        self.anchor == anchor || !self.anchor.still_matches()
    }

    fn minimize(&mut self, candidates: Vec<WindowTarget>) -> Result<(), String> {
        let mut all_queued = true;
        for target in candidates {
            if self.windows.iter().any(|window| window.target == target)
                || !target.is_minimize_candidate()
            {
                continue;
            }
            // SAFETY: Post to the validated app window without waiting for its thread.
            if unsafe { ShowWindowAsync(target.window(), SW_SHOWMINNOACTIVE).as_bool() } {
                self.windows.push(MinimizedWindow {
                    target,
                    observed_minimized: target.is_minimized(),
                });
            } else {
                all_queued = false;
            }
        }
        all_queued
            .then_some(())
            .ok_or_else(|| "Some windows could not be minimized.".into())
    }

    fn restore(&mut self, order: &mut RestoreOrder, target: WindowTarget) -> Result<(), String> {
        let deadline = Instant::now() + RESTORE_BATCH_WAIT;
        let mut rejected = false;
        for window in &self.windows {
            if !order.windows.contains(&window.target) {
                order.windows.push(window.target);
            }
        }
        self.windows
            .retain_mut(|window| match window.restore(deadline) {
                Ok(pending) => pending,
                Err(()) => {
                    rejected = true;
                    true
                }
            });
        set_window_level(order.anchor, HWND_TOP);
        focus_window(target);
        if rejected {
            return Err("Windows could not start restoring some windows.".into());
        }
        Ok(())
    }
}

struct MinimizedWindow {
    target: WindowTarget,
    observed_minimized: bool,
}

impl MinimizedWindow {
    fn refresh(&mut self) -> bool {
        if !self.target.is_visible() {
            return false;
        }
        if self.target.is_minimized() {
            self.observed_minimized = true;
            return true;
        }
        // A queued minimize can still run. Keep it eligible for an undo on the same queue.
        !self.observed_minimized && self.target.still_matches()
    }

    fn restore(&mut self, batch_deadline: Instant) -> Result<bool, ()> {
        if !self.refresh() {
            return Ok(false);
        }
        // SAFETY: Post to the validated app window without activating it.
        if !unsafe { ShowWindowAsync(self.target.window(), SW_SHOWNOACTIVATE).as_bool() } {
            return if self.refresh() { Err(()) } else { Ok(false) };
        }
        // Restore front to back even across separate queues, within a shared wait budget.
        let deadline = (Instant::now() + RESTORE_WINDOW_WAIT).min(batch_deadline);
        while self.target.is_minimized() && Instant::now() < deadline {
            thread::sleep(RESTORE_POLL_INTERVAL);
        }
        self.observed_minimized = self.target.is_minimized();
        Ok(self.observed_minimized)
    }
}

struct RestoreOrder {
    anchor: WindowTarget,
    windows: Vec<WindowTarget>,
    placing: Option<(WindowTarget, WindowTarget)>,
}

impl RestoreOrder {
    fn new(anchor: WindowTarget) -> Self {
        Self {
            anchor,
            windows: Vec::new(),
            placing: None,
        }
    }

    fn advance(&mut self) -> bool {
        if let Some((window, ahead)) = self.placing
            && [window, ahead].into_iter().all(|window| {
                window.still_matches() && window.is_visible() && !window.is_minimized()
            })
            && let Ok(topmost) = read_topmost(window.window())
            && read_topmost(ahead.window()).ok() == Some(topmost)
            && !ahead.is_above(window)
        {
            return true;
        }
        self.placing = None;

        let mut preceding = [None; 2];
        if self.anchor.still_matches()
            && let Ok(topmost) = read_topmost(self.anchor.window())
        {
            preceding[usize::from(topmost)] = Some(self.anchor);
        }
        self.windows
            .retain(|window| window.still_matches() && window.is_visible());
        let mut pending = false;
        for &window in &self.windows {
            if window.is_minimized() {
                pending = true;
                continue;
            }
            let Ok(topmost) = read_topmost(window.window()) else {
                continue;
            };
            let previous = &mut preceding[usize::from(topmost)];
            // A different queue can finish restoring later. Use the recorded predecessor,
            // and confirm the position before making another cross-queue change.
            if let Some(ahead) = *previous
                && !ahead.is_above(window)
                && set_window_level(window, ahead.window())
            {
                self.placing = Some((window, ahead));
                return true;
            }
            *previous = Some(window);
        }
        pending
    }
}

fn focus_window(target: WindowTarget) {
    if target.still_matches() {
        // SAFETY: The handle identifies the selected window.
        let _ = unsafe { SetForegroundWindow(target.window()) };
    }
}

fn set_window_level(window: WindowTarget, after: HWND) -> bool {
    window.still_matches() && {
        // SAFETY: Only change the z-order of the validated window.
        unsafe {
            SetWindowPos(
                window.window(),
                Some(after),
                0,
                0,
                0,
                0,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            )
        }
        .is_ok()
    }
}

fn open_file_location(target: WindowTarget) -> Result<(), String> {
    let mut image = ProcessImage::default();
    let path = image
        .read(target.process_id())
        .map_err(|error| format!("Could not read the window's executable path.\n\n{error}"))?;
    if !target.still_matches() {
        return Err("The selected window closed before its file location could open.".into());
    }
    let path = HSTRING::from_wide(path);

    let _apartment = Apartment::initialize()
        .map_err(|error| format!("Could not initialize File Explorer integration.\n\n{error}"))?;
    // SAFETY: Explorer allocates the absolute item ID using the COM task allocator.
    // Keep the allocation live until the Shell call returns.
    let opened = unsafe {
        let mut item = std::ptr::null_mut();
        SHParseDisplayName(&path, None::<&IBindCtx>, &mut item, 0, None).and_then(|()| {
            let item = TaskMem::from_raw(item);
            SHOpenFolderAndSelectItems(item.as_ptr(), None, 0)
        })
    };
    opened.map_err(|error| format!("Could not select the executable in File Explorer.\n\n{error}"))
}

pub(super) fn is_topmost(target: WindowTarget) -> bool {
    read_topmost(target.window()).unwrap_or(false)
}

fn read_topmost(window: HWND) -> windows::core::Result<bool> {
    // SAFETY: GWL_EXSTYLE is a read-only query of the opaque HWND. Zero can be
    // either a valid style or an error, so clear and inspect last error.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        if style == 0 && GetLastError() != ERROR_SUCCESS {
            return Err(Error::from_thread());
        }
        Ok(style as u32 & WS_EX_TOPMOST.0 != 0)
    }
}

fn toggle_topmost(target: WindowTarget) -> Result<(), String> {
    let window = target.window();
    let read_state = || {
        read_topmost(window)
            .map_err(|error| format!("Could not read the window's Always on top state.\n\n{error}"))
    };
    let pin = !read_state()?;
    let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
    let (order, flags) = if pin {
        // Explorer windows can require SWP_NOOWNERZORDER to pin.
        (HWND_TOPMOST, flags | SWP_NOOWNERZORDER)
    } else {
        (HWND_NOTOPMOST, flags)
    };
    if !target.still_matches() {
        return Err("The selected window closed before its z-order could change.".into());
    }
    // SAFETY: The worker checked the captured HWND. Change only z-order and
    // preserve focus, size, and position.
    unsafe { SetWindowPos(window, Some(order), 0, 0, 0, 0, flags) }
        .map_err(|error| format!("Windows rejected the Always on top request.\n\n{error}"))?;
    if read_state()? == pin {
        Ok(())
    } else {
        Err(
            "Windows accepted the request but the window's Always on top state did not change."
                .into(),
        )
    }
}

fn restore_frame(target: WindowTarget, removed: u32) -> bool {
    let window = target.window();
    // SAFETY: The caller checked the target window identity. Read its current
    // style so unrelated changes made since toggling are preserved.
    let current = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
    (current & removed == removed || write_style(window, current | removed))
        && refresh_frame(target)
}

fn write_style(window: HWND, style: u32) -> bool {
    // SAFETY: Set only the documented GWL_STYLE field. Win32 uses zero for both
    // a previous style value of zero and failure, so inspect last error.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let previous = SetWindowLongPtrW(window, GWL_STYLE, style as isize);
        previous != 0 || GetLastError() == ERROR_SUCCESS
    }
}

fn refresh_frame(target: WindowTarget) -> bool {
    let window = target.window();
    let flags = SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER;
    if !target.is_maximized() {
        // SAFETY: Recalculate the frame without moving or resizing the validated window.
        return unsafe {
            SetWindowPos(
                window,
                None,
                0,
                0,
                0,
                0,
                flags | SWP_FRAMECHANGED | SWP_NOSIZE,
            )
        }
        .is_ok();
    }
    let Some(bounds) = target.bounds() else {
        return false;
    };
    let width = bounds.x.length();
    let height = bounds.y.length();
    // SAFETY: Queue both sizes on the validated window's thread. A one-pixel
    // resize forces DWM to redraw the changed frame before we reset the size.
    unsafe {
        SetWindowPos(
            window,
            None,
            0,
            0,
            width,
            height.saturating_add(1),
            flags | SWP_FRAMECHANGED,
        )
        .is_ok()
            && target.still_matches()
            && SetWindowPos(window, None, 0, 0, width, height, flags).is_ok()
    }
}

fn center(target: WindowTarget) -> bool {
    if target.is_minimized() {
        return true;
    }
    let Some(area) = window_work_area(target.window()) else {
        return false;
    };
    if target.is_maximized() {
        // SAFETY: Post a native restore to the validated window without activating it.
        if !unsafe { ShowWindowAsync(target.window(), SW_SHOWNOACTIVATE).as_bool() } {
            return false;
        }
        // The normal size and visible borders are available after the restore runs.
        let deadline = Instant::now() + RESTORE_WINDOW_WAIT;
        while target.still_matches() && target.is_maximized() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(RESTORE_POLL_INTERVAL);
        }
        // The show-state bit can clear while the target is still processing the restore.
        if !target.is_responsive() {
            return false;
        }
        if target.is_minimized() {
            return true;
        }
    }
    let Some(frame) = target.frame() else {
        return false;
    };
    target.place(frame.centered_in(area), DragMode::Move)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Rect, WindowFrame};
    use std::fs;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, MSG, PM_REMOVE,
        PeekMessageW, PostMessageW, SW_HIDE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
        WS_EX_TOOLWINDOW, WS_MAXIMIZE, WS_MAXIMIZEBOX, WS_MINIMIZE, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };
    use windows::core::w;

    #[test]
    fn centers_the_visible_frame_in_a_negative_origin_work_area() {
        let outer = Rect::from_edges(12, 36, 222, 246);
        let visible = Rect::from_edges(17, 38, 217, 238);
        let area = Rect::from_edges(-1600, 40, 0, 1040);
        assert_eq!(
            WindowFrame { outer, visible }.centered_in(area),
            Rect::from_edges(-905, 438, -695, 648)
        );
    }

    #[test]
    fn centers_a_maximized_window_at_its_restored_size() {
        let window = WindowThread::new();
        let target = window.target;
        let normal = target.bounds().unwrap();
        window.maximize();
        let area = window_work_area(target.window()).unwrap();

        WindowActions::default()
            .apply(target, WindowAction::Center)
            .unwrap();
        wait_until(|| {
            target
                .frame()
                .is_some_and(|frame| frame.outer == frame.centered_in(area))
        });

        assert!(!target.is_maximized());
        let frame = target.frame().unwrap();
        assert_eq!(frame.outer.x.length(), normal.x.length());
        assert_eq!(frame.outer.y.length(), normal.y.length());
    }

    #[test]
    fn toggles_borderless_while_preserving_maximized_bounds() {
        let window = WindowThread::new();
        let target = window.target;
        let mut actions = WindowActions::default();
        window.maximize();
        let bounds = target.bounds().unwrap();

        actions.apply(target, WindowAction::Borderless).unwrap();
        window.flush_messages();
        assert!(actions.menu_state(target).borderless);
        assert!(target.accepts_window_actions());
        assert!(target.is_maximized());
        wait_until(|| target.bounds() == Some(bounds));
        assert_eq!(
            unsafe { GetWindowLongPtrW(target.window(), GWL_STYLE) } as u32 & FRAME,
            0
        );

        actions.apply(target, WindowAction::Borderless).unwrap();
        window.flush_messages();
        assert!(!actions.menu_state(target).borderless);
        assert!(target.is_maximized());
        wait_until(|| target.bounds() == Some(bounds));
        assert_eq!(
            unsafe { GetWindowLongPtrW(target.window(), GWL_STYLE) } as u32 & FRAME,
            FRAME
        );
    }

    #[test]
    fn borderless_can_toggle_and_restore_on_shutdown_without_overwriting_other_style_changes() {
        // SAFETY: This hidden native test window is owned by the test thread.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                40,
                50,
                240,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let target = WindowTarget::identify(window).unwrap();
        let mut actions = WindowActions::default();

        assert!(!actions.menu_state(target).borderless);
        actions.apply(target, WindowAction::Borderless).unwrap();
        assert!(actions.menu_state(target).borderless);
        let borderless = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
        assert_eq!(borderless & (FRAME | WS_SYSMENU.0), WS_SYSMENU.0);
        actions.apply(target, WindowAction::Borderless).unwrap();
        assert!(!actions.menu_state(target).borderless);
        assert_eq!(
            unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32 & FRAME,
            FRAME
        );

        actions.apply(target, WindowAction::Borderless).unwrap();
        assert!(write_style(window, borderless & !WS_MAXIMIZEBOX.0));
        assert!(actions.restore_all());
        assert_eq!(
            unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32 & (FRAME | WS_MAXIMIZEBOX.0),
            FRAME
        );
        unsafe { DestroyWindow(window) }.unwrap();
        assert!(!target.still_matches());
        assert!(
            WindowActions::default()
                .apply(target, WindowAction::AlwaysOnTop)
                .is_err()
        );
    }

    #[test]
    fn restores_only_the_windows_minimized_by_the_action() {
        let anchor = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
        let other = FixtureWindow::new(WS_VISIBLE | WS_MAXIMIZE, WINDOW_EX_STYLE::default(), None);
        let already_minimized =
            FixtureWindow::new(WS_VISIBLE | WS_MINIMIZE, WINDOW_EX_STYLE::default(), None);
        let hidden = FixtureWindow::new(WINDOW_STYLE::default(), WINDOW_EX_STYLE::default(), None);
        let hides_after_minimize = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
        let tool = FixtureWindow::new(WS_VISIBLE, WS_EX_TOOLWINDOW, None);
        let pinned = FixtureWindow::new(WS_VISIBLE, WS_EX_TOPMOST, None);
        let dialog = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), Some(anchor.0));
        let mut actions = WindowActions::default();

        assert!(!actions.menu_state(dialog.0).other_windows_minimized);
        actions
            .toggle_other_windows(dialog.0, |excluded| {
                assert!(excluded == anchor.0);
                Ok(vec![
                    other.0,
                    already_minimized.0,
                    hidden.0,
                    hides_after_minimize.0,
                    tool.0,
                    pinned.0,
                    dialog.0,
                ])
            })
            .unwrap();
        wait_until(|| {
            other.0.is_minimized()
                && pinned.0.is_minimized()
                && hides_after_minimize.0.is_minimized()
        });
        // SAFETY: This test owns the fixture that simulates an app hiding its window.
        assert!(unsafe { ShowWindowAsync(hides_after_minimize.0.window(), SW_HIDE).as_bool() });
        wait_until(|| !hides_after_minimize.0.is_visible());
        assert!(already_minimized.0.is_minimized());
        for window in [&anchor, &hidden, &tool, &dialog] {
            assert!(!window.0.is_minimized());
        }
        assert!(actions.menu_state(anchor.0).other_windows_minimized);
        assert!(actions.menu_state(dialog.0).other_windows_minimized);
        assert!(!actions.menu_state(other.0).other_windows_minimized);

        restore(&mut actions, dialog.0);
        wait_until(|| {
            !actions.advance_restore_order() && other.0.is_maximized() && !pinned.0.is_minimized()
        });
        assert!(already_minimized.0.is_minimized());
        assert!(!hides_after_minimize.0.is_visible());
        assert!(is_topmost(pinned.0));
        assert!(!actions.menu_state(anchor.0).other_windows_minimized);
    }

    #[test]
    fn restores_windows_in_order_across_separate_message_queues() {
        let anchor = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
        let first = WindowThread::new();
        let second = WindowThread::new();
        let mut actions = WindowActions::default();
        actions
            .toggle_other_windows(anchor.0, |_| Ok(vec![first.target, second.target]))
            .unwrap();
        wait_until(|| first.target.is_minimized() && second.target.is_minimized());

        restore(&mut actions, anchor.0);
        wait_until(|| {
            !actions.advance_restore_order()
                && !first.target.is_minimized()
                && !second.target.is_minimized()
                && anchor.0.is_above(first.target)
                && first.target.is_above(second.target)
        });
    }

    #[test]
    fn can_undo_a_minimize_while_the_window_queue_is_paused() {
        let anchor = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
        let peer = WindowThread::new();
        let pause = peer.pause();
        let mut actions = WindowActions::default();

        actions
            .toggle_other_windows(anchor.0, |_| Ok(vec![peer.target]))
            .unwrap();
        assert!(!peer.target.is_minimized());
        assert!(actions.menu_state(anchor.0).other_windows_minimized);
        restore(&mut actions, anchor.0);

        drop(pause);
        peer.flush_messages();
        assert!(!peer.target.is_minimized());
        assert!(!actions.menu_state(anchor.0).other_windows_minimized);
    }

    #[test]
    fn late_windows_keep_their_order_when_their_queues_resume_in_reverse() {
        let anchor = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
        let first = WindowThread::new();
        let second = WindowThread::new();
        let last = WindowThread::new();
        let mut actions = WindowActions::default();
        actions
            .toggle_other_windows(anchor.0, |_| {
                Ok(vec![first.target, second.target, last.target])
            })
            .unwrap();
        wait_until(|| {
            [first.target, second.target, last.target]
                .iter()
                .all(WindowTarget::is_minimized)
        });

        let first_pause = first.pause();
        let second_pause = second.pause();
        restore(&mut actions, anchor.0);
        assert!(first.target.is_minimized());
        assert!(second.target.is_minimized());
        assert!(actions.menu_state(anchor.0).other_windows_minimized);
        wait_until(|| !last.target.is_minimized());

        drop(second_pause);
        wait_until(|| {
            actions.advance_restore_order();
            !second.target.is_minimized()
                && anchor.0.is_above(second.target)
                && second.target.is_above(last.target)
        });

        drop(first_pause);
        wait_until(|| !actions.advance_restore_order() && !first.target.is_minimized());
        assert!(anchor.0.is_above(first.target));
        assert!(first.target.is_above(second.target));
        assert!(second.target.is_above(last.target));
        assert!(!actions.menu_state(anchor.0).other_windows_minimized);
    }

    fn restore(actions: &mut WindowActions, target: WindowTarget) {
        actions
            .toggle_other_windows(target, |_| Ok(Vec::new()))
            .unwrap();
    }

    const FIXTURE_TIMEOUT: Duration = Duration::from_secs(3);

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + FIXTURE_TIMEOUT;
        let mut message = MSG::default();
        while !ready() {
            assert!(Instant::now() < deadline, "window state did not change");
            // SAFETY: Read and dispatch only messages on this thread.
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    DispatchMessageW(&message);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct FixtureWindow(WindowTarget);

    impl FixtureWindow {
        fn new(
            style: WINDOW_STYLE,
            ex_style: WINDOW_EX_STYLE,
            owner: Option<WindowTarget>,
        ) -> Self {
            // SAFETY: The test creates and destroys the window on the owning thread.
            let window = unsafe {
                CreateWindowExW(
                    ex_style,
                    w!("STATIC"),
                    w!(""),
                    WS_OVERLAPPEDWINDOW | style,
                    40,
                    50,
                    240,
                    200,
                    owner.map(|owner| owner.window()),
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            Self(WindowTarget::identify(window).unwrap())
        }
    }

    impl Drop for FixtureWindow {
        fn drop(&mut self) {
            // SAFETY: The fixture never leaves the thread that owns its window.
            let _ = unsafe { DestroyWindow(self.0.window()) };
        }
    }

    struct WindowThread {
        target: WindowTarget,
        pause_requests: mpsc::Sender<(mpsc::Receiver<()>, mpsc::Sender<()>)>,
        owner: Option<thread::JoinHandle<()>>,
    }

    impl WindowThread {
        fn new() -> Self {
            let (ready, target) = mpsc::channel();
            let (pause_requests, pauses) =
                mpsc::channel::<(mpsc::Receiver<()>, mpsc::Sender<()>)>();
            let owner = thread::spawn(move || {
                let window = FixtureWindow::new(WS_VISIBLE, WINDOW_EX_STYLE::default(), None);
                ready.send(window.0).unwrap();
                let mut message = MSG::default();
                // SAFETY: This thread owns the fixture and pumps only its own queue.
                unsafe {
                    while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                        match message.message {
                            WM_CLOSE => break,
                            WM_APP => {
                                let (resume, paused) = pauses.recv().unwrap();
                                let _ = paused.send(());
                                let _ = resume.recv_timeout(FIXTURE_TIMEOUT);
                            }
                            _ => {
                                DispatchMessageW(&message);
                            }
                        }
                    }
                }
            });
            Self {
                target: target.recv_timeout(FIXTURE_TIMEOUT).unwrap(),
                pause_requests,
                owner: Some(owner),
            }
        }

        fn maximize(&self) {
            let normal = self.target.bounds().unwrap();
            // SAFETY: Queue a native maximize on the fixture's window thread.
            assert!(unsafe { ShowWindowAsync(self.target.window(), SW_MAXIMIZE).as_bool() });
            wait_until(|| {
                self.target.is_maximized()
                    && self.target.bounds().is_some_and(|bounds| bounds != normal)
            });
        }

        fn pause(&self) -> mpsc::Sender<()> {
            let (resume, wait) = mpsc::channel();
            let (paused, ready) = mpsc::channel();
            self.pause_requests.send((wait, paused)).unwrap();
            // SAFETY: The private message has no payload and targets only this fixture.
            unsafe { PostMessageW(Some(self.target.window()), WM_APP, WPARAM(0), LPARAM(0)) }
                .unwrap();
            ready.recv_timeout(FIXTURE_TIMEOUT).unwrap();
            resume
        }

        fn flush_messages(&self) {
            drop(self.pause());
        }
    }

    impl Drop for WindowThread {
        fn drop(&mut self) {
            // SAFETY: Close only the window created by this fixture thread.
            let _ =
                unsafe { PostMessageW(Some(self.target.window()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            if let Some(owner) = self.owner.take() {
                let _ = owner.join();
            }
        }
    }

    const FIXTURE_PATH: &str = "WINDOW_GESTURES_TEST_WINDOW_PATH";

    struct Fixture(Child);

    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn toggles_topmost_on_a_window_owned_by_another_process() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("window");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "platform::actions::tests::topmost_fixture",
            ])
            .env(FIXTURE_PATH, &path)
            .stdout(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .unwrap();
        let mut child = Fixture(child);
        let timeout = Instant::now() + Duration::from_secs(4);
        let window = loop {
            if let Ok(text) = fs::read_to_string(&path)
                && let Ok(handle) = text.parse::<usize>()
            {
                break HWND(std::ptr::with_exposed_provenance_mut(handle));
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "test window process exited early"
            );
            assert!(Instant::now() < timeout, "test window did not become ready");
            thread::sleep(Duration::from_millis(10));
        };
        let target = WindowTarget::identify(window).unwrap();
        assert_eq!(target.process_id(), child.0.id());
        let mut actions = WindowActions::default();

        assert!(!is_topmost(target));
        actions
            .apply(target, WindowAction::AlwaysOnTop)
            .expect("pin window from another process");
        assert!(is_topmost(target));
        actions
            .apply(target, WindowAction::AlwaysOnTop)
            .expect("unpin window from another process");
        assert!(!is_topmost(target));

        // SAFETY: Close only the fixture that this test launched.
        unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) }.unwrap();
    }

    #[test]
    #[ignore = "launched by the cross-process test"]
    fn topmost_fixture() {
        let Some(path) = std::env::var_os(FIXTURE_PATH) else {
            return;
        };
        // SAFETY: The fixture creates a hidden native window and pumps only its own queue.
        unsafe {
            let window = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                40,
                50,
                240,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            fs::write(path, (window.0 as usize).to_string()).unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 && message.message != WM_CLOSE {
                DispatchMessageW(&message);
            }
            DestroyWindow(window).unwrap();
        }
    }
}
