use fm_frame::TimeBase;

use crate::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AudioSeek {
    pub input_microseconds: Option<u64>,
    pub expected_first_pts: Option<i64>,
    pub correction_samples: usize,
}

pub(crate) fn sample_pts(pts: i64, sample_rate: u32, time_base: TimeBase) -> Result<i64, Error> {
    let numerator = i128::from(pts)
        .checked_mul(i128::from(time_base.numerator()))
        .and_then(|value| value.checked_mul(i128::from(sample_rate)))
        .ok_or(Error::InvalidTimeline)?;
    let denominator = i128::from(time_base.denominator());
    if numerator.rem_euclid(denominator) != 0 {
        return Err(Error::InvalidTimeline);
    }
    i64::try_from(numerator.div_euclid(denominator)).map_err(|_| Error::InvalidTimeline)
}

pub(crate) fn timestamp_microseconds_floor(pts: i64, time_base: TimeBase) -> Result<i64, Error> {
    let numerator = i128::from(pts)
        .checked_mul(i128::from(time_base.numerator()))
        .and_then(|value| value.checked_mul(1_000_000))
        .ok_or(Error::InvalidTimeline)?;
    i64::try_from(numerator.div_euclid(i128::from(time_base.denominator())))
        .map_err(|_| Error::InvalidTimeline)
}

pub(crate) fn parse_input_start_microseconds(value: Option<&str>) -> Result<i64, Error> {
    let Some(value) = value else {
        return Ok(0);
    };
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::MalformedProbe);
    }
    let (microseconds, remainder) = fraction.split_at(fraction.len().min(6));
    if remainder.bytes().any(|byte| byte != b'0') {
        return Err(Error::MalformedProbe);
    }
    let mut padded = microseconds.to_owned();
    padded.extend(std::iter::repeat_n('0', 6 - padded.len()));
    let whole = whole.parse::<i128>().map_err(|_| Error::MalformedProbe)?;
    let fraction = padded.parse::<i128>().map_err(|_| Error::MalformedProbe)?;
    let value = whole
        .checked_mul(1_000_000)
        .and_then(|whole| whole.checked_add(fraction))
        .ok_or(Error::MalformedProbe)?;
    let value = if negative {
        value.checked_neg().ok_or(Error::MalformedProbe)?
    } else {
        value
    };
    i64::try_from(value).map_err(|_| Error::MalformedProbe)
}

pub(crate) fn validate_diagnostic(
    stderr: &[u8],
    expected_first_pts: i64,
    expected_sample_rate: u32,
) -> Result<(), Error> {
    let stderr = String::from_utf8_lossy(stderr);
    let line = stderr
        .lines()
        .find(|line| {
            line.contains("ashowinfo") && line.split_whitespace().any(|part| part == "n:0")
        })
        .ok_or(Error::InvalidTimeline)?;
    let pts = diagnostic_value(line, "pts:")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(Error::InvalidTimeline)?;
    let sample_rate = diagnostic_value(line, "rate:")
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(Error::InvalidTimeline)?;
    if pts == expected_first_pts && sample_rate == expected_sample_rate {
        Ok(())
    } else {
        Err(Error::InvalidTimeline)
    }
}

fn diagnostic_value<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_math_preserves_nonzero_pts_at_common_sample_rates() {
        assert_eq!(
            parse_input_start_microseconds(Some("5.000000")),
            Ok(5_000_000)
        );
        assert_eq!(
            parse_input_start_microseconds(Some("-0.125000")),
            Ok(-125_000)
        );
        for sample_rate in [44_100, 48_000] {
            let time_base = TimeBase::new(1, sample_rate).unwrap();
            let base = i64::from(sample_rate) * 5;
            assert_eq!(
                sample_pts(base + 2_048, sample_rate, time_base),
                Ok(base + 2_048)
            );
            assert_eq!(
                timestamp_microseconds_floor(base + 2_048, time_base),
                Ok(5_000_000 + i64::from(2_048 * 1_000_000 / sample_rate))
            );
        }
    }

    #[test]
    fn diagnostic_requires_exact_first_sample_pts_and_rate() {
        let stderr = b"[Parsed_ashowinfo_0 @ 0x1] n:0 pts:330750 pts_time:7.5 fmt:s16 channels:2 chlayout:stereo rate:44100 nb_samples:1024\n";
        assert_eq!(validate_diagnostic(stderr, 330_750, 44_100), Ok(()));
        assert_eq!(
            validate_diagnostic(stderr, 330_751, 44_100),
            Err(Error::InvalidTimeline)
        );
        assert_eq!(
            validate_diagnostic(stderr, 330_750, 48_000),
            Err(Error::InvalidTimeline)
        );
    }
}
