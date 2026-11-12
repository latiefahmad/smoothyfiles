//! Time conversions. ZIP stores DOS local time (no zone), so we convert through the Windows
//! time-zone APIs to match what Explorer shows.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime,
};

/// 100 ns ticks between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_TICKS: u64 = 116_444_736_000_000_000;

pub fn systemtime_to_filetime(t: SystemTime) -> Option<FILETIME> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    let ticks = EPOCH_DIFF_TICKS + (d.as_nanos() / 100) as u64;
    Some(FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 })
}

pub fn filetime_to_systemtime(ft: FILETIME) -> Option<SystemTime> {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    let since = ticks.checked_sub(EPOCH_DIFF_TICKS)?;
    Some(UNIX_EPOCH + Duration::from_nanos(since.checked_mul(100)?))
}

pub fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// DOS local time fields -> `SystemTime` (UTC based).
pub fn dos_local_to_systemtime(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> Option<SystemTime> {
    let local = SYSTEMTIME {
        wYear: y,
        wMonth: mo as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: h as u16,
        wMinute: mi as u16,
        wSecond: s as u16,
        wMilliseconds: 0,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, &local, &mut utc).ok()?;
        let mut ft = FILETIME::default();
        SystemTimeToFileTime(&utc, &mut ft).ok()?;
        filetime_to_systemtime(ft)
    }
}

/// `SystemTime` -> local (year, month, day, hour, minute, second).
pub fn systemtime_to_local_fields(t: SystemTime) -> Option<(u16, u8, u8, u8, u8, u8)> {
    let ft = systemtime_to_filetime(t)?;
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some((local.wYear, local.wMonth as u8, local.wDay as u8, local.wHour as u8, local.wMinute as u8, local.wSecond as u8))
}
