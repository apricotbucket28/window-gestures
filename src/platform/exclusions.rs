use windows::Win32::Foundation::MAX_PATH;
use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::core::w;

use super::process::ProcessImage;
use super::window::GestureAccess;

pub(super) struct Exclusions {
    names: Vec<Vec<u16>>,
    image: ProcessImage,
    windows_directory: Option<Vec<u16>>,
}

impl Exclusions {
    pub(super) fn new(names: &[String]) -> Self {
        Self {
            names: names
                .iter()
                .map(|name| name.encode_utf16().collect())
                .collect(),
            image: ProcessImage::default(),
            windows_directory: windows_directory(),
        }
    }

    pub(super) fn excludes(&mut self, pid: u32, access: GestureAccess) -> bool {
        // Frameless moves and explicit exclusions require a known executable.
        let require_image = access == GestureAccess::MoveOnly || !self.names.is_empty();
        self.image.read(pid).map_or(require_image, |path| {
            is_excluded(path, access, &self.names, self.windows_directory.as_deref())
        })
    }
}

fn windows_directory() -> Option<Vec<u16>> {
    let mut path = [0; MAX_PATH as usize];
    // SAFETY: The buffer is writable for the duration of the call.
    let length = unsafe { GetSystemWindowsDirectoryW(Some(&mut path)) } as usize;
    if length == 0 || length >= path.len() {
        return None;
    }
    let mut directory = path[..length].to_vec();
    if !directory.ends_with(&[u16::from(b'\\')]) {
        directory.push(u16::from(b'\\'));
    }
    Some(directory)
}

fn is_excluded(
    path: &[u16],
    access: GestureAccess,
    names: &[Vec<u16>],
    windows_directory: Option<&[u16]>,
) -> bool {
    let system_image = windows_directory.is_some_and(|directory| {
        path.get(..directory.len())
            .is_some_and(|prefix| equals_ignore_case(prefix, directory))
    });
    // Limit frameless moves to non-system applications.
    if access == GestureAccess::MoveOnly && (system_image || windows_directory.is_none()) {
        return true;
    }

    let basename = path
        .rsplit(|&ch| ch == u16::from(b'\\'))
        .next()
        .unwrap_or_default();
    // These dedicated hosts may also own windows with a frame style.
    // Windhawk documents which hosts own Quick Settings and Notification Center.
    // https://github.com/ramensoftware/windhawk-mods/blob/main/mods/block-windows-startmenu-and-hosts.wh.cpp
    let shell_host = system_image
        && [w!("ShellHost.exe"), w!("ShellExperienceHost.exe")]
            .iter()
            // SAFETY: The literals are static and NUL-terminated.
            .any(|host| equals_ignore_case(basename, unsafe { host.as_wide() }));
    shell_host
        || names
            .iter()
            .any(|excluded| equals_ignore_case(basename, excluded))
}

fn equals_ignore_case(left: &[u16], right: &[u16]) -> bool {
    // SAFETY: Both inputs are valid slices. CompareStringOrdinal uses their lengths.
    (unsafe { CompareStringOrdinal(left, right, true) }) == CSTR_EQUAL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_the_whole_executable_basename_case_insensitively() {
        let names = ["Äpp.exe".encode_utf16().collect()];
        let excluded = |path: &str| {
            is_excluded(
                &path.encode_utf16().collect::<Vec<_>>(),
                GestureAccess::Standard,
                &names,
                None,
            )
        };
        assert!(excluded(r"C:\Tools\äpp.EXE"));
        assert!(!excluded(r"C:\äpp.exe\other.exe"));
        assert!(!excluded(r"C:\Tools\MyÄpp.exe"));
    }

    #[test]
    fn excludes_shell_hosts_and_limits_frameless_moves_to_applications_outside_windows() {
        let windows = r"D:\Windows\".encode_utf16().collect::<Vec<_>>();
        let excluded = |path: &str, access| {
            is_excluded(
                &path.encode_utf16().collect::<Vec<_>>(),
                access,
                &[],
                Some(&windows),
            )
        };
        for host in [
            r"D:\WINDOWS\System32\ShellHost.exe",
            r"D:\Windows\SystemApps\ShellExperienceHost_cw5n1h2txyewy\ShellExperienceHost.exe",
        ] {
            assert!(excluded(host, GestureAccess::Standard));
            assert!(excluded(host, GestureAccess::MoveOnly));
        }
        assert!(excluded(
            r"D:\Windows\explorer.exe",
            GestureAccess::MoveOnly
        ));
        assert!(!excluded(
            r"D:\Windows\explorer.exe",
            GestureAccess::Standard
        ));
        assert!(!excluded(
            r"D:\WindowsTools\ShellHost.exe",
            GestureAccess::MoveOnly
        ));
        assert!(!excluded(
            r"C:\Program Files\WindowsApps\Raycast\Raycast.exe",
            GestureAccess::MoveOnly
        ));
    }
}
