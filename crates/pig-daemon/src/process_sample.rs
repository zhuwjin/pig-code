use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcessSample {
    pub cpu: Option<f64>,
    pub memory: Option<u64>,
    pub gpu: Option<f64>,
}

struct PrevCpu {
    at: Instant,
    cpu_seconds: f64,
}

#[derive(Default)]
pub struct ProcessSampler {
    prev: Mutex<Option<PrevCpu>>,
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample(&self) -> ProcessSample {
        let memory = current_memory_bytes();
        let cpu = current_cpu_seconds().map(|cpu_seconds| {
            let now = Instant::now();
            let mut prev = self
                .prev
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let percent = prev.as_ref().and_then(|last| {
                let elapsed = now.duration_since(last.at).as_secs_f64();
                if elapsed > 0.05 {
                    let delta = (cpu_seconds - last.cpu_seconds).max(0.0);
                    Some((delta / elapsed) * 100.0)
                } else {
                    None
                }
            });
            *prev = Some(PrevCpu {
                at: now,
                cpu_seconds,
            });
            percent.unwrap_or(0.0)
        });
        ProcessSample {
            cpu,
            memory,
            gpu: None,
        }
    }
}

#[cfg(windows)]
fn current_cpu_seconds() -> Option<f64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn filetime_to_secs(ft: FILETIME) -> f64 {
        let ticks = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
        (ticks as f64) * 1e-7
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    Some(filetime_to_secs(kernel) + filetime_to_secs(user))
}

#[cfg(windows)]
fn current_memory_bytes() -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .ok()?;
    }
    Some(counters.WorkingSetSize as u64)
}

#[cfg(unix)]
fn current_cpu_seconds() -> Option<f64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    let user = (usage.ru_utime.tv_sec as f64) + (usage.ru_utime.tv_usec as f64) * 1e-6;
    let sys = (usage.ru_stime.tv_sec as f64) + (usage.ru_stime.tv_usec as f64) * 1e-6;
    Some(user + sys)
}

#[cfg(target_os = "linux")]
fn current_memory_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(rss_pages.saturating_mul(page_size as u64))
}

#[cfg(target_os = "macos")]
fn current_memory_bytes() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 || usage.ru_maxrss <= 0 {
        return None;
    }
    Some(usage.ru_maxrss as u64)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn current_memory_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(windows, unix)))]
fn current_cpu_seconds() -> Option<f64> {
    None
}
