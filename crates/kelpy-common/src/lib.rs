//! Kelpy common: the few helpers every Kelpy crate shares, so that each exists once.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Locks `mutex`, recovering it if a thread panicked while holding it. Kelpy's critical sections only insert,
/// remove or replace whole values, so a panic inside one cannot leave the data half-changed.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The current Unix time, in seconds: the unit of every timestamp Kelpy stores or reports.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A Unix time as a date and time in UTC, such as `2026-11-03 14:05 UTC`.
pub fn utc(seconds: i64) -> String {
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // From Howard Hinnant's days-to-civil algorithm.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// This computer's name.
pub fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this server".to_owned())
}

/// This computer's operating system, as `Ubuntu 24.04` where `/etc/os-release` names it, else as `Windows`.
pub fn system_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|release| os_release_name(&release))
        .unwrap_or_else(|| {
            match std::env::consts::OS {
                "windows" => "Windows",
                "macos" => "macOS",
                "linux" => "Linux",
                other => other,
            }
            .to_owned()
        })
}

/// The name and version in an `os-release` file, such as `Ubuntu 24.04`.
fn os_release_name(release: &str) -> Option<String> {
    let field = |key: &str| {
        release.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            Some(value.trim().trim_matches('"').to_owned()).filter(|value| !value.is_empty())
        })
    };
    let name = field("NAME")?;
    Some(match field("VERSION_ID") {
        Some(version) => format!("{name} {version}"),
        None => name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_is_named_with_its_version_from_os_release() {
        let ubuntu = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n";
        assert_eq!(os_release_name(ubuntu).as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(
            os_release_name("NAME=Arch Linux\n").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(os_release_name("ID=x\n"), None);
    }

    #[test]
    fn a_poisoned_mutex_is_still_usable() {
        let mutex = std::sync::Arc::new(Mutex::new(1));
        let poisoner = std::sync::Arc::clone(&mutex);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(mutex.is_poisoned());
        *lock(&mutex) += 1;
        assert_eq!(*lock(&mutex), 2);
    }

    #[test]
    fn the_clock_is_after_2026() {
        assert!(unix_now() > 1_767_225_600);
    }

    #[test]
    fn dates_are_written_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_793_718_300), "2026-11-03 15:05 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
    }
}
