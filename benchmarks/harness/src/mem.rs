//! Per-process memory sampling.
//!
//! Windows: working set, peak working set and private bytes (commit charge)
//! via `GetProcessMemoryInfo`. Linux: VmRSS / VmHWM and private (anon+file
//! private) pages from `/proc/<pid>/smaps_rollup`. macOS: RSS via `sysinfo`
//! (private bytes not yet sampled: UNVERIFIED there).

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MemSample {
    pub working_set_bytes: u64,
    pub peak_working_set_bytes: u64,
    /// `None` when the platform probe is not implemented.
    pub private_bytes: Option<u64>,
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn sample(pid: u32) -> anyhow::Result<MemSample> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: plain Win32 calls with a correctly sized, zero-initialised out
    // struct; the handle is closed on every path.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            anyhow::bail!("OpenProcess({pid}) failed");
        }
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let ok = GetProcessMemoryInfo(
            h,
            (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            c.cb,
        );
        CloseHandle(h);
        if ok == 0 {
            anyhow::bail!("GetProcessMemoryInfo({pid}) failed");
        }
        Ok(MemSample {
            working_set_bytes: c.WorkingSetSize as u64,
            peak_working_set_bytes: c.PeakWorkingSetSize as u64,
            private_bytes: Some(c.PrivateUsage as u64),
        })
    }
}

#[cfg(target_os = "linux")]
pub fn sample(pid: u32) -> anyhow::Result<MemSample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let kib = |key: &str, text: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v * 1024)
    };
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
    let private = match (
        kib("Private_Clean:", &rollup),
        kib("Private_Dirty:", &rollup),
    ) {
        (Some(c), Some(d)) => Some(c + d),
        _ => None,
    };
    Ok(MemSample {
        working_set_bytes: kib("VmRSS:", &status).unwrap_or(0),
        peak_working_set_bytes: kib("VmHWM:", &status).unwrap_or(0),
        private_bytes: private,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn sample(pid: u32) -> anyhow::Result<MemSample> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new()?;
    let pid = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let p = sys
        .process(pid)
        .ok_or_else(|| anyhow::anyhow!("process {pid} not found"))?;
    Ok(MemSample {
        working_set_bytes: p.memory(),
        peak_working_set_bytes: 0,
        private_bytes: None,
    })
}
