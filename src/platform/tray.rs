use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETFOCUS, NIM_SETVERSION,
    NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT, RegisterWindowMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_ENDSESSION, WM_QUIT, WNDCLASSW,
};
use windows::core::{Error, PCWSTR, Result, w};
use windows_reactor::{ScreenMenuChange, ScreenMenuItem, ScreenMenus, ScreenPoint};

use super::actions::{WindowAction, is_topmost};
use super::hook::{InputHooks, MenuRequest};
use super::owned_window::OwnedWindow;
use super::ui_window::WindowController;
use super::{APP_TITLE, AppIcon, AppResult, show_error};
use crate::settings::Settings;

const TRAY_EVENT: u32 = WM_APP + 10;
const SHOW_MENU: u32 = WM_APP + 11;
const CLASS_NAME: PCWSTR = w!("WindowGesturesTrayWindow");
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
enum TrayAction {
    TogglePause,
    OpenSettings,
    OpenGuide,
    Exit,
}

impl TrayAction {
    fn opens_window(self) -> bool {
        matches!(self, Self::OpenSettings | Self::OpenGuide)
    }
}

const TRAY_MENU: usize = 0;
const WINDOW_MENU: usize = 1;
const PAUSE_ITEM: usize = 0;
const ALWAYS_ON_TOP_ITEM: usize = 0;
const MAXIMIZE_ITEM: usize = 4;
const MINIMIZE_OTHERS_ITEM: usize = 5;

const TRAY_ITEMS: &[(ScreenMenuItem, Option<TrayAction>)] = &[
    (
        ScreenMenuItem::Command("Pause"),
        Some(TrayAction::TogglePause),
    ),
    (
        ScreenMenuItem::Command("Settings…"),
        Some(TrayAction::OpenSettings),
    ),
    (
        ScreenMenuItem::Command("Quick guide…"),
        Some(TrayAction::OpenGuide),
    ),
    (ScreenMenuItem::Separator, None),
    (ScreenMenuItem::Command("Exit"), Some(TrayAction::Exit)),
];

const WINDOW_ITEMS: &[(ScreenMenuItem, Option<WindowAction>)] = &[
    (
        ScreenMenuItem::Toggle("Always on top"),
        Some(WindowAction::AlwaysOnTop),
    ),
    (
        ScreenMenuItem::Command("Center on monitor"),
        Some(WindowAction::Center),
    ),
    (ScreenMenuItem::Separator, None),
    (
        ScreenMenuItem::Command("Minimize"),
        Some(WindowAction::Minimize),
    ),
    (
        ScreenMenuItem::Toggle("Maximize"),
        Some(WindowAction::MaximizeRestore),
    ),
    (
        ScreenMenuItem::Toggle("Minimize other windows"),
        Some(WindowAction::MinimizeOthers),
    ),
    (ScreenMenuItem::Separator, None),
    (
        ScreenMenuItem::Command("Open file location"),
        Some(WindowAction::OpenFileLocation),
    ),
];

fn selection<T: Copy>(items: &[(ScreenMenuItem, Option<T>)], index: Option<usize>) -> Option<T> {
    items.get(index?).and_then(|(_, action)| *action)
}

enum OpenMenu {
    Tray,
    Window(MenuRequest),
}

pub(super) struct TrayNotifications {
    ready: OwnedHandle,
    request: Mutex<Option<MenuRequest>>,
    action_error: Mutex<Option<String>>,
    menu_result: Mutex<Option<std::result::Result<Option<usize>, String>>>,
}

impl TrayNotifications {
    fn new() -> Result<Self> {
        // SAFETY: Transfer ownership of the new unnamed, auto-reset event.
        let ready =
            unsafe { OwnedHandle::from_raw_handle(CreateEventW(None, false, false, None)?.0) };
        Ok(Self {
            ready,
            request: Mutex::new(None),
            action_error: Mutex::new(None),
            menu_result: Mutex::new(None),
        })
    }

    pub(super) fn post(&self, request: MenuRequest) {
        let Ok(mut slot) = self.request.try_lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        *slot = Some(request);
        if !self.wake() {
            slot.take();
        }
    }

    pub(super) fn report_action_failed(&self, error: String) {
        *self
            .action_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
        self.wake();
    }

    fn wake(&self) -> bool {
        // SAFETY: The owned event stays live while any publisher can signal it.
        unsafe { SetEvent(self.wait_handle()) }.is_ok()
    }

    fn wait_handle(&self) -> HANDLE {
        HANDLE(self.ready.as_raw_handle())
    }

    fn take_window_menu(&self) -> AppResult<Option<MenuRequest>> {
        Ok(self
            .request
            .lock()
            .map_err(|_| "The window menu became unavailable.")?
            .take())
    }

