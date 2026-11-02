//! Human durations for the command line: `90s`, `30m`, `2h`, `1h30m`, `1d`.

use std::time::Duration;

/// Parses a sequence of `<number><unit>` parts, with units `s`, `m`, `h` and `d`. The total must be positive.
pub fn parse(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let invalid = || format!("invalid duration `{text}`: use for example 90s, 30m, 2h or 1h30m");
    let mut total: u64 = 0;
    let mut digits = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let unit = match character.to_ascii_lowercase() {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return Err(invalid()),
        };
        let value: u64 = digits.parse().map_err(|_| invalid())?;
        digits.clear();
        total = value
            .checked_mul(unit)
            .and_then(|seconds| total.checked_add(seconds))
            .ok_or_else(invalid)?;
    }
    if !digits.is_empty() || total == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(total))
}

/// Formats a duration the way `parse` reads it, e.g. `1h30m`. Sub-second precision is dropped.
pub fn format(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    if seconds == 0 {
        return "0s".to_owned();
    }
    let mut text = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if seconds >= size {
            text.push_str(&format!("{}{unit}", seconds / size));
            seconds %= size;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_and_combined_units() {
        assert_eq!(parse("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse("2H"), Ok(Duration::from_secs(7_200)));
        assert_eq!(parse("1h30m"), Ok(Duration::from_secs(5_400)));
        assert_eq!(parse(" 1d "), Ok(Duration::from_secs(86_400)));
    }

    #[test]
    fn rejects_missing_units_zero_and_garbage() {
        for text in [
            "",
            "30",
            "0m",
            "m",
            "1h30",
            "10x",
            "-5m",
            "1.5h",
            "99999999999999999999d",
        ] {
            assert!(parse(text).is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn formats_what_it_parses() {
        for text in ["45s", "30m", "2h", "1h30m", "1d2h3m4s"] {
            assert_eq!(format(parse(text).unwrap()), text);
        }
    }
}
