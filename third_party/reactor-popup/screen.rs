use super::*;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use windows_core::{Error, Result as WinResult};

windows_core::link!("user32.dll" "system" fn GetForegroundWindow() -> HWND);
windows_core::link!("user32.dll" "system" fn GetFocus() -> HWND);
windows_core::link!("user32.dll" "system" fn IsChild(parent: HWND, child: HWND) -> windows_core::BOOL);
windows_core::link!("user32.dll" "system" fn SetFocus(window: HWND) -> HWND);

const FOCUS_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const FOCUS_RETRY_LIMIT: Duration = Duration::from_millis(250);

/// The controls to create once for a screen-positioned menu.
#[derive(Clone, Copy)]
pub enum ScreenMenuItem {
    Command(&'static str),
    Toggle(&'static str),
    Separator,
}

/// Values to refresh before showing an existing menu. Indices include separators.
pub enum ScreenMenuChange {
    Text(usize, &'static str),
    Checked(usize, bool),
}

/// One Reactor UI thread and XAML Island shared by a set of reusable flyouts.
pub struct ScreenMenus {
    dispatcher: DispatcherQueue,
}

type Completion = Box<dyn FnOnce(WinResult<Option<usize>>) + Send>;

thread_local! {
    static HOST: RefCell<Option<ScreenMenuHost>> = const { RefCell::new(None) };
}

impl ScreenMenus {
    /// Prepares the flyouts and waits for the anchor to load before taking requests.
    pub fn start(layouts: Vec<Vec<ScreenMenuItem>>) -> WinResult<Self> {
        // Runtime installation messages run before the anchor's startup timeout.
        crate::native::app::bootstrap_runtime()?;
        let (ready, started) = sync_channel(1);
        thread::Builder::new()
            .name("reactor-popups".into())
            .spawn(move || {
                let startup = ready.clone();
                let result = crate::native::app::run(move |dispatcher| {
                    let menus = layouts
                        .into_iter()
                        .map(|items| PreparedMenu::new(dispatcher, items))
                        .collect::<WinResult<_>>()?;
                    let ready_dispatcher = dispatcher.clone();
                    let anchor = TransientMenuHost::with_ready(dispatcher.clone(), move || {
                        if startup.send(Ok(ready_dispatcher)).is_err() {
                            crate::native::app::exit_ui_thread();
                        }
                    })?;
                    let input_window = anchor
                        .state
                        .borrow()
                        ._source
                        .SiteBridge()?
                        .cast::<IDesktopSiteBridge>()?
                        .WindowId()?
                        .value as HWND;
                    HOST.with(|host| {
                        host.replace(Some(ScreenMenuHost {
                            menus,
                            anchor,
                            input_window,
                        }))
                    });
                    Ok(HostLifetime)
                });
                if let Err(error) = result {
                    let _ = ready.try_send(Err(error));
                }
            })
            .map_err(|error| Error::new(E_FAIL, error.to_string()))?;

        let dispatcher =
            started
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => failure("The WinUI menu anchor did not load."),
                    RecvTimeoutError::Disconnected => failure("The WinUI menu host stopped."),
                })??;
        Ok(Self { dispatcher })
    }

    /// Shows a prepared menu at a physical screen point. The callback runs after dismissal.
    pub fn show(
        &self,
        menu: usize,
        position: ScreenPoint,
        changes: Vec<ScreenMenuChange>,
        complete: impl FnOnce(WinResult<Option<usize>>) + Send + 'static,
    ) -> WinResult<()> {
        // Capture the window that owned the foreground when the gesture was handled.
        let previous_foreground = unsafe { GetForegroundWindow() } as usize;
        let request = Mutex::new(Some((changes, complete)));
        let handler = DispatcherQueueHandler::new(move || {
            let (changes, complete) = request.lock().unwrap().take().unwrap();
            let complete = Box::new(complete);
            HOST.with(|host| {
                if let Some(host) = host.borrow().as_ref() {
                    host.show(
                        menu,
                        position,
                        previous_foreground as HWND,
                        changes,
                        complete,
                    );
                } else {
                    complete(Err(failure("The WinUI menu host stopped.")));
                }
            });
        });
        if self
            .dispatcher
            .TryEnqueueWithPriority(DispatcherQueuePriority::Normal, &handler)?
        {
            Ok(())
        } else {
            Err(failure("The WinUI menu host stopped."))
        }
    }
}

impl Drop for ScreenMenus {
    fn drop(&mut self) {
        let handler = DispatcherQueueHandler::new(crate::native::app::exit_ui_thread);
        let _ = self
            .dispatcher
            .TryEnqueueWithPriority(DispatcherQueuePriority::High, &handler);
    }
}

struct HostLifetime;

impl Drop for HostLifetime {
    fn drop(&mut self) {
        HOST.with(|host| drop(host.take()));
    }
}

struct ScreenMenuHost {
    menus: Vec<PreparedMenu>,
    anchor: TransientMenuHost,
    input_window: HWND,
}

impl ScreenMenuHost {
    fn show(
        &self,
        index: usize,
        position: ScreenPoint,
        previous_foreground: HWND,
        changes: Vec<ScreenMenuChange>,
        complete: Completion,
    ) {
        if self
            .menus
            .iter()
            .any(|menu| menu.pending.borrow().is_some())
        {
            complete(Err(failure("A WinUI menu is already open.")));
        } else if let Some(menu) = self.menus.get(index) {
            menu.show(
                &self.anchor,
                self.input_window,
                position,
                previous_foreground,
                changes,
                complete,
            );
        } else {
            complete(Err(failure("The requested WinUI menu is unavailable.")));
        }
    }
}

struct PreparedMenu {
    flyout: IFlyoutBase,
    items: Vec<Option<PreparedItem>>,
    pending: Rc<RefCell<Option<PendingMenu>>>,
    focus_timer: DispatcherQueueTimer,
    _handlers: Vec<windows_core::EventRevoker>,
}

struct PreparedItem {
    item: MenuFlyoutItem,
    toggle: Option<IToggleMenuFlyoutItem>,
    label: Cell<&'static str>,
}

#[derive(Clone, Copy)]
struct MenuFocus {
    hwnd: HWND,
    input_window: HWND,
    previous_foreground: HWND,
    retry_until: Instant,
}

enum MenuActivation {
    Ready,
    Waiting,
    Cancelled,
}

impl MenuFocus {
    fn try_activate(self) -> MenuActivation {
        // SAFETY: The host and its island belong to this dispatcher. Never take
        // foreground from an unrelated window the user switched to after invoking the menu.
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground == self.previous_foreground && foreground != self.hwnd {
                let _ = SetForegroundWindow(self.hwnd);
            }
            let foreground = GetForegroundWindow();
            if foreground == self.hwnd {
                // The site can redirect keyboard focus to one of its own child HWNDs.
                let has_focus = |focused| {
                    focused == self.input_window || IsChild(self.input_window, focused).as_bool()
                };
                if !has_focus(GetFocus()) {
                    let _ = SetFocus(self.input_window);
                }
                if has_focus(GetFocus()) {
                    return MenuActivation::Ready;
                }
            } else if !foreground.is_null() && foreground != self.previous_foreground {
                return MenuActivation::Cancelled;
            }
        }
        if Instant::now() < self.retry_until {
            MenuActivation::Waiting
        } else {
            MenuActivation::Cancelled
        }
    }
}

