use std::marker::PhantomData;
use std::rc::Rc;

use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Result;

pub(super) struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    pub(super) fn initialize() -> Result<Self> {
        // SAFETY: The guard stays on this thread and balances either success code.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: This guard has a matching successful initialization on this thread.
        unsafe { CoUninitialize() };
    }
}

pub(super) struct TaskMem<T>(*mut T);

impl<T> TaskMem<T> {
    // SAFETY: The caller transfers ownership of a COM task allocator pointer
    // that must be freed with CoTaskMemFree.
    pub(super) unsafe fn from_raw(pointer: *mut T) -> Self {
        Self(pointer)
    }

    pub(super) fn as_ptr(&self) -> *const T {
        self.0
    }
}

impl<T> Drop for TaskMem<T> {
    fn drop(&mut self) {
        // SAFETY: The constructor takes ownership of a COM task allocation.
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}
