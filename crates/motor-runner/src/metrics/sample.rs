use motor_lease::metrics::{Cpu, Disk, HostMetrics, Memory};
use std::io;

#[derive(Default)]
pub(super) struct Sampler {
    previous: Option<(u64, u64)>,
}

impl Sampler {
    pub(super) fn sample(&mut self, at_ms: u64) -> io::Result<Option<HostMetrics>> {
        let (mut cpu, mem, disks, total, idle) = platform()?;
        let Some((old_total, old_idle)) = self.previous.replace((total, idle)) else {
            return Ok(None);
        };
        let elapsed = total.saturating_sub(old_total);
        if elapsed == 0 {
            return Ok(None);
        }
        cpu.busy_pct = elapsed
            .saturating_sub(idle.saturating_sub(old_idle))
            .saturating_mul(100)
            .checked_div(elapsed)
            .unwrap_or(0)
            .min(100);
        Ok(Some(HostMetrics {
            at_ms,
            cpu,
            mem,
            disks,
            tenants: vec![],
            runners: vec![],
        }))
    }
}

#[cfg(target_os = "linux")]
pub(super) fn cpu_ticks(stat: &str) -> io::Result<(u64, u64, u64)> {
    let ticks = stat
        .lines()
        .next()
        .ok_or_else(|| io::Error::other("cpu counters"))?
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    if ticks.len() < 5 {
        return Err(io::Error::other("cpu counters"));
    }
    let threads = stat
        .lines()
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|suffix| suffix.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count() as u64;
    Ok((ticks.iter().sum(), ticks[3] + ticks[4], threads))
}

#[cfg(target_os = "linux")]
pub(super) fn memory(text: &str) -> io::Result<Memory> {
    let kib = |name: &str| -> io::Result<u64> {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .ok_or_else(|| io::Error::other("memory counter"))?
            .parse()
            .map_err(io::Error::other)
    };
    Ok(Memory {
        total_mib: kib("MemTotal:")? / 1024,
        avail_mib: kib("MemAvailable:")? / 1024,
    })
}

#[cfg(target_os = "linux")]
fn platform() -> io::Result<(Cpu, Memory, Vec<Disk>, u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat")?;
    let (total, idle, threads) = cpu_ticks(&stat)?;
    let load: f64 = std::fs::read_to_string("/proc/loadavg")?
        .split_whitespace()
        .next()
        .ok_or_else(|| io::Error::other("load1"))?
        .parse()
        .map_err(io::Error::other)?;
    let mem = memory(&std::fs::read_to_string("/proc/meminfo")?)?;
    Ok((
        Cpu {
            threads,
            busy_pct: 0,
            load1: Some((load * 100.0).round() / 100.0),
        },
        mem,
        root_disk().into_iter().collect(),
        total,
        idle,
    ))
}

#[cfg(unix)]
fn root_disk() -> Option<Disk> {
    Some(Disk {
        mount: "/".into(),
        total_gib: fs2::total_space("/").ok()? >> 30,
        free_gib: fs2::available_space("/").ok()? >> 30,
    })
}

#[cfg(target_os = "macos")]
fn platform() -> io::Result<(Cpu, Memory, Vec<Disk>, u64, u64)> {
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: i32, data: *mut i32, count: *mut u32) -> i32;
        fn host_statistics64(host: u32, flavor: i32, data: *mut i32, count: *mut u32) -> i32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        static mach_task_self_: u32;
    }
    let mut total_mem = 0u64;
    let mut size = std::mem::size_of::<u64>();
    let mut load = 0f64;
    let mut ticks = [0u32; 4];
    // Mach's VM result starts with free, active, inactive and wired page
    // counts. A generously sized aligned buffer tolerates newer trailing fields.
    let mut vm = [0u64; 64];
    let mut cpu_count = 4;
    let mut vm_count = (std::mem::size_of_val(&vm) / 4) as u32;
    let host = unsafe { mach_host_self() };
    let cpu_ok = unsafe { host_statistics(host, 3, ticks.as_mut_ptr().cast(), &mut cpu_count) };
    let mem_ok = unsafe { host_statistics64(host, 4, vm.as_mut_ptr().cast(), &mut vm_count) };
    unsafe { mach_port_deallocate(mach_task_self_, host) };
    if cpu_ok != 0
        || mem_ok != 0
        || cpu_count < 4
        || vm_count < 4
        || unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut total_mem).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        || unsafe { libc::getloadavg(&mut load, 1) } != 1
    {
        return Err(io::Error::other("macOS host counters unavailable"));
    }
    let pages = vm.as_ptr().cast::<u32>();
    let available = u64::from(unsafe { *pages }) + u64::from(unsafe { *pages.add(2) });
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(io::Error::other("macOS page size unavailable"));
    }
    let mem = Memory {
        total_mib: total_mem >> 20,
        avail_mib: (available * page_size as u64) >> 20,
    };
    Ok((
        Cpu {
            threads: std::thread::available_parallelism()?.get() as u64,
            busy_pct: 0,
            load1: Some((load * 100.0).round() / 100.0),
        },
        mem,
        root_disk().into_iter().collect(),
        ticks.iter().map(|t| u64::from(*t)).sum(),
        u64::from(ticks[2]),
    ))
}

#[cfg(windows)]
fn platform() -> io::Result<(Cpu, Memory, Vec<Disk>, u64, u64)> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::GetSystemTimes,
        },
    };
    let mut idle: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0
        || unsafe { GlobalMemoryStatusEx(&mut mem) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let system_drive = std::env::var("SystemDrive").map_err(io::Error::other)? + "\\";
    let disks = match (
        fs2::total_space(&system_drive),
        fs2::available_space(&system_drive),
    ) {
        (Ok(total), Ok(free)) => vec![Disk {
            mount: system_drive,
            total_gib: total >> 30,
            free_gib: free >> 30,
        }],
        _ => vec![],
    };
    Ok((
        Cpu {
            threads: std::thread::available_parallelism()?.get() as u64,
            busy_pct: 0,
            load1: None,
        },
        Memory {
            total_mib: mem.ullTotalPhys >> 20,
            avail_mib: mem.ullAvailPhys >> 20,
        },
        disks,
        ticks(kernel) + ticks(user),
        ticks(idle),
    ))
}
