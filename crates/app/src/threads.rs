//! CPU time used by each thread of this process, for finding out what wakes an idle app.
//! Only used by the automation (performance tests).

use std::collections::HashMap;

/// CPU time per thread so far, in milliseconds, keyed by thread id, with the thread's
/// name ("pdfium", "render-pool", ... or empty for threads that have none, such as ones
/// started by the graphics driver).
pub fn cpu_times() -> HashMap<u64, (String, f64)> {
    imp::cpu_times()
}

/// Threads that used CPU between two readings, busiest first, as (name, milliseconds).
pub fn busy_between(
    before: &HashMap<u64, (String, f64)>,
    after: &HashMap<u64, (String, f64)>,
) -> Vec<(String, f64)> {
    let mut busy: Vec<(String, f64)> = after
        .iter()
        .filter_map(|(tid, (name, ms))| {
            let used = ms - before.get(tid).map_or(0.0, |(_, earlier)| *earlier);
            let name = if name.is_empty() {
                format!("thread {tid}")
            } else {
                name.clone()
            };
            (used > 0.0).then_some((name, used))
        })
        .collect();
    busy.sort_by(|a, b| b.1.total_cmp(&a.1));
    busy
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadTimes, OpenThread,
        THREAD_QUERY_LIMITED_INFORMATION,
    };

    fn ms(time: FILETIME) -> f64 {
        // FILETIME counts 100-nanosecond intervals.
        (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) as f64 / 10_000.0
    }

    pub fn cpu_times() -> HashMap<u64, (String, f64)> {
        let mut threads = HashMap::new();
        // SAFETY: Win32 calls with correctly sized, zero-initialised structs; every handle
        // opened here is closed, and the description buffer is freed with LocalFree.
        unsafe {
            let pid = GetCurrentProcessId();
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return threads;
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread =
                        OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        let mut times: [FILETIME; 4] = std::mem::zeroed();
                        let [created, exited, kernel, user] = &mut times;
                        if GetThreadTimes(thread, created, exited, kernel, user) != 0 {
                            let mut name = String::new();
                            let mut text = std::ptr::null_mut();
                            if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
                                let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                                name =
                                    String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                                LocalFree(text as _);
                            }
                            threads
                                .insert(entry.th32ThreadID as u64, (name, ms(*kernel) + ms(*user)));
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        threads
    }
}

#[cfg(not(windows))]
mod imp {
    use std::collections::HashMap;

    /// Linux reports thread CPU time in clock ticks, which are 1/100 s on every common
    /// configuration.
    const MS_PER_TICK: f64 = 10.0;

    pub fn cpu_times() -> HashMap<u64, (String, f64)> {
        let mut threads = HashMap::new();
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            return threads;
        };
        for task in tasks.flatten() {
            let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|t| t.parse::<u64>().ok())
            else {
                continue;
            };
            let dir = task.path();
            let name = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
            let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
                continue;
            };
            // Fields after the parenthesised name: utime and stime are the 12th and 13th.
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let ticks = |i: usize| {
                fields
                    .get(i)
                    .and_then(|f| f.parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            threads.insert(
                tid,
                (
                    name.trim().to_owned(),
                    (ticks(11) + ticks(12)) * MS_PER_TICK,
                ),
            );
        }
        threads
    }
}
