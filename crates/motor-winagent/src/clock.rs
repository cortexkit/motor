//! The authenticated host supplies UTC before the guest can prepare or run a job.
use serde::{Deserialize, Serialize};
use std::{fmt, io, time::SystemTime};

// Reject corrupt timestamps without trusting the guest's unsynchronized hardware clock.
// These bounds admit 2024 through 2099, including hosts with seasonal local time.
const MIN_UNIX_MS: u64 = 1_704_067_200_000;
const MAX_UNIX_MS: u64 = 4_102_444_800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    ImplausibleTime,
    SetSystemTimeFailed,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ImplausibleTime => "windows_clock_implausible_time",
            Self::SetSystemTimeFailed => "windows_clock_set_system_time_failed",
        })
    }
}
impl std::error::Error for Refusal {}
impl From<Refusal> for io::Error {
    fn from(value: Refusal) -> Self {
        io::Error::other(value)
    }
}

pub fn synchronize(
    unix_ms: u64,
    mut set: impl FnMut(SystemTime) -> io::Result<()>,
) -> Result<i64, Refusal> {
    if !(MIN_UNIX_MS..MAX_UNIX_MS).contains(&unix_ms) {
        return Err(Refusal::ImplausibleTime);
    }
    let before = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i128,
        Err(error) => -(error.duration().as_millis() as i128),
    };
    let requested = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(unix_ms);
    set(requested).map_err(|_| Refusal::SetSystemTimeFailed)?;
    Ok((i128::from(unix_ms) - before).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

#[cfg(windows)]
pub fn set_system_time(time: SystemTime) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::{SystemInformation::SetSystemTime, Time::FileTimeToSystemTime},
    };
    let unix_ms = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let ticks =
        u64::try_from(unix_ms).map_err(io::Error::other)? * 10_000 + 116_444_736_000_000_000;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { FileTimeToSystemTime(&file, &mut utc) } == 0 || unsafe { SetSystemTime(&utc) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_implausible_time_without_setting_clock() {
        for time in [0, MIN_UNIX_MS - 1, MAX_UNIX_MS, u64::MAX] {
            assert_eq!(
                synchronize(time, |_| panic!("invalid clock reached setter")),
                Err(Refusal::ImplausibleTime)
            );
        }
    }

    #[test]
    fn setter_failure_refuses_job() {
        assert_eq!(
            synchronize(MIN_UNIX_MS, |_| Err(io::Error::other("denied"))),
            Err(Refusal::SetSystemTimeFailed)
        );
    }

    #[test]
    fn host_utc_reaches_setter_before_success() {
        let mut set = false;
        let unix_ms = MIN_UNIX_MS + 1234;
        synchronize(unix_ms, |requested| {
            assert_eq!(
                requested
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
                u128::from(unix_ms)
            );
            set = true;
            Ok(())
        })
        .unwrap();
        assert!(set);
    }
}