struct PendingMenu {
    focus: MenuFocus,
    anchor: Option<FrameworkElement>,
    selected: Option<usize>,
    complete: Option<Completion>,
}

/// Returns true while the flyout is waiting for native focus.
fn open_when_focused(flyout: &IFlyoutBase, pending: &RefCell<Option<PendingMenu>>) -> bool {
    let focus = pending
        .borrow()
        .as_ref()
        .filter(|menu| menu.anchor.is_some())
        .map(|menu| menu.focus);
    let Some(focus) = focus else {
        return false;
    };
    match focus.try_activate() {
        MenuActivation::Waiting => true,
        MenuActivation::Cancelled => {
            if let Some(menu) = pending.take() {
                menu.finish(Ok(None));
            }
            false
        }
        MenuActivation::Ready => {
            let anchor = pending
                .borrow_mut()
                .as_mut()
                .and_then(|menu| menu.anchor.take());
            if let Some(anchor) = anchor
                && let Err(error) = flyout.ShowAt(&anchor)
                && let Some(menu) = pending.take()
            {
                menu.finish(Err(error));
            }
            false
        }
    }
}

impl PendingMenu {
    fn finish(mut self, result: WinResult<Option<usize>>) {
        // Leave an app the user clicked on in the foreground.
        unsafe {
            if !self.focus.previous_foreground.is_null() && GetForegroundWindow() == self.focus.hwnd
            {
                let _ = SetForegroundWindow(self.focus.previous_foreground);
            }
        }
        self.complete.take().unwrap()(result);
    }
}

