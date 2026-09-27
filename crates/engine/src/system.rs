//! What the machine can spare for render workers: CPU cores, free memory, battery.

/// Logical CPU cores (1 if unknown).
pub fn cpu_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Physical memory currently available to programs, in MB, if the OS says.
pub fn free_memory_mb() -> Option<u64> {
    imp::free_memory_mb()
}

/// Whether the computer is running on battery (a laptop unplugged).
pub fn on_battery() -> bool {
    imp::on_battery()
}

/// Memory of a child process in MB, as Task Manager shows it (working set), if known.
pub fn process_memory_mb(child: &std::process::Child) -> Option<u64> {
    imp::process_memory_mb(child)
}

/// Memory of this process in MB (working set), if known.
pub fn own_memory_mb() -> Option<u64> {
    imp::own_memory_mb()
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    fn working_set_mb(process: HANDLE) -> Option<u64> {
        // SAFETY: plain Win32 call with a correctly sized, zero-initialised struct and a
        // process handle that stays valid for the call.
        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            (K32GetProcessMemoryInfo(process, &mut counters, size) != 0)
                .then_some(counters.WorkingSetSize as u64 / (1024 * 1024))
        }
    }

    pub fn process_memory_mb(child: &std::process::Child) -> Option<u64> {
        working_set_mb(child.as_raw_handle() as HANDLE)
    }

    pub fn own_memory_mb() -> Option<u64> {
        // SAFETY: returns a pseudo-handle that needs no closing.
        working_set_mb(unsafe { GetCurrentProcess() })
    }
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    pub fn free_memory_mb() -> Option<u64> {
        // SAFETY: plain Win32 call with a correctly sized, zero-initialised struct.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            let ok = GlobalMemoryStatusEx(&mut status) != 0;
            ok.then_some(status.ullAvailPhys / (1024 * 1024))
        }
    }

    pub fn on_battery() -> bool {
        // SAFETY: plain Win32 call with a zero-initialised struct.
        unsafe {
            let mut status: SYSTEM_POWER_STATUS = std::mem::zeroed();
            // ACLineStatus: 0 = offline (battery), 1 = online, 255 = unknown.
            GetSystemPowerStatus(&mut status) != 0 && status.ACLineStatus == 0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    fn rss_mb(status: &str) -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024)
    }

    pub fn process_memory_mb(child: &std::process::Child) -> Option<u64> {
        rss_mb(&std::fs::read_to_string(format!("/proc/{}/status", child.id())).ok()?)
    }

    pub fn own_memory_mb() -> Option<u64> {
        rss_mb(&std::fs::read_to_string("/proc/self/status").ok()?)
    }

    pub fn free_memory_mb() -> Option<u64> {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024)
    }

    pub fn on_battery() -> bool {
        let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
            return false;
        };
        entries.flatten().any(|entry| {
            let dir = entry.path();
            let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
            read("type").trim() == "Battery" && read("status").trim() == "Discharging"
        })
    }
}
