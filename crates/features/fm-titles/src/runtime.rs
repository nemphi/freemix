#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockDirection {
    CountUp,
    CountDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockFormat {
    MinutesSeconds,
    HoursMinutesSeconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSpec {
    pub direction: ClockDirection,
    pub start_value_ms: u64,
    pub starts_at_ms: u64,
    pub format: ClockFormat,
}

impl ClockSpec {
    #[must_use]
    pub fn value_ms(self, scene_time_ms: u64) -> u64 {
        let elapsed = scene_time_ms.saturating_sub(self.starts_at_ms);
        match self.direction {
            ClockDirection::CountUp => self.start_value_ms.saturating_add(elapsed),
            ClockDirection::CountDown => self.start_value_ms.saturating_sub(elapsed),
        }
    }
}

/// Evaluates a clock at scene time. Display values use whole seconds.
#[must_use]
pub fn evaluate_clock(spec: ClockSpec, scene_time_ms: u64) -> String {
    let total_seconds = spec.value_ms(scene_time_ms) / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    match spec.format {
        ClockFormat::MinutesSeconds => format!("{total_minutes:02}:{seconds:02}"),
        ClockFormat::HoursMinutesSeconds => {
            let hours = total_minutes / 60;
            let minutes = total_minutes % 60;
            format!("{hours:02}:{minutes:02}:{seconds:02}")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickerDirection {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickerSpec {
    pub field: crate::FieldId,
    pub direction: TickerDirection,
    pub pixels_per_second: u32,
    pub gap_px: u32,
    pub starts_at_ms: u64,
}

/// Returns the ticker's x position relative to its viewport.
///
/// A left-moving item starts at the right edge and wraps after it has cleared
/// the left edge plus its gap. Right-moving behavior is the mirror image.
#[must_use]
pub fn evaluate_ticker_position(
    spec: TickerSpec,
    scene_time_ms: u64,
    viewport_width: u32,
    content_width: u32,
) -> i64 {
    let cycle = u64::from(viewport_width)
        .saturating_add(u64::from(content_width))
        .saturating_add(u64::from(spec.gap_px))
        .max(1);
    let elapsed = scene_time_ms.saturating_sub(spec.starts_at_ms);
    let distance = elapsed.saturating_mul(u64::from(spec.pixels_per_second)) / 1_000 % cycle;
    match spec.direction {
        TickerDirection::Left => {
            i64::from(viewport_width) - i64::try_from(distance).unwrap_or(i64::MAX)
        }
        TickerDirection::Right => {
            -i64::from(content_width) + i64::try_from(distance).unwrap_or(i64::MAX)
        }
    }
}
