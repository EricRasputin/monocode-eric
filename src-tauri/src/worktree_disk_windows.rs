//! Windows volume identities use volume GUIDs (including directory mount points),
//! never drive-letter prefixes. File identities deduplicate NTFS hard links.
use super::{existing_ancestor, now, path_to_js, VolumeUsage};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::path::Path;
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetFileInformationByHandle, GetVolumeNameForVolumeMountPointW,
        GetVolumePathNameW, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT,
    },
    System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
};
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn identity(path: &Path) -> Result<(Vec<u16>, String), String> {
    let path = wide(path);
    let mut mount = vec![0u16; 32768];
    let mut guid = vec![0u16; 32768];
    // SAFETY: NUL-terminated input and output buffers of the supplied sizes.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) == 0
            || GetVolumeNameForVolumeMountPointW(
                mount.as_ptr(),
                guid.as_mut_ptr(),
                guid.len() as u32,
            ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let end = guid.iter().position(|v| *v == 0).unwrap_or(guid.len());
    Ok((mount, String::from_utf16_lossy(&guid[..end]).to_lowercase()))
}
pub(super) fn volume_id(path: &Path, _metadata: &std::fs::Metadata) -> Result<String, String> {
    identity(path).map(|(_, id)| id)
}
pub(super) fn volume(path: &Path) -> Result<VolumeUsage, String> {
    let ancestor = existing_ancestor(path)?;
    let (mount, id) = identity(&ancestor)?;
    let mut available = 0;
    if unsafe {
        GetDiskFreeSpaceExW(
            mount.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(VolumeUsage {
        id,
        path: path_to_js(&ancestor),
        available_bytes: available,
        measured_at: now(),
    })
}
pub(super) fn file_accounting(path: &Path, metadata: &std::fs::Metadata) -> (String, u64, bool) {
    let info = (|| {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return None;
        }
        Some(unsafe { info.assume_init() })
    })();
    match info {
        Some(info) => (
            format!(
                "{}:{}:{}",
                info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
            ),
            metadata.len(),
            metadata.is_file() && info.nNumberOfLinks > 1,
        ),
        // Unknown sharing is excluded from reclaimable estimates.
        None => (path_to_js(path), metadata.len(), true),
    }
}
pub(super) fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return std::io::Error::last_os_error().raw_os_error() != Some(87);
        }
        let mut code = 0;
        let known = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        !known || code == 259
    }
}
