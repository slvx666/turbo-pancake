//! One running client per local profile (spec §9 with the tray: a second launch must
//! bring back the hidden window, not start a second session of the same account on the
//! same device). A named mutex decides who runs; the loser asks the winner to show its
//! window through the tray window ([`crate::tray::signal_running`]).

use std::time::{Duration, Instant};

pub use imp::InstanceLock;

/// Outcome of a launch.
pub enum Launch {
    /// This process is the profile's client; keep the lock for the whole run.
    First(InstanceLock),
    /// Another client of the profile runs; it was asked to show its window
    /// (`Some(pid)`), or did not answer in time (`None`). Either way this one exits.
    AlreadyRunning(Option<u32>),
}

/// Takes the profile's lock, or hands over to its running client. The running client
/// may still be starting (lock taken, tray window not yet created), so its window is
/// looked for for up to `wait`.
pub fn claim(profile: &str, wait: Duration) -> Result<Launch, String> {
    if let Some(lock) = InstanceLock::acquire(profile)? {
        return Ok(Launch::First(lock));
    }
    let deadline = Instant::now() + wait;
    loop {
        if let Some(pid) = crate::tray::signal_running(profile) {
            return Ok(Launch::AlreadyRunning(Some(pid)));
        }
        if Instant::now() >= deadline {
            return Ok(Launch::AlreadyRunning(None));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Session-local name (`Local\`): per Windows logon session, like the profile's data.
/// Backslashes are not allowed after the namespace prefix.
fn mutex_name(profile: &str) -> String {
    format!("Local\\turbo-pancake-{}", profile.replace('\\', "_"))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    pub struct InstanceLock(HANDLE);

    impl InstanceLock {
        /// `Some` for the first holder of the profile's lock, `None` while another
        /// handle to it exists (in any process of this logon session).
        pub fn acquire(profile: &str) -> Result<Option<InstanceLock>, String> {
            let name: Vec<u16> = super::mutex_name(profile)
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            unsafe {
                let handle =
                    CreateMutexW(None, false, PCWSTR(name.as_ptr())).map_err(|e| e.message())?;
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    return Ok(None);
                }
                Ok(Some(InstanceLock(handle)))
            }
        }
    }

    impl Drop for InstanceLock {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// No guard before S9 (ADR 0012).
    pub struct InstanceLock;

    impl InstanceLock {
        pub fn acquire(_profile: &str) -> Result<Option<InstanceLock>, String> {
            Ok(Some(InstanceLock))
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_per_profile_and_released_on_drop() {
        let a = InstanceLock::acquire("lock-test-a")
            .unwrap()
            .expect("first");
        assert!(InstanceLock::acquire("lock-test-a").unwrap().is_none());
        assert!(InstanceLock::acquire("lock-test-b").unwrap().is_some());
        drop(a);
        assert!(InstanceLock::acquire("lock-test-a").unwrap().is_some());
    }

    #[test]
    fn a_second_launch_without_an_answer_gives_up_after_the_wait() {
        let _a = InstanceLock::acquire("lock-test-c")
            .unwrap()
            .expect("first");
        let t = Instant::now();
        match claim("lock-test-c", Duration::from_millis(200)).unwrap() {
            Launch::AlreadyRunning(None) => {}
            _ => panic!("expected no answer"),
        }
        assert!(t.elapsed() >= Duration::from_millis(200));
    }
}
