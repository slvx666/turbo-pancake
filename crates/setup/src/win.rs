//! Windows integration of the installer (ADR 0017): the per-user uninstall entry, the
//! Start-menu shortcut, message boxes and removal of stored sessions.

#![allow(unsafe_code)]

use std::path::Path;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Security::Credentials::{
    CRED_ENUMERATE_FLAGS, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{
    IDOK, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_OKCANCEL, MB_YESNO,
    MESSAGEBOX_STYLE, MessageBoxW,
};
use windows::core::{HSTRING, Interface, PCWSTR};

const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What "Apps & features" shows and runs.
pub struct UninstallEntry<'a> {
    pub key: &'a str,
    pub display_name: &'a str,
    pub version: &'a str,
    pub install_location: &'a Path,
    pub icon: &'a Path,
    pub uninstall: &'a str,
    pub quiet_uninstall: &'a str,
    pub size_kb: u32,
}

pub fn register_uninstall(e: &UninstallEntry) -> Result<(), String> {
    let sub = wide(&format!("{UNINSTALL}\\{}", e.key));
    let mut key = HKEY::default();
    // SAFETY: valid NUL-terminated strings and an out-pointer to a local HKEY.
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if r != ERROR_SUCCESS {
        return Err(format!("registry: {r:?}"));
    }
    let set_str = |name: &str, value: &str| {
        let n = wide(name);
        let v = wide(value);
        // SAFETY: `v` is a NUL-terminated UTF-16 buffer, passed as bytes with its length.
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2) };
        unsafe { RegSetValueExW(key, PCWSTR(n.as_ptr()), None, REG_SZ, Some(bytes)) }
    };
    let set_dword = |name: &str, value: u32| {
        let n = wide(name);
        // SAFETY: a 4-byte value buffer.
        unsafe {
            RegSetValueExW(
                key,
                PCWSTR(n.as_ptr()),
                None,
                REG_DWORD,
                Some(&value.to_le_bytes()),
            )
        }
    };
    let results = [
        set_str("DisplayName", e.display_name),
        set_str("DisplayVersion", e.version),
        set_str("Publisher", "turbo-pancake"),
        set_str("InstallLocation", &e.install_location.display().to_string()),
        set_str("DisplayIcon", &e.icon.display().to_string()),
        set_str("UninstallString", e.uninstall),
        set_str("QuietUninstallString", e.quiet_uninstall),
        set_dword("EstimatedSize", e.size_kb),
        set_dword("NoModify", 1),
        set_dword("NoRepair", 1),
    ];
    // SAFETY: the key was opened above.
    unsafe {
        let _ = RegCloseKey(key);
    }
    match results.iter().find(|r| **r != ERROR_SUCCESS) {
        Some(r) => Err(format!("registry value: {r:?}")),
        None => Ok(()),
    }
}

pub fn unregister_uninstall(key: &str) -> Result<(), String> {
    let sub = wide(&format!("{UNINSTALL}\\{key}"));
    // SAFETY: valid NUL-terminated key name under HKCU.
    let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr())) };
    // Already gone is fine (ERROR_FILE_NOT_FOUND = 2).
    if r == ERROR_SUCCESS || r.0 == 2 {
        Ok(())
    } else {
        Err(format!("registry: {r:?}"))
    }
}

/// A `.lnk` to `target` (COM IShellLinkW + IPersistFile).
pub fn create_shortcut(
    lnk: &Path,
    target: &Path,
    workdir: &Path,
    description: &str,
) -> Result<(), String> {
    if let Some(parent) = lnk.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // SAFETY: COM is initialised for this call and released before returning; the
    // interfaces are dropped (released) inside the block.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(target.as_os_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(workdir.as_os_str()))?;
            link.SetDescription(&HSTRING::from(description))?;
            link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
            let file: IPersistFile = link.cast()?;
            file.Save(&HSTRING::from(lnk.as_os_str()), true)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result.map_err(|e| e.message())
    }
}

/// Deletes stored sessions whose target starts with `prefix` (e.g. `turbo-pancake/`).
pub fn delete_credentials(prefix: &str) -> usize {
    let filter = wide(&format!("{prefix}*"));
    let mut count = 0u32;
    let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: out-pointers to locals; the list is freed with CredFree.
    unsafe {
        if CredEnumerateW(
            PCWSTR(filter.as_ptr()),
            Some(CRED_ENUMERATE_FLAGS(0)),
            &mut count,
            &mut list,
        )
        .is_err()
        {
            return 0;
        }
        let mut deleted = 0;
        for i in 0..count as usize {
            let c = &**list.add(i);
            if c.Type == CRED_TYPE_GENERIC
                && CredDeleteW(PCWSTR(c.TargetName.0), CRED_TYPE_GENERIC, None).is_ok()
            {
                deleted += 1;
            }
        }
        CredFree(list.cast());
        deleted
    }
}

pub enum Ask {
    OkCancel,
    YesNo,
}

fn show(text: &str, style: MESSAGEBOX_STYLE) -> i32 {
    let t = HSTRING::from(text);
    let c = HSTRING::from("turbo-pancake");
    // SAFETY: plain modal message box without an owner window.
    unsafe { MessageBoxW(None, &t, &c, style).0 }
}

pub fn info(text: &str) {
    show(text, MB_OK | MB_ICONINFORMATION);
}

pub fn error(text: &str) {
    show(text, MB_OK | MB_ICONERROR);
}

/// True for OK / Yes.
pub fn ask(text: &str, kind: Ask) -> bool {
    match kind {
        Ask::OkCancel => show(text, MB_OKCANCEL | MB_ICONQUESTION) == IDOK.0,
        Ask::YesNo => show(text, MB_YESNO | MB_ICONQUESTION) == IDYES.0,
    }
}
