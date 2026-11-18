use std::path::Path;

use windows::{
    Win32::{
        Foundation::{E_ABORT, ERROR_CANCELLED, HWND},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            AttachmentServices, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NODEREFERENCELINKS,
            FileOpenDialog, IAttachmentExecute, IFileDialog, OAIF_EXEC, OPENASINFO,
            SHOpenWithDialog, SIGDN_FILESYSPATH,
        },
    },
    core::{HSTRING, PCWSTR, w},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenWithError {
    #[error("Windows could not mark the local copy as a remote attachment.")]
    Attachment,
    #[error("Windows could not open the app picker.")]
    Picker,
    #[error("Windows could not open the upload file picker.")]
    UploadPicker,
}

pub(crate) fn choose_upload_file(
    parent: HWND,
) -> Result<Option<std::path::PathBuf>, OpenWithError> {
    let _apartment = Apartment::enter(OpenWithError::UploadPicker)?;
    // The dialog is used on the current STA; the shell-owned path is freed after copying it.
    (|| unsafe {
        let dialog: IFileDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetTitle(w!("Upload a file"))?;
        dialog.SetOptions(
            dialog.GetOptions()? | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_NODEREFERENCELINKS,
        )?;
        match dialog.Show(Some(parent)) {
            Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
            result => result?,
        }
        let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        use std::os::windows::ffi::OsStringExt;
        let local = std::ffi::OsString::from_wide(path.as_wide());
        CoTaskMemFree(Some(path.0.cast()));
        Ok::<_, windows::core::Error>(Some(local.into()))
    })()
    .map_err(|_| OpenWithError::UploadPicker)
}

#[derive(Clone, Copy)]
pub(crate) enum OpenWithOutcome {
    Opened,
    Cancelled,
}

struct Apartment;

impl Apartment {
    fn enter(error: OpenWithError) -> Result<Self, OpenWithError> {
        // Match this thread's COM initialization with an uninitialize, including S_FALSE.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }.map_err(|_| error)?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // The guard stays on the thread that initialized COM.
        unsafe { CoUninitialize() }
    }
}

pub(crate) fn mark_remote_attachment(path: &Path) -> Result<(), OpenWithError> {
    let _apartment = Apartment::enter(OpenWithError::Attachment)?;
    let path = HSTRING::from(path.as_os_str());
    // Give Windows a remote source for zone policy without disclosing the host or username.
    (|| unsafe {
        let attachment: IAttachmentExecute =
            CoCreateInstance(&AttachmentServices, None, CLSCTX_INPROC_SERVER)?;
        attachment.SetLocalPath(&path)?;
        attachment.SetSource(w!("https://sftp-attachment.invalid/"))?;
        attachment.Save()
    })()
    .map_err(|_| OpenWithError::Attachment)
}

pub(crate) fn choose_application(
    path: &Path,
    parent: HWND,
) -> Result<OpenWithOutcome, OpenWithError> {
    let _apartment = Apartment::enter(OpenWithError::Picker)?;
    let path = HSTRING::from(path.as_os_str());
    let parameters = OPENASINFO {
        pcszFile: PCWSTR(path.as_ptr()),
        oaifInFlags: OAIF_EXEC,
        ..Default::default()
    };
    // Both the parent window and the UTF-16 filename stay alive through the modal call.
    match unsafe { SHOpenWithDialog(Some(parent), &parameters) } {
        Ok(()) => Ok(OpenWithOutcome::Opened),
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() || error.code() == E_ABORT => {
            Ok(OpenWithOutcome::Cancelled)
        }
        Err(_) => Err(OpenWithError::Picker),
    }
}