    fn take_action_error(&self) -> AppResult<Option<String>> {
        Ok(self
            .action_error
            .lock()
            .map_err(|_| "Could not read the window action result.")?
            .take())
    }

    fn complete_menu(&self, result: std::result::Result<Option<usize>, String>) {
        *self.menu_result.lock().unwrap() = Some(result);
        self.wake();
    }

    fn take_menu_result(&self) -> Option<std::result::Result<Option<usize>, String>> {
        self.menu_result.lock().unwrap().take()
    }
}

pub(super) struct Tray {
    window: OwnedWindow,
    notifications: Arc<TrayNotifications>,
    menus: ScreenMenus,
}

impl Tray {
    pub(super) fn create() -> AppResult<Self> {
        let notifications = Arc::new(TrayNotifications::new()?);
        let menus = ScreenMenus::start(vec![
            TRAY_ITEMS.iter().map(|(item, _)| *item).collect(),
            WINDOW_ITEMS.iter().map(|(item, _)| *item).collect(),
        ])?;
        // SAFETY: The name is static and the procedure implements the Win32 ABI.
        let window = unsafe {
            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            if taskbar_created == 0 {
                return Err(Error::from_thread().into());
            }
            TASKBAR_CREATED.store(taskbar_created, Ordering::Relaxed);
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            OwnedWindow::create(
                &class,
                APP_TITLE,
                WINDOW_STYLE::default(),
                WINDOW_EX_STYLE::default(),
            )?
        };
        let tray = Self {
            window,
            notifications,
            menus,
        };
        add_icon(tray.window.hwnd)?;
        Ok(tray)
    }

    pub(super) fn notifications(&self) -> Arc<TrayNotifications> {
        Arc::clone(&self.notifications)
    }

    pub(super) fn run(
        &self,
        hooks: &InputHooks,
        settings_path: &Path,
        settings: Settings,
        show_welcome: bool,
    ) -> AppResult<()> {
        let notifications = self.notifications();
        let mut windows = WindowController::new(settings_path, settings, move || {
            notifications.wake();
        });
        let mut paused = false;
        let mut open_menu = None;
        if show_welcome {
            windows.show_welcome();
        }
        let mut message = MSG::default();
        loop {
            let mut handles = [HANDLE::default(); 3];
            handles[0] = self.notifications.wait_handle();
            let mut count = 1;
            for handle in windows.handles() {
                handles[count] = handle;
                count += 1;
            }
            let received = wait_for_message(&mut message, &handles[..count])?;
            if received && message.message == WM_QUIT {
                break;
            }
            windows.poll(|saved| hooks.update_settings(saved));
            if let Some(result) = self.notifications.take_menu_result()
                && let Some(menu) = open_menu.take()
            {
                let index = result.unwrap_or_else(|error| {
                    show_error(&error);
                    None
                });
                match menu {
                    OpenMenu::Tray => {
                        let action = selection(TRAY_ITEMS, index);
                        if !action.is_some_and(TrayAction::opens_window) {
                            self.restore_tray_focus();
                        }
                        match action {
                            Some(TrayAction::TogglePause) => {
                                hooks.set_paused(!paused)?;
                                paused = !paused;
                            }
                            Some(TrayAction::OpenSettings) => windows.open_settings(),
                            Some(TrayAction::OpenGuide) => windows.open_guide(),
                            Some(TrayAction::Exit) => break,
                            None => (),
                        }
                    }
                    OpenMenu::Window(request) => {
                        if let Some(action) = selection(WINDOW_ITEMS, index)
                            && request.target.accepts_window_actions()
                        {
                            request.reservation.select(request.target, action)?;
                        }
                    }
                }
            }

            if received && message.hwnd == self.window.hwnd && message.message == SHOW_MENU {
                if open_menu.is_none() {
                    let anchor = message.wParam.0;
                    let point = ScreenPoint::new(
                        i32::from(anchor as i16),
                        i32::from((anchor >> 16) as i16),
                    );
                    self.show_popup(
                        TRAY_MENU,
                        point,
                        vec![ScreenMenuChange::Text(
                            PAUSE_ITEM,
                            if paused { "Resume" } else { "Pause" },
                        )],
                    )?;
                    open_menu = Some(OpenMenu::Tray);
                }
            } else if received {
                // SAFETY: This message was obtained from this thread's queue.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }

            if let Some(request) = self.notifications.take_window_menu()?
                && !paused
                && open_menu.is_none()
                && request.target.accepts_window_actions()
            {
                let state = request.reservation.menu_state(request.target)?;
                self.show_popup(
                    WINDOW_MENU,
                    ScreenPoint::new(request.point.x, request.point.y),
                    vec![
                        ScreenMenuChange::Checked(ALWAYS_ON_TOP_ITEM, is_topmost(request.target)),
                        ScreenMenuChange::Checked(MAXIMIZE_ITEM, request.target.is_maximized()),
                        ScreenMenuChange::Checked(
                            MINIMIZE_OTHERS_ITEM,
                            state.other_windows_minimized,
                        ),
                    ],
                )?;
                open_menu = Some(OpenMenu::Window(request));
            }
            if let Some(error) = self.notifications.take_action_error()? {
                show_error(&error);
            }
        }
        Ok(())
    }

    fn restore_tray_focus(&self) {
        // SAFETY: Give focus back to the notification area after the tray menu.
        let _ = unsafe { Shell_NotifyIconW(NIM_SETFOCUS, &icon_data(self.window.hwnd)) };
    }

    fn show_popup(
        &self,
        menu: usize,
        point: ScreenPoint,
        changes: Vec<ScreenMenuChange>,
    ) -> AppResult<()> {
        let notifications = self.notifications();
        self.menus.show(menu, point, changes, move |result| {
            notifications.complete_menu(result.map_err(|error| error.to_string()));
        })?;
        Ok(())
    }
}

fn wait_for_message(message: &mut MSG, windows: &[HANDLE]) -> Result<bool> {
    // SAFETY: This thread owns the queue and borrows the window workers' live handles.
    unsafe {
        let ready =
            MsgWaitForMultipleObjectsEx(Some(windows), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        match ready {
            WAIT_FAILED => Err(Error::from_thread()),
            ready if ready.0 < WAIT_OBJECT_0.0 + windows.len() as u32 => Ok(false),
            _ => Ok(PeekMessageW(message, None, 0, 0, PM_REMOVE).as_bool()),
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: This guard owns the top-level window and its registered icon.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(self.window.hwnd)) };
    }
}

fn icon_data(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        ..Default::default()
    }
}

