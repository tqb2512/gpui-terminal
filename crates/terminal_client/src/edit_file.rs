use std::{ffi::OsString, fs::File, io, path::Path};

use crate::FileError;

pub(crate) struct WatchFile {
    parent: File,
    name: OsString,
}

impl WatchFile {
    pub(crate) fn new(path: &Path) -> Result<Self, FileError> {
        let parent = open_directory(path.parent().ok_or(FileError::NotRegular)?)?;
        let name = path.file_name().ok_or(FileError::NotRegular)?.to_owned();
        Ok(Self { parent, name })
    }

    pub(crate) fn open(&self) -> Result<File, FileError> {
        let file = self.open_relative()?;
        let metadata = file.metadata().map_err(|_| FileError::LocalRead)?;
        if !metadata.is_file() || is_reparse(&metadata) {
            return Err(FileError::NotRegular);
        }
        Ok(file)
    }

    #[cfg(unix)]
    fn open_relative(&self) -> Result<File, FileError> {
        use std::{
            ffi::CString,
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::ffi::OsStrExt,
            },
        };
        let name = CString::new(self.name.as_bytes()).map_err(|_| FileError::NotRegular)?;
        // The fd stays valid through openat; O_NOFOLLOW applies to the only path component.
        let fd = unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(open_error(io::Error::last_os_error()));
        }
        // A successful openat transfers ownership of the new descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(windows)]
    fn open_relative(&self) -> Result<File, FileError> {
        use std::os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        };
        use windows::{
            Wdk::{
                Foundation::OBJECT_ATTRIBUTES,
                Storage::FileSystem::{
                    FILE_NON_DIRECTORY_FILE, FILE_SYNCHRONOUS_IO_NONALERT, NtOpenFile,
                },
            },
            Win32::{
                Foundation::{
                    HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
                    UNICODE_STRING,
                },
                Storage::FileSystem::{
                    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
                    SYNCHRONIZE,
                },
                System::IO::IO_STATUS_BLOCK,
            },
            core::PWSTR,
        };
        let mut wide = self.name.encode_wide().collect::<Vec<_>>();
        let length = u16::try_from(wide.len() * 2).map_err(|_| FileError::NotRegular)?;
        let name = UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: PWSTR(wide.as_mut_ptr()),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(self.parent.as_raw_handle()),
            ObjectName: &name,
            Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
            ..Default::default()
        };
        let mut handle = HANDLE::default();
        let mut io = IO_STATUS_BLOCK::default();
        // The pinned parent and the single-component UTF-16 name outlive this synchronous call.
        // Denying write sharing prevents a read while an editor is still writing a save.
        let status = unsafe {
            NtOpenFile(
                &mut handle,
                (FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE).0,
                &attributes,
                &mut io,
                (FILE_SHARE_READ | FILE_SHARE_DELETE).0,
                (FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT).0,
            )
        };
        if status.0 < 0 {
            return Err(open_error(io::Error::from_raw_os_error(unsafe {
                RtlNtStatusToDosError(status)
            }
                as i32)));
        }
        // NtOpenFile returned a new owned handle.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }
}

fn open_directory(path: &Path) -> Result<File, FileError> {
    let mut options = fs_options();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0);
    }
    let parent = options.open(path).map_err(open_error)?;
    let metadata = parent.metadata().map_err(|_| FileError::LocalRead)?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(FileError::NotRegular);
    }
    Ok(parent)
}

fn fs_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    options
}

fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return true;
        }
    }
    metadata.is_symlink()
}

fn open_error(error: io::Error) -> FileError {
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(32 | 33)) {
        return FileError::LocalBusy;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return FileError::NotRegular;
    }
    if error.kind() == io::ErrorKind::NotFound {
        FileError::LocalBusy
    } else {
        FileError::LocalRead
    }
}
