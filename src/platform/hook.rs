use std::cell::RefCell;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_LMENU, VK_LWIN, VK_MBUTTON, VK_RBUTTON, VK_RMENU,
    VK_RWIN, keybd_event,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK, HOOKPROC,
    KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    SM_CXDOUBLECLK, SM_CXDRAG, SM_CYDOUBLECLK, SM_CYDRAG, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOWS_HOOK_ID, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::{Error, Result};

use super::AppResult;
use super::actions::WindowAction;
use super::cursor::{CursorOverlay, CursorShape};
use super::exclusions::Exclusions;
use super::window::{GestureAccess, WindowTarget};
use super::worker::{DragStart, MenuReservation, WindowCommands};
use crate::geometry::{DragMode, Point, ResizeHandle};
use crate::settings::{Modifier, Settings, WindowPermissions};

const SET_PAUSED: u32 = WM_APP + 1;

thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

pub(super) struct InputHooks {
    thread_id: u32,
    thread: JoinHandle<Result<()>>,
    pending_settings: Arc<Mutex<Option<InputSettings>>>,
}

impl InputHooks {
    pub(super) fn start(
        commands: WindowCommands,
        settings: Settings,
        post_menu: impl Fn(MenuRequest) + Send + 'static,
    ) -> AppResult<Self> {
        let (ready, started) = channel();
        let pending_settings = Arc::new(Mutex::new(None));
        let hook_settings = Arc::clone(&pending_settings);
        let thread = thread::Builder::new()
            .name("input-hooks".into())
            .spawn(move || {
                // SAFETY: Query the current executable's instance handle.
                let module = unsafe { HINSTANCE(GetModuleHandleW(None)?.0) };
                STATE.with(|slot| {
                    slot.replace(Some(HookState::new(
                        commands,
                        &settings,
                        hook_settings,
                        Box::new(post_menu),
                        CursorOverlay::create(module).ok(),
                    )));
                });

                let mut message = MSG::default();
                // SAFETY: This thread owns the message queue and both hooks. The
                // module belongs to this process, and both callbacks use the HOOKPROC ABI.
                unsafe {
                    // Force creation of the queue before publishing its thread ID.
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                    let keyboard =
                        NativeHook::install(WH_KEYBOARD_LL, Some(keyboard_hook), module)?;
                    let mouse = NativeHook::install(WH_MOUSE_LL, Some(mouse_hook), module)?;
                    let result = if ready.send(GetCurrentThreadId()).is_ok() {
                        loop {
                            match GetMessageW(&mut message, None, 0, 0).0 {
                                -1 => break Err(Error::from_thread()),
                                0 => break Ok(()),
                                _ if message.message == SET_PAUSED && message.hwnd.is_invalid() => {
                                    with_state(|state| state.set_paused(message.wParam.0 != 0));
                                }
                                _ => {
                                    DispatchMessageW(&message);
                                }
                            }
                        }
                    } else {
                        Ok(())
                    };
                    let unhooked_mouse = mouse.remove();
                    let unhooked_keyboard = keyboard.remove();
                    STATE.with(|slot| drop(slot.take()));
                    result.and(unhooked_mouse).and(unhooked_keyboard)
                }
            })?;

        match started.recv() {
            Ok(thread_id) => Ok(Self {
                thread_id,
                thread,
                pending_settings,
            }),
            Err(_) => match thread.join() {
                Ok(Err(error)) => Err(error.into()),
                _ => Err("The input hooks did not start.".into()),
            },
        }
    }

    pub(super) fn update_settings(&self, settings: &Settings) {
        let settings = InputSettings::new(settings);
        *self
            .pending_settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(settings);
    }

    pub(super) fn set_paused(&self, paused: bool) -> Result<()> {
        // SAFETY: The hook owns this message's destination and accepts an integer flag.
        unsafe {
            PostThreadMessageW(
                self.thread_id,
                SET_PAUSED,
                WPARAM(usize::from(paused)),
                LPARAM(0),
            )
        }
    }