fn add_icon(window: HWND) -> Result<()> {
    let image = AppIcon::load(window, true)?;
    let mut icon = icon_data(window);
    icon.hIcon = image.0;
    icon.uCallbackMessage = TRAY_EVENT;
    icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    // SAFETY: Shell_NotifyIcon copies the image during NIM_ADD. The handle and
    // initialized structure remain valid for both calls.
    unsafe {
        for (slot, &ch) in icon.szTip.iter_mut().zip(APP_TITLE.as_wide()) {
            *slot = ch;
        }
        Shell_NotifyIconW(NIM_ADD, &icon).ok()?;
        icon.Anonymous = NOTIFYICONDATAW_0 {
            uVersion: NOTIFYICON_VERSION_4,
        };
        Shell_NotifyIconW(NIM_SETVERSION, &icon).ok()
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        TRAY_EVENT => {
            let event = lparam.0 as u16 as u32;
            if matches!(event, WM_CONTEXTMENU | NIN_SELECT) || event == NIN_SELECT + 1 {
                // SAFETY: Post to our own window so popup requests run in the main loop.
                let _ = unsafe { PostMessageW(Some(window), SHOW_MENU, wparam, LPARAM(0)) };
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_ENDSESSION if message == WM_CLOSE || wparam.0 != 0 => {
            // SAFETY: The window procedure runs on the UI thread's own queue.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        message if message != 0 && message == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            let _ = add_icon(window);
            LRESULT(0)
        }
        // SAFETY: Unhandled messages are forwarded with their original parameters.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{PM_NOREMOVE, PostThreadMessageW};

    #[test]
    fn receives_messages_and_either_window_exit() {
        let mut received = MSG::default();
        let notifications = TrayNotifications::new().unwrap();
        notifications.report_action_failed("Window action failed".into());
        assert!(!wait_for_message(&mut received, &[notifications.wait_handle()]).unwrap());
        assert_eq!(
            notifications.take_action_error().unwrap().as_deref(),
            Some("Window action failed")
        );
        // SAFETY: This test owns its message queue and the test event.
        unsafe {
            let handles =
                std::array::from_fn::<_, 2, _>(|_| CreateEventW(None, false, false, None).unwrap());
            let _events = handles.map(|handle| OwnedHandle::from_raw_handle(handle.0));
            let _ = PeekMessageW(&mut received, None, 0, 0, PM_NOREMOVE);
            PostThreadMessageW(GetCurrentThreadId(), SHOW_MENU, WPARAM(0), LPARAM(0)).unwrap();
            assert!(wait_for_message(&mut received, &handles).unwrap());
            assert_eq!(received.message, SHOW_MENU);
            for handle in handles {
                SetEvent(handle).unwrap();
                assert!(!wait_for_message(&mut received, &handles).unwrap());
            }
        }
    }
}
