use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn timestamp() -> String {
    epoch_millis().to_string()
}

pub(crate) fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn duration_ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10_000.0).round() / 10.0
}

pub(crate) fn parse_timestamp(value: &str) -> Option<u128> {
    value.parse::<u128>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_epoch_millis_string() {
        let value = timestamp();
        assert!(value.parse::<u128>().is_ok());
        assert!(value.len() >= 13);
    }

    #[test]
    fn duration_ms_rounds_to_tenths_of_a_millisecond() {
        assert_eq!(duration_ms(Duration::from_micros(1_234)), 1.2);
        assert_eq!(duration_ms(Duration::from_micros(1_250)), 1.3);
    }

    #[test]
    fn parse_timestamp_accepts_integer_epoch_millis_only() {
        assert_eq!(parse_timestamp("123"), Some(123));
        assert_eq!(parse_timestamp("12.3"), None);
        assert_eq!(parse_timestamp("not-a-timestamp"), None);
    }
}