    pub(super) fn stop(self) -> AppResult<()> {
        // SAFETY: Startup publishes the ID only after the message queue exists.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        let posted = match posted {
            Err(error) if !self.thread.is_finished() => return Err(error.into()),
            result => result,
        };
        let stopped = self
            .thread
            .join()
            .map_err(|_| "The input hook thread stopped unexpectedly.")?;
        stopped?;
        posted?;
        Ok(())
    }
}

struct NativeHook(Option<HHOOK>);

impl NativeHook {
    fn install(kind: WINDOWS_HOOK_ID, callback: HOOKPROC, module: HINSTANCE) -> Result<Self> {
        // SAFETY: The module belongs to this process and the callback uses the HOOKPROC ABI.
        unsafe { SetWindowsHookExW(kind, callback, Some(module), 0) }.map(|hook| Self(Some(hook)))
    }

    fn remove(mut self) -> Result<()> {
        self.unhook()
    }

    fn unhook(&mut self) -> Result<()> {
        self.0.take().map_or(Ok(()), |hook| {
            // SAFETY: The calling thread installed the hook and owns its handle.
            unsafe { UnhookWindowsHookEx(hook) }
        })
    }
}

impl Drop for NativeHook {
    fn drop(&mut self) {
        let _ = self.unhook();
    }
}

pub(super) struct MenuRequest {
    pub(super) target: WindowTarget,
    pub(super) point: Point,
    pub(super) reservation: MenuReservation,
}

struct HookState {
    commands: WindowCommands,
    post_menu: Box<dyn Fn(MenuRequest)>,
    settings: InputSettings,
    pending_settings: Arc<Mutex<Option<InputSettings>>>,
    paused: bool,
    captured: Option<MouseCapture>,
    last_click: Option<MovePress>,
    cursor: Option<CursorFeedback>,
    escape: EscapePress,
    gesture_keys: MenuKeys,
}

struct InputSettings {
    modifier: Modifier,
    snapping: bool,
    tiling: bool,
    exclusions: Exclusions,
    permissions: WindowPermissions,
}

impl InputSettings {
    fn new(settings: &Settings) -> Self {
        Self {
            modifier: settings.modifier,
            snapping: settings.snapping,
            tiling: settings.tiling,
            exclusions: Exclusions::new(&settings.excluded_apps),
            permissions: settings.window_permissions,
        }
    }
}

enum MouseCapture {
    PendingMove(MovePress),
    PendingMenu(MenuRequest),
    Drag(DragMode, CursorShape),
    ConsumeRelease(MouseButton),
}

