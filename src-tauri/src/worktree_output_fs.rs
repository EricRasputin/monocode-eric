//! Narrow no-follow filesystem boundary for output cleanup. Deletion is relative
//! to open directories on Unix; Windows holds ancestors without delete sharing.
use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions};
use std::path::{Path, PathBuf};

pub(super) struct Directory {
    file: File,
    path: PathBuf,
    // Keep ancestors open too (Windows denies rename/delete while these live).
    _ancestors: Vec<Directory>,
}

#[cfg(unix)]
pub(super) fn identity(metadata: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}

fn file_identity(file: &File) -> Result<String, String> {
    #[cfg(unix)]
    {
        file.metadata()
            .map(|m| identity(&m))
            .map_err(|e| e.to_string())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        // SAFETY: live file handle and valid writable output storage.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let info = unsafe { info.assume_init() };
        Ok(format!(
            "{}:{}:{}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        ))
    }
}

pub(super) fn path_identity(_path: &Path, metadata: &Metadata) -> Result<String, String> {
    #[cfg(unix)]
    {
        Ok(identity(metadata))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        if is_link(metadata) {
            return Err("Git metadata was replaced by a reparse point".into());
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(_path)
            .map_err(|e| e.to_string())?;
        file_identity(&file)
    }
}

impl Directory {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let mut current = PathBuf::new();
        let mut ancestors = Vec::new();
        for component in path.components() {
            current.push(component);
            if !current.is_absolute() {
                continue;
            }
            let directory = Self::open_one(&current)?;
            ancestors.push(directory);
        }
        let mut result = ancestors.pop().ok_or("Missing directory")?;
        result._ancestors = ancestors;
        result.check()?;
        Ok(result)
    }

    fn open_one(path: &Path) -> Result<Self, String> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::*;
            options
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let file = options.open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err("Output path is a symlink or is not a directory".into());
        }
        Ok(Self {
            file,
            path: path.into(),
            _ancestors: Vec::new(),
        })
    }

    pub(super) fn identity(&self) -> Result<String, String> {
        file_identity(&self.file)
    }

    pub(super) fn same_filesystem(&self, other: &Self) -> Result<bool, String> {
        Ok(self.identity()?.split(':').next() == other.identity()?.split(':').next())
    }

    pub(super) fn check(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let current_identity = identity(&metadata);
        #[cfg(windows)]
        let current_identity = Self::open_one(&self.path)?.identity()?;
        if is_link(&metadata)
            || current_identity != self.identity()?
            || super::super::path_to_js(
                &std::fs::canonicalize(&self.path).map_err(|e| e.to_string())?,
            ) != super::super::path_to_js(&self.path)
        {
            return Err("Output path was replaced or moved; review again".into());
        }
        Ok(())
    }

    pub(super) fn child(&self, name: &std::ffi::OsStr) -> Result<Self, String> {
        self.check()?;
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;
            let encoded = std::ffi::CString::new(name.as_bytes()).map_err(|e| e.to_string())?;
            // SAFETY: live directory descriptor, literal NUL-terminated child name.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    encoded.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let child = Self {
                file: unsafe { File::from_raw_fd(fd) },
                path: self.path.join(name),
                _ancestors: Vec::new(),
            };
            use std::os::unix::fs::MetadataExt;
            if child.file.metadata().map_err(|e| e.to_string())?.dev()
                != self.file.metadata().map_err(|e| e.to_string())?.dev()
            {
                return Err("Output crosses a filesystem mount; kept".into());
            }
            child.check()?;
            Ok(child)
        }
        #[cfg(windows)]
        {
            let child = Self::open_one(&self.path.join(name))?;
            if !self.same_filesystem(&child)? {
                return Err("Output crosses a filesystem mount; kept".into());
            }
            Ok(child)
        }
    }

    pub(super) fn names(&self) -> Result<Vec<OsString>, String> {
        self.check()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            // fdopendir owns the duplicated descriptor. closedir releases it.
            let fd = unsafe { libc::dup(self.file.as_raw_fd()) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                unsafe {
                    libc::close(fd);
                }
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut names = Vec::new();
            unsafe {
                libc::rewinddir(stream);
            }
            let error = loop {
                #[cfg(target_os = "macos")]
                unsafe {
                    *libc::__error() = 0;
                }
                #[cfg(target_os = "linux")]
                unsafe {
                    *libc::__errno_location() = 0;
                }
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    break std::io::Error::last_os_error();
                }
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name != b"." && name != b".." {
                    names.push(std::ffi::OsStr::from_bytes(name).to_os_string());
                }
            };
            unsafe {
                libc::closedir(stream);
            }
            if error.raw_os_error().is_some_and(|code| code != 0) {
                return Err(error.to_string());
            }
            names.sort();
            Ok(names)
        }
        #[cfg(windows)]
        {
            let mut names = std::fs::read_dir(&self.path)
                .map_err(|e| e.to_string())?
                .map(|e| e.map(|e| e.file_name()).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            names.sort();
            Ok(names)
        }
    }

    pub(super) fn metadata(&self, name: &std::ffi::OsStr) -> Result<Metadata, String> {
        self.check()?;
        // Used only to classify/estimate, never to follow a child for deletion.
        std::fs::symlink_metadata(self.path.join(name)).map_err(|e| e.to_string())
    }

    pub(super) fn entry_identity(&self, name: &std::ffi::OsStr) -> Result<String, String> {
        #[cfg(unix)]
        {
            self.metadata(name).map(|m| identity(&m))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::*;
            self.check()?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(self.path.join(name))
                .map_err(|e| e.to_string())?;
            file_identity(&file)
        }
    }

    pub(super) fn remove(&self, name: &std::ffi::OsStr, directory: bool) -> Result<(), String> {
        self.check()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(name.as_bytes()).map_err(|e| e.to_string())?;
            // Never recursive and never follows a link. A replaced nonempty
            // directory fails rather than deleting its contents.
            if unsafe {
                libc::unlinkat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            if directory {
                std::fs::remove_dir(self.path.join(name))
            } else {
                std::fs::remove_file(self.path.join(name))
            }
            .map_err(|e| e.to_string())
        }
    }
}

pub(super) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn estimated_bytes(metadata: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 && metadata.is_file() {
            return 0;
        }
        metadata.blocks().saturating_mul(512)
    }
    #[cfg(windows)]
    {
        metadata.len()
    }
}