impl Drop for PendingMenu {
    fn drop(&mut self) {
        if let Some(complete) = self.complete.take() {
            complete(Err(failure("The WinUI menu closed unexpectedly.")));
        }
    }
}

impl PreparedMenu {
    fn new(dispatcher: &DispatcherQueue, layout: Vec<ScreenMenuItem>) -> WinResult<Self> {
        let flyout = MenuFlyout::new()?;
        let output = flyout.Items()?;
        let flyout = flyout.cast::<IFlyoutBase>()?;
        flyout.SetPlacement(FlyoutPlacementMode::BottomEdgeAlignedLeft)?;
        flyout.SetShouldConstrainToRootBounds(false)?;
        let pending = Rc::<RefCell<Option<PendingMenu>>>::default();
        let mut items = Vec::with_capacity(layout.len());
        let mut handlers = Vec::with_capacity(layout.len() + 2);
        for (index, entry) in layout.into_iter().enumerate() {
            let (control, label, is_toggle) = match entry {
                ScreenMenuItem::Command(label) => ("MenuFlyoutItem", label, false),
                ScreenMenuItem::Toggle(label) => ("ToggleMenuFlyoutItem", label, true),
                ScreenMenuItem::Separator => {
                    output.Append(&MenuFlyoutSeparator::new()?)?;
                    items.push(None);
                    continue;
                }
            };
            // Native triggers bypass the island's input history. Use the WinUI
            // mouse/keyboard padding even before the island sees its first input.
            let element = XamlReader::Load(&format!(
                r#"<{control} xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
                    Padding="{{ThemeResource MenuFlyoutItemThemePaddingNarrow}}"/>"#,
            ))?;
            let item = element.cast::<MenuFlyoutItem>()?;
            let toggle = if is_toggle {
                Some(element.cast()?)
            } else {
                None
            };
            item.SetText(label)?;
            let active = Rc::downgrade(&pending);
            handlers.push(item.Click(local::handler(move |_, _| {
                if let Some(active) = active.upgrade()
                    && let Some(menu) = active.borrow_mut().as_mut()
                {
                    menu.selected = Some(index);
                }
            }))?);
            output.Append(&item)?;
            items.push(Some(PreparedItem {
                item,
                toggle,
                label: Cell::new(label),
            }));
        }

        let focus_timer = dispatcher.CreateTimer()?;
        focus_timer.SetInterval(TimeSpan::try_from(FOCUS_RETRY_INTERVAL).unwrap())?;
        focus_timer.SetIsRepeating(true)?;
        let active = Rc::downgrade(&pending);
        let opening_flyout = flyout.clone();
        handlers.push(focus_timer.Tick(
            local::handler::<DispatcherQueueTimer, IInspectable, _>(move |sender, _| {
                let waiting = active
                    .upgrade()
                    .is_some_and(|active| open_when_focused(&opening_flyout, &active));
                if !waiting && let Some(timer) = sender.as_ref() {
                    let _ = timer.Stop();
                }
            }),
        )?);
        let closing_timer = focus_timer.clone();
        let active = Rc::downgrade(&pending);
        let dispatcher = dispatcher.clone();
        handlers.push(flyout.Closed(local::handler(move |_, _| {
            let _ = closing_timer.Stop();
            let active = active.clone();
            // Let Click record the command even if Closed fires first in this input turn.
            let finish = DispatcherQueueHandler::new(local::task(move || {
                if let Some(active) = active.upgrade()
                    && let Some(menu) = active.take()
                {
                    let selected = menu.selected;
                    menu.finish(Ok(selected));
                }
            }));
            let _ = dispatcher.TryEnqueueWithPriority(DispatcherQueuePriority::Normal, &finish);
        }))?);
        Ok(Self {
            flyout,
            items,
            pending,
            focus_timer,
            _handlers: handlers,
        })
    }

