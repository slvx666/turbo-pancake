//! Real-time scheduling for the threads that carry voice.
//!
//! The device callbacks already run on the audio backend's own real-time threads, but
//! everything between them and the network (resampling, Opus, jitter buffer, the call's
//! packet pump) runs on our worker threads. At the default priority those lose the CPU
//! whenever a game or a build saturates every core, and 40 ms of queued audio is gone
//! after two missed scheduler quanta: crackling exactly when the user is playing.
//!
//! Windows: the Multimedia Class Scheduler Service lifts the calling thread into the
//! "Pro Audio" task (priority band above normal applications, CPU share guaranteed by
//! the system). The registration ends with the thread. Other systems: not implemented
//! (S9); the call reports `false` and the thread keeps its priority.

/// Registers the calling thread for audio-class scheduling until it exits. Returns
/// whether the system granted it (e.g. `false` when the service is disabled).
pub fn boost_current_thread() -> bool {
    imp::boost()
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    use windows::core::w;

    pub fn boost() -> bool {
        let mut task_index = 0u32;
        // SAFETY: plain call for the current thread; the handle it returns is only
        // needed to revert early, and the registration ends with the thread.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
            .is_ok_and(|h| !h.is_invalid())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn boost() -> bool {
        false
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn a_worker_thread_can_be_registered() {
        // On a helper thread: the test harness's own thread keeps its priority.
        let granted = std::thread::spawn(super::boost_current_thread)
            .join()
            .unwrap();
        // The service may be disabled on a stripped-down system; the call must not fail
        // in any other way.
        println!("audio-class scheduling granted: {granted}");
    }
}
