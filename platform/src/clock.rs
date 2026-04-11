// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform real-time clock for CMOS/RTC device emulation.

/// Broken-down UTC time components.
#[derive(Debug, Default, Clone, Copy)]
pub struct UtcTime {
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    pub mday: i32,
    pub mon: i32,  // 0-11
    pub year: i32, // years since 1900
    pub wday: i32, // 0 = Sunday
    pub nsec: i64, // nanoseconds within second
}

/// Get the current UTC time broken down into components.
#[cfg(unix)]
pub fn get_utc_time() -> UtcTime {
    // SAFETY: zeroed timespec/tm are valid, clock_gettime and gmtime_r
    // write into them.
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&ts.tv_sec, &mut tm);
        UtcTime {
            sec: tm.tm_sec,
            min: tm.tm_min,
            hour: tm.tm_hour,
            mday: tm.tm_mday,
            mon: tm.tm_mon,
            year: tm.tm_year,
            wday: tm.tm_wday,
            nsec: ts.tv_nsec,
        }
    }
}

#[cfg(target_os = "windows")]
pub fn get_utc_time() -> UtcTime {
    use windows::Win32::System::SystemInformation::GetSystemTime;

    // SAFETY: GetSystemTime returns a valid SYSTEMTIME struct.
    let st = unsafe { GetSystemTime() };

    UtcTime {
        sec: st.wSecond as i32,
        min: st.wMinute as i32,
        hour: st.wHour as i32,
        mday: st.wDay as i32,
        mon: (st.wMonth as i32) - 1, // Windows: 1-12 → C-style: 0-11
        year: (st.wYear as i32) - 1900,
        wday: st.wDayOfWeek as i32,
        nsec: (st.wMilliseconds as i64) * 1_000_000,
    }
}