    fn show(
        &self,
        host: &TransientMenuHost,
        input_window: HWND,
        position: ScreenPoint,
        previous_foreground: HWND,
        changes: Vec<ScreenMenuChange>,
        complete: Completion,
    ) {
        let (theme, hwnd) = {
            let state = host.state.borrow();
            let hwnd = state.host.hwnd().cast();
            let positioned = position_host(&state.host, position);
            if let Err(error) = positioned {
                complete(Err(error));
                return;
            }
            (Rc::clone(&state.theme), hwnd)
        };
        let setup = (|| {
            for change in changes {
                self.update(change)?;
            }
            self.flyout
                .SetXamlRoot(&theme.anchor.cast::<IUIElement>()?.XamlRoot()?)
        })();
        if let Err(error) = setup {
            complete(Err(error));
            return;
        }

        let focus = MenuFocus {
            hwnd,
            input_window,
            previous_foreground,
            retry_until: Instant::now() + FOCUS_RETRY_LIMIT,
        };
        self.pending.replace(Some(PendingMenu {
            focus,
            anchor: Some(theme.anchor.clone()),
            selected: None,
            complete: Some(complete),
        }));
        // Opening before the island has keyboard focus leaves WinUI's light-dismiss
        // machinery observing the foreground handoff as a focus loss.
        if open_when_focused(&self.flyout, &self.pending) {
            if let Err(error) = self.focus_timer.Start()
                && let Some(menu) = self.pending.take()
            {
                menu.finish(Err(error));
            }
        }
    }

    fn update(&self, change: ScreenMenuChange) -> WinResult<()> {
        let item = |index| {
            self.items
                .get(index)
                .and_then(Option::as_ref)
                .ok_or_else(|| failure("Invalid WinUI menu item index."))
        };
        match change {
            ScreenMenuChange::Text(index, label) => {
                let item = item(index)?;
                if item.label.get() != label {
                    item.item.SetText(label)?;
                    item.label.set(label);
                }
                Ok(())
            }
            ScreenMenuChange::Checked(index, checked) => item(index)?
                .toggle
                .as_ref()
                .ok_or_else(|| failure("The WinUI menu item is not a toggle."))?
                .set_checked(checked),
        }
    }
}

fn failure(message: &str) -> Error {
    Error::new(E_FAIL, message)
}

// Microsoft.UI.Xaml.winmd, Microsoft.UI.Xaml.Controls.IToggleMenuFlyoutItem.
windows_core::imp::define_winrt_interface!(
    IToggleMenuFlyoutItem,
    IToggleMenuFlyoutItemVtable,
    0x1803f260_67e4_5bc1_a63a_123510167bb8
);
#[repr(C)]
pub struct IToggleMenuFlyoutItemVtable {
    base: windows_core::IInspectable_Vtbl,
    get_is_checked: unsafe extern "system" fn(*mut core::ffi::c_void, *mut bool) -> HRESULT,
    set_is_checked: unsafe extern "system" fn(*mut core::ffi::c_void, bool) -> HRESULT,
}

impl IToggleMenuFlyoutItem {
    fn set_checked(&self, checked: bool) -> WinResult<()> {
        // SAFETY: The cast verified the interface and the object is on its owning UI thread.
        unsafe { (self.vtable().set_is_checked)(self.as_raw(), checked).ok() }
    }
}
