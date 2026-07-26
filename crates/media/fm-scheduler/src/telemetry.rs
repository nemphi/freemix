#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedulerTelemetry {
    pub late_frames: u64,
    pub dropped_frames: u64,
    pub realized_frames: u64,
}

impl SchedulerTelemetry {
    pub(crate) const fn record_late(&mut self) {
        self.late_frames = self.late_frames.saturating_add(1);
    }

    pub(crate) const fn record_dropped(&mut self) {
        self.dropped_frames = self.dropped_frames.saturating_add(1);
    }

    pub(crate) const fn record_realized(&mut self) {
        self.realized_frames = self.realized_frames.saturating_add(1);
    }
}
