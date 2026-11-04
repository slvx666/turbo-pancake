//! The signed-in session (token + user) in the Windows Credential Manager (spec §10:
//! secrets in the OS vault, not in SQLite or plain files). One generic credential per
//! server and profile; the blob is an `AuthResponse` (protobuf) with the token and user.

use prost::Message;
use turbo_pancake_protocol::v1;

/// Credential name for `server` and a local profile (several clients on one machine,
/// e.g. trials, use different profiles).
pub fn target(server: &str, profile: &str) -> String {
    format!("turbo-pancake/{profile}/{}", server.trim_end_matches('/'))
}

pub fn save(target: &str, token: &str, user: &v1::User) -> Result<(), String> {
    let blob = v1::AuthResponse {
        token: token.to_owned(),
        user: Some(user.clone()),
        ..Default::default()
    }
    .encode_to_vec();
    imp::write(target, &blob)
}

/// The stored token and user, if any.
pub fn load(target: &str) -> Option<(String, v1::User)> {
    let blob = imp::read(target)?;
    let a = v1::AuthResponse::decode(blob.as_slice()).ok()?;
    Some((a.token, a.user?)).filter(|(t, _)| !t.is_empty())
}

pub fn delete(target: &str) {
    imp::delete(target);
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Security::Credentials::{
        CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW,
        CredFree, CredReadW, CredWriteW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn write(target: &str, blob: &[u8]) -> Result<(), String> {
        let mut name = wide(target);
        let mut user = wide("turbo-pancake");
        let mut data = blob.to_vec();
        let cred = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            Comment: PWSTR::null(),
            LastWritten: FILETIME::default(),
            CredentialBlobSize: data.len() as u32,
            CredentialBlob: data.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: PWSTR::null(),
            UserName: PWSTR(user.as_mut_ptr()),
        };
        // SAFETY: every pointer refers to a live local buffer for the call's duration.
        unsafe { CredWriteW(&cred, 0) }.map_err(|e| e.message())
    }

    pub fn read(target: &str) -> Option<Vec<u8>> {
        let name = wide(target);
        let mut out: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated name; `out` is freed with CredFree below.
        unsafe {
            CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut out).ok()?;
            let c = &*out;
            let blob = if c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            };
            CredFree(out.cast());
            Some(blob)
        }
    }

    pub fn delete(target: &str) {
        let name = wide(target);
        // SAFETY: valid NUL-terminated name.
        let _ = unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) };
    }
}

#[cfg(not(windows))]
mod imp {
    // S9: macOS Keychain / Secret Service. Until then nothing is stored (sign in again).
    pub fn write(_: &str, _: &[u8]) -> Result<(), String> {
        Err("no credential store on this system yet".into())
    }
    pub fn read(_: &str) -> Option<Vec<u8>> {
        None
    }
    pub fn delete(_: &str) {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn round_trip_in_the_credential_manager() {
        let t = target(
            "http://127.0.0.1:1",
            &format!("test-{}", std::process::id()),
        );
        let user = v1::User {
            user_id: "u".into(),
            username: "alice".into(),
            display_name: "Алиса".into(),
            online: false,
        };
        save(&t, "secret-token", &user).unwrap();
        let (token, back) = load(&t).unwrap();
        assert_eq!(
            (token.as_str(), back.display_name.as_str()),
            ("secret-token", "Алиса")
        );
        delete(&t);
        assert!(load(&t).is_none());
    }
}
