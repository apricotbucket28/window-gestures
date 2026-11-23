use windows::Win32::Foundation::{CloseHandle, E_FAIL};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{Error, PWSTR, Result};

const IMAGE_PATH_CAPACITY: usize = 32_768;

pub(super) struct ProcessImage(Box<[u16]>);

impl Default for ProcessImage {
    fn default() -> Self {
        Self(vec![0; IMAGE_PATH_CAPACITY].into_boxed_slice())
    }
}

impl ProcessImage {
    pub(super) fn read(&mut self, pid: u32) -> Result<&[u16]> {
        let mut length = self.0.len() as u32;
        // SAFETY: The queried process handle remains valid until the call returns.
        // The buffer capacity is measured in UTF-16 units.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
            let result = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(self.0.as_mut_ptr()),
                &mut length,
            );
            let _ = CloseHandle(process);
            result?;
        }
        self.0
            .get(..length as usize)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| Error::from(E_FAIL))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn reads_the_full_executable_path_for_a_process() {
        let mut image = ProcessImage::default();
        let path = image.read(std::process::id()).unwrap();
        assert_eq!(
            PathBuf::from(OsString::from_wide(path))
                .canonicalize()
                .unwrap(),
            std::env::current_exe().unwrap().canonicalize().unwrap(),
        );
    }
}
