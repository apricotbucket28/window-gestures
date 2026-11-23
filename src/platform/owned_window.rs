use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, RegisterClassW, UnregisterClassW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WNDCLASSW,
};
use windows::core::{Error, PCWSTR, Result};

pub(super) struct OwnedWindow {
    pub(super) hwnd: HWND,
    instance: HINSTANCE,
    class: PCWSTR,
}

impl OwnedWindow {
    /// The class name, window procedure and resources must remain valid until the window is dropped.
    pub(super) unsafe fn create(
        class: &WNDCLASSW,
        title: PCWSTR,
        style: WINDOW_STYLE,
        ex_style: WINDOW_EX_STYLE,
    ) -> Result<Self> {
        // SAFETY: The caller supplies a valid class and uses the window on its owning thread.
        unsafe {
            if RegisterClassW(class) == 0 {
                return Err(Error::from_thread());
            }
            let result = CreateWindowExW(
                ex_style,
                class.lpszClassName,
                title,
                style,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(class.hInstance),
                None,
            );
            match result {
                Ok(hwnd) => Ok(Self {
                    hwnd,
                    instance: class.hInstance,
                    class: class.lpszClassName,
                }),
                Err(error) => {
                    let _ = UnregisterClassW(class.lpszClassName, Some(class.hInstance));
                    Err(error)
                }
            }
        }
    }
}

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: The window and its class belong to the creating thread.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(self.class, Some(self.instance));
        }
    }
}