impl MouseCapture {
    fn button(&self) -> MouseButton {
        match self {
            Self::PendingMove(_) | Self::Drag(DragMode::Move, _) => MouseButton::Left,
            Self::Drag(DragMode::Resize, _) => MouseButton::Right,
            Self::PendingMenu(_) => MouseButton::Middle,
            Self::ConsumeRelease(button) => *button,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn transition(message: u32) -> Option<(Self, bool)> {
        match message {
            WM_LBUTTONDOWN => Some((Self::Left, true)),
            WM_LBUTTONUP => Some((Self::Left, false)),
            WM_RBUTTONDOWN => Some((Self::Right, true)),
            WM_RBUTTONUP => Some((Self::Right, false)),
            WM_MBUTTONDOWN => Some((Self::Middle, true)),
            WM_MBUTTONUP => Some((Self::Middle, false)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct MovePress {
    target: WindowTarget,
    origin: Point,
    time: u32,
    drag_radius: (u32, u32),
    can_click: bool,
}

impl MovePress {
    fn moved(self, point: Point) -> bool {
        !within_radius(self.origin, point, self.drag_radius)
    }

    fn matches(self, next: Self, radius: (u32, u32), timeout: u32) -> bool {
        self.target == next.target
            && next.time.wrapping_sub(self.time) <= timeout
            && within_radius(self.origin, next.origin, radius)
    }
}

fn within_radius(origin: Point, point: Point, radius: (u32, u32)) -> bool {
    origin.x.abs_diff(point.x) <= radius.0 && origin.y.abs_diff(point.y) <= radius.1
}

struct CursorFeedback {
    overlay: CursorOverlay,
    target: Option<ReadyTarget>,
}

struct ReadyTarget {
    window: WindowTarget,
    access: GestureAccess,
    eligible: bool,
}

impl CursorFeedback {
    fn ready_shape(&mut self, point: Point, exclusions: &mut Exclusions) -> Option<CursorShape> {
        let target = if self.overlay.owns_point(point) {
            self.target.as_ref().map(|target| target.window)
        } else {
            WindowTarget::at(point)
        }
        .and_then(|window| Some((window, window.gesture_access()?)));
        if target
            != self
                .target
                .as_ref()
                .map(|target| (target.window, target.access))
        {
            self.target = target.map(|(window, access)| ReadyTarget {
                window,
                access,
                eligible: !exclusions.excludes(window.process_id(), access),
            });
        }
        self.target
            .as_ref()
            .is_some_and(|target| target.eligible)
            .then_some(CursorShape::Ready)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Forward,
    MaskMenuRelease,
    Consume,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapePress {
    Up,
    Forwarded,
    Consumed,
}

impl HookState {
    fn new(
        commands: WindowCommands,
        settings: &Settings,
        pending_settings: Arc<Mutex<Option<InputSettings>>>,
        post_menu: Box<dyn Fn(MenuRequest)>,
        cursor: Option<CursorOverlay>,
    ) -> Self {
        Self {
            commands,
            post_menu,
            settings: InputSettings::new(settings),
            pending_settings,
            paused: false,
            captured: None,
            last_click: None,
            cursor: cursor.map(|overlay| CursorFeedback {
                overlay,
                target: None,
            }),
            escape: EscapePress::Up,
            gesture_keys: MenuKeys::default(),
        }
    }

    fn cancel_drag_capture(&mut self) -> bool {
        let Some(capture) = self.captured.as_mut() else {
            return false;
        };
        let button = capture.button();
        if button == MouseButton::Middle {
            return false;
        }
        if matches!(capture, MouseCapture::Drag(..)) {
            self.commands.cancel_drag();
        }
        *capture = MouseCapture::ConsumeRelease(button);
        true
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.last_click = None;
            self.hide_cursor();
            if matches!(self.captured, Some(MouseCapture::PendingMenu(_))) {
                self.captured = Some(MouseCapture::ConsumeRelease(MouseButton::Middle));
            } else {
                self.cancel_drag_capture();
            }
        }
    }

    fn on_mouse(&mut self, message: u32, point: Point, time: u32) -> bool {
        if !matches!(message, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP) {
            self.break_click_sequence();
        }
        if message == WM_MOUSEMOVE {
            self.update_drag(point);
            return false;
        }
        let Some((button, down)) = MouseButton::transition(message) else {
            return false;
        };
        if let Some(capture) = &self.captured {
            if button != capture.button() {
                return false;
            }
            if !down {
                self.update_drag(point);
                match self.captured.take() {
                    Some(MouseCapture::Drag(..)) => self.commands.release_drag(point),
                    Some(MouseCapture::PendingMove(press))
                        if press.can_click
                            && !MenuKeys::held(self.settings.modifier).is_empty() =>
                    {
                        self.last_click = Some(press);
                    }
                    Some(MouseCapture::PendingMenu(request))
                        if request.target.accepts_window_actions() =>
                    {
                        (self.post_menu)(request);
                    }
                    _ => (),
                }
            }
            return true;
        }
        if !down || self.paused || !self.commands.is_idle() {
            if down {
                self.last_click = None;
            }
            return false;
        }
        self.begin(point, time, button)
    }

    fn begin(&mut self, point: Point, time: u32, button: MouseButton) -> bool {
        let gesture = self.gesture_target(point, button);
        let last_click = self.last_click.take();
        let Some((keys, target)) = gesture else {
            return false;
        };
        self.captured = Some(match button {
            MouseButton::Left => {
                let can_maximize = target.can_maximize();
                if !can_maximize
                    && target
                        .drag_state(DragMode::Move, self.settings.permissions)
                        .is_none()
                {
                    return false;
                }
                let dpi = target.dpi();
                // SAFETY: These calls read the desktop's click and drag preferences.
                let (drag_radius, double_radius, timeout) = unsafe {
                    let radius = |metric| GetSystemMetricsForDpi(metric, dpi).max(1) as u32 / 2;
                    (
                        (radius(SM_CXDRAG), radius(SM_CYDRAG)),
                        (radius(SM_CXDOUBLECLK), radius(SM_CYDOUBLECLK)),
                        GetDoubleClickTime(),
                    )
                };
                let press = MovePress {
                    target,
                    origin: point,
                    time,
                    drag_radius,
                    can_click: can_maximize,
                };
                if can_maximize
                    && last_click.is_some_and(|last| last.matches(press, double_radius, timeout))
                {
                    if !self
                        .commands
                        .run_action(target, WindowAction::MaximizeRestore)
                    {
                        return false;
                    }
                    MouseCapture::ConsumeRelease(button)
                } else {
                    MouseCapture::PendingMove(press)
                }
            }
            MouseButton::Right => {
                let Some(cursor) = self.start_drag(target, point, DragMode::Resize) else {
                    return false;
                };
                MouseCapture::Drag(DragMode::Resize, cursor)
            }
            MouseButton::Middle => {
                let Some(reservation) = self.commands.reserve_menu() else {
                    return false;
                };
                MouseCapture::PendingMenu(MenuRequest {
                    target,
                    point,
                    reservation,
                })
            }
        });
        self.gesture_keys.add(keys);
        true
    }

    fn update_drag(&mut self, point: Point) {
        match self.captured.as_ref() {
            Some(MouseCapture::PendingMove(press)) if press.moved(point) => {
                self.captured = Some(
                    if let Some(cursor) =
                        self.start_drag(press.target, press.origin, DragMode::Move)
                    {
                        self.commands.update_drag(point);
                        MouseCapture::Drag(DragMode::Move, cursor)
                    } else {
                        MouseCapture::ConsumeRelease(MouseButton::Left)
                    },
                );
            }
            Some(MouseCapture::Drag(..)) => self.commands.update_drag(point),
            _ => (),
        }
    }

    fn start_drag(
        &self,
        target: WindowTarget,
        origin: Point,
        mode: DragMode,
    ) -> Option<CursorShape> {
        target.drag_state(mode, self.settings.permissions)?;
        let resize = if mode == DragMode::Resize {
            Some(ResizeHandle::at(target.frame()?.visible, origin))
        } else {
            None
        };
        if !self.commands.start_drag(DragStart {
            target,
            origin,
            resize,
            snapping: self.settings.snapping,
            tiling: self.settings.tiling,
            permissions: self.settings.permissions,
        }) {
            return None;
        }
        Some(resize.map_or(CursorShape::Move, |handle| {
            CursorShape::Resize(handle.direction())
        }))
    }

    fn gesture_target(
        &mut self,
        point: Point,
        button: MouseButton,
    ) -> Option<(MenuKeys, WindowTarget)> {
        self.apply_pending_settings();
        let keys = MenuKeys::held(self.settings.modifier);
        if keys.is_empty() {
            return None;
        }
        let target = WindowTarget::at(point)?;
        let access = target.gesture_access()?;
        if access == GestureAccess::MoveOnly && button != MouseButton::Left {
            return None;
        }
        (!self
            .settings
            .exclusions
            .excludes(target.process_id(), access))
        .then_some((keys, target))
    }

    fn apply_pending_settings(&mut self) {
        // The worker's current drag already owns its configuration snapshot.
        if !self.commands.is_idle() || self.captured.is_some() {
            return;
        }
        let mut pending = match self.pending_settings.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if let Some(settings) = pending.take() {
            self.settings = settings;
            self.last_click = None;
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.target = None;
            }
        }
    }

    fn break_click_sequence(&mut self) {
        self.last_click = None;
        if let Some(MouseCapture::PendingMove(press)) = self.captured.as_mut() {
            press.can_click = false;
        }
    }

    fn hide_cursor(&mut self) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.overlay.hide();
        }
    }

    fn refresh_cursor(&mut self, point: Point, event: Option<(u32, bool)>) {
        if self.cursor.is_none() {
            return;
        }
        self.apply_pending_settings();
        let Some(cursor) = self.cursor.as_mut() else {
            return;
        };
        let shape = if self.paused {
            None
        } else {
            match self.captured.as_ref() {
                Some(MouseCapture::Drag(_, shape)) => Some(*shape),
                Some(MouseCapture::PendingMove(_)) => Some(CursorShape::Ready),
                None if self.commands.is_idle()
                    && !MenuKeys::held_with_event(self.settings.modifier, event).is_empty()
                    && ![VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
                        .into_iter()
                        .any(key_is_down) =>
                {
                    cursor.ready_shape(point, &mut self.settings.exclusions)
                }
                _ => None,
            }
        };
        if let Some(shape) = shape {
            cursor.overlay.show(point, shape);
        } else {
            cursor.overlay.hide();
        }
    }

    fn handle_mouse(&mut self, message: u32, point: Point, time: u32) -> bool {
        // Let the real window receive any button or wheel event that we forward.
        // Hiding first also lets the initial window hit test see through the overlay.
        if message != WM_MOUSEMOVE {
            self.hide_cursor();
        }
        let handled = self.on_mouse(message, point, time);
        if handled || message == WM_MOUSEMOVE {
            self.refresh_cursor(point, None);
        }
        handled
    }

    fn handle_key(&mut self, key: u32, up: bool) -> KeyAction {
        let action = self.on_key(key, up);
        if self.cursor.is_some() {
            if MenuKeys::bit(key).is_some()
                || key == u32::from(VK_ESCAPE.0)
                || key == u32::from(VK_CONTROL.0)
            {
                let mut point = POINT::default();
                // SAFETY: The pointer is writable for the duration of the desktop query.
                if unsafe { GetCursorPos(&mut point) }.is_ok() {
                    self.refresh_cursor(point.into(), Some((key, up)));
                }
            } else if !up && self.captured.is_none() {
                self.hide_cursor();
            }
        }
        action
    }

    fn on_key(&mut self, key: u32, up: bool) -> KeyAction {
        if key == u32::from(VK_ESCAPE.0) {
            self.break_click_sequence();
            let press = if up {
                std::mem::replace(&mut self.escape, EscapePress::Up)
            } else {
                if self.escape == EscapePress::Up {
                    self.escape = if self.cancel_drag_capture() {
                        EscapePress::Consumed
                    } else {
                        EscapePress::Forwarded
                    };
                }
                self.escape
            };
            return if press == EscapePress::Consumed {
                KeyAction::Consume
            } else {
                KeyAction::Forward
            };
        }
        if up {
            let modifier_released = MenuKeys::for_modifier(self.settings.modifier)
                .iter()
                .any(|&(vk, _)| u32::from(vk.0) == key);
            if modifier_released {
                self.break_click_sequence();
            }
            if self.gesture_keys.release(key) {
                return KeyAction::MaskMenuRelease;
            }
        }
        KeyAction::Forward
    }
}

fn with_state<T>(callback: impl FnOnce(&mut HookState) -> T) -> Option<T> {
    // Reentrant hooks forward input without accessing an in-progress gesture.
    STATE
        .try_with(|slot| {
            let mut state = slot.try_borrow_mut().ok()?;
            state.as_mut().map(callback)
        })
        .ok()
        .flatten()
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && data.0 != 0 {
        // SAFETY: For HC_ACTION the OS passes an MSLLHOOKSTRUCT valid for this call.
        let event = unsafe { &*(data.0 as *const MSLLHOOKSTRUCT) };
        let handled =
            with_state(|state| state.handle_mouse(message.0 as u32, event.pt.into(), event.time));
        if handled == Some(true) {
            return LRESULT(1);
        }
    }
    // SAFETY: Forward the original hook parameters unchanged. HHOOK is ignored.
    unsafe { CallNextHookEx(None, code, message, data) }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    let message_id = message.0 as u32;
    if code == HC_ACTION as i32
        && data.0 != 0
        && matches!(
            message_id,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
    {
        // SAFETY: For HC_ACTION the OS passes a KBDLLHOOKSTRUCT valid for this call.
        let key = unsafe { (*(data.0 as *const KBDLLHOOKSTRUCT)).vkCode };
        let up = matches!(message_id, WM_KEYUP | WM_SYSKEYUP);
        let action = with_state(|state| state.handle_key(key, up));
        match action {
            Some(KeyAction::MaskMenuRelease) => mask_menu(),
            Some(KeyAction::Consume) => return LRESULT(1),
            _ => (),
        }
    }
    // SAFETY: Forward the real modifier release and other unhandled key events.
    unsafe { CallNextHookEx(None, code, message, data) }
}

#[derive(Clone, Copy, Default)]
struct MenuKeys(u8);

impl MenuKeys {
    const WIN: [(VIRTUAL_KEY, u8); 2] = [(VK_LWIN, 1), (VK_RWIN, 2)];
    const ALT: [(VIRTUAL_KEY, u8); 2] = [(VK_LMENU, 4), (VK_RMENU, 8)];

    fn for_modifier(modifier: Modifier) -> [(VIRTUAL_KEY, u8); 2] {
        match modifier {
            Modifier::Win => Self::WIN,
            Modifier::Alt => Self::ALT,
        }
    }

    fn held(modifier: Modifier) -> Self {
        Self::held_with_event(modifier, None)
    }

    fn held_with_event(modifier: Modifier, event: Option<(u32, bool)>) -> Self {
        let keys = Self::for_modifier(modifier);
        // The keyboard hook runs before the async state for this key is updated.
        let held = |key: VIRTUAL_KEY| match event {
            Some((vk, up)) if vk == u32::from(key.0) => !up,
            _ => key_is_down(key),
        };
        let mut bits = 0;
        for (key, bit) in keys {
            // Right Alt plus Ctrl is AltGr on layouts that use it.
            if held(key) && !(key == VK_RMENU && held(VK_CONTROL)) {
                bits |= bit;
            }
        }
        Self(bits)
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn add(&mut self, keys: Self) {
        self.0 |= keys.0;
    }

    fn release(&mut self, key: u32) -> bool {
        let Some(bit) = Self::bit(key) else {
            return false;
        };
        let used_for_gesture = self.0 & bit != 0;
        self.0 &= !bit;
        used_for_gesture
    }

    fn bit(key: u32) -> Option<u8> {
        Self::WIN
            .into_iter()
            .chain(Self::ALT)
            .find_map(|(vk, bit)| (u32::from(vk.0) == key).then_some(bit))
    }
}

fn key_is_down(key: VIRTUAL_KEY) -> bool {
    // SAFETY: This is a read-only input state query for a valid virtual key.
    unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
}

fn mask_menu() {
    // Mask immediately before Windows handles the real release. A mask sent at
    // mouse-down can be undone by Win key repeat while the drag is in progress.
    // AutoHotkey uses keybd_event here for reentrant delivery from the keyboard hook.
    const MENU_MASK_KEY: u8 = 0xE8;
    // SAFETY: Use an unassigned virtual key and send both transitions. The
    // callback has released its state borrow before the generated events arrive.
    unsafe {
        keybd_event(MENU_MASK_KEY, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(MENU_MASK_KEY, 0, KEYEVENTF_KEYUP, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::worker::{self, WindowRequests};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };
    use windows::core::w;

    #[test]
    fn installs_and_removes_the_native_hooks_and_checks_drag_cursors() {
        let (commands, _requests) = worker::channel();
        commands.close();
        InputHooks::start(commands, Settings::default(), |_| {})
            .unwrap()
            .stop()
            .unwrap();

        // SAFETY: Obtain the current executable's instance handle for our overlay.
        let instance = unsafe { HINSTANCE(GetModuleHandleW(None).unwrap().0) };
        let mut cursor = CursorOverlay::create(instance).unwrap();
        cursor.verify_hit_testing();

        let (mut state, requests) = test_state(Some(cursor));
        let motion = requests.motion();
        let point = Point { x: 1, y: 1 };
        for (mode, shape, release) in [
            (DragMode::Move, CursorShape::Move, WM_LBUTTONUP),
            (
                DragMode::Resize,
                CursorShape::Resize(crate::geometry::ResizeDirection::Horizontal),
                WM_RBUTTONUP,
            ),
        ] {
            assert!(motion.begin(point));
            state.captured = Some(MouseCapture::Drag(mode, shape));
            state.refresh_cursor(point, None);
            assert!(state.cursor.as_ref().unwrap().overlay.owns_point(point));
            assert!(state.handle_mouse(release, point, 0));
            assert!(!state.cursor.as_ref().unwrap().overlay.owns_point(point));
            motion.finish_drag();
        }
    }

    #[test]
    fn masks_only_a_win_key_used_by_a_gesture_and_only_once() {
        let mut keys = MenuKeys::default();
        assert!(!keys.release(u32::from(VK_LWIN.0)));
        keys.add(MenuKeys(MenuKeys::WIN[1].1));
        assert!(!keys.release(u32::from(VK_LWIN.0)));
        assert!(!keys.release(u32::from(b'E')));
        assert!(keys.release(u32::from(VK_RWIN.0)));
        assert!(!keys.release(u32::from(VK_RWIN.0)));
    }

    #[test]
    fn double_click_matches_the_same_window_within_time_and_distance() {
        let make_window = || {
            // SAFETY: Create a hidden test window from a shared system class.
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    None,
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap()
        };
        let first = make_window();
        let second = make_window();
        let start = MovePress {
            target: WindowTarget::identify(first).unwrap(),
            origin: Point { x: -100, y: 50 },
            time: u32::MAX - 80,
            drag_radius: (2, 2),
            can_click: true,
        };
        let next = MovePress {
            time: 20,
            origin: Point { x: -98, y: 51 },
            ..start
        };
        assert!(!start.moved(next.origin));
        assert!(start.moved(Point { x: -97, y: 51 }));
        assert!(start.matches(next, (2, 2), 500));
        assert!(!start.matches(next, (2, 2), 100));
        assert!(!start.matches(next, (1, 2), 500));
        let other = MovePress {
            target: WindowTarget::identify(second).unwrap(),
            ..next
        };
        assert!(!start.matches(other, (2, 2), 500));
        // SAFETY: The test owns these two windows on this thread.
        unsafe {
            DestroyWindow(second).unwrap();
            DestroyWindow(first).unwrap();
        }
    }

    fn test_state(cursor: Option<CursorOverlay>) -> (HookState, WindowRequests) {
        let (commands, requests) = worker::channel();
        let state = HookState::new(
            commands,
            &Settings::default(),
            Arc::new(Mutex::new(None)),
            Box::new(|_| {}),
            cursor,
        );
        (state, requests)
    }

    #[test]
    fn cancelling_consumes_only_the_captured_button_and_escape_pair() {
        let (mut state, requests) = test_state(None);
        state.captured = Some(MouseCapture::Drag(DragMode::Resize, CursorShape::Move));
        let motion = requests.motion();
        let origin = Point { x: 10, y: 20 };
        assert!(motion.begin(origin));
        let escape = u32::from(VK_ESCAPE.0);

        let mut next_settings = Settings::default();
        next_settings.modifier = Modifier::Alt;
        next_settings.snapping = false;
        next_settings.tiling = false;
        *state.pending_settings.lock().unwrap() = Some(InputSettings::new(&next_settings));
        state.apply_pending_settings();
        assert_eq!(state.settings.modifier, Modifier::Win);
        assert!(state.settings.tiling);

        assert!(state.on_key(escape, false) == KeyAction::Consume);
        assert!(!state.on_mouse(WM_LBUTTONUP, origin, 0));
        assert!(state.on_mouse(WM_RBUTTONUP, origin, 0));
        assert!(state.on_key(escape, true) == KeyAction::Consume);
        assert!(state.on_key(escape, false) == KeyAction::Forward);

        state.captured = Some(MouseCapture::Drag(DragMode::Move, CursorShape::Move));
        assert!(state.on_key(escape, false) == KeyAction::Forward);
        assert!(state.on_key(escape, true) == KeyAction::Forward);
        state.set_paused(true);
        assert!(matches!(
            state.captured,
            Some(MouseCapture::ConsumeRelease(MouseButton::Left))
        ));
        assert!(state.on_mouse(WM_LBUTTONUP, origin, 0));
        assert!(!state.on_mouse(WM_LBUTTONDOWN, origin, 0));
        motion.finish_drag();
        state.apply_pending_settings();
        assert_eq!(state.settings.modifier, Modifier::Alt);
        assert!(!state.settings.snapping);
        assert!(!state.settings.tiling);
    }
}
