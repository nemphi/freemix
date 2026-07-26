use std::num::NonZeroU32;

/// A drawable presentation extent with non-zero dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationExtent {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl PresentationExtent {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        match (NonZeroU32::new(width), NonZeroU32::new(height)) {
            (Some(width), Some(height)) => Some(Self { width, height }),
            _ => None,
        }
    }

    #[must_use]
    pub const fn width(self) -> NonZeroU32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> NonZeroU32 {
        self.height
    }
}

/// Strictly ordered generation supplied with each frame.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FrameGeneration(u64);

impl FrameGeneration {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Generation of the currently requested presentation extent.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ResizeGeneration(u64);

impl ResizeGeneration {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A frame tagged for one exact presentation extent generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationFrame {
    frame_generation: FrameGeneration,
    resize_generation: ResizeGeneration,
}

impl PresentationFrame {
    #[must_use]
    pub const fn new(
        frame_generation: FrameGeneration,
        resize_generation: ResizeGeneration,
    ) -> Self {
        Self {
            frame_generation,
            resize_generation,
        }
    }

    #[must_use]
    pub const fn frame_generation(self) -> FrameGeneration {
        self.frame_generation
    }

    #[must_use]
    pub const fn resize_generation(self) -> ResizeGeneration {
        self.resize_generation
    }
}

/// Backend-neutral result of trying to acquire a presentable surface image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceAcquisition {
    Success,
    Suboptimal,
    Timeout,
    Outdated,
    Lost,
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationFailure {
    ConfigurationFailed,
    OutOfMemory,
    RecreationFailed,
    ResizeGenerationExhausted,
}

/// Current portable presentation state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PresentationState {
    #[default]
    Unconfigured,
    Configuring {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Configured {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Suspended {
        resize_generation: ResizeGeneration,
    },
    Recreate {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Failed(PresentationFailure),
}

impl PresentationState {
    #[must_use]
    pub const fn resize_generation(self) -> Option<ResizeGeneration> {
        match self {
            Self::Unconfigured | Self::Failed(_) => None,
            Self::Configuring {
                resize_generation, ..
            }
            | Self::Configured {
                resize_generation, ..
            }
            | Self::Suspended { resize_generation }
            | Self::Recreate {
                resize_generation, ..
            } => Some(resize_generation),
        }
    }
}

/// Operation for a backend integration to perform next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationAction {
    None,
    /// An acquisition callback arrived while a prior present was outstanding.
    /// A successful acquired image must be discarded by the backend.
    RejectAcquisition(SurfaceAcquisition),
    Configure {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Reconfigure {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Suspend {
        resize_generation: ResizeGeneration,
    },
    Present(PresentationFrame),
    PresentAndReconfigure {
        frame: PresentationFrame,
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Retry,
    Recreate {
        extent: PresentationExtent,
        resize_generation: ResizeGeneration,
    },
    Fail(PresentationFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameRejection {
    NotConfigured,
    Failed(PresentationFailure),
    StaleResizeGeneration {
        expected: ResizeGeneration,
        actual: ResizeGeneration,
    },
    NonMonotonicGeneration {
        previous: FrameGeneration,
        actual: FrameGeneration,
    },
}

/// Result of offering a frame to the one-slot latest-frame queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameDecision {
    Queued,
    Replaced { dropped: PresentationFrame },
    Rejected(FrameRejection),
}

/// Saturating presentation counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentationTelemetry {
    pub pending_depth: u64,
    pub peak_pending_depth: u64,
    pub frames_accepted: u64,
    pub frames_presented: u64,
    pub frames_replaced: u64,
    pub frames_dropped: u64,
    pub stale_frames: u64,
    pub successful_acquisitions: u64,
    pub timeouts: u64,
    pub suboptimal_acquisitions: u64,
    pub outdated_acquisitions: u64,
    pub surface_losses: u64,
    pub recreation_attempts: u64,
    pub recreation_failures: u64,
    pub out_of_memory_failures: u64,
}

impl PresentationTelemetry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending_depth: 0,
            peak_pending_depth: 0,
            frames_accepted: 0,
            frames_presented: 0,
            frames_replaced: 0,
            frames_dropped: 0,
            stale_frames: 0,
            successful_acquisitions: 0,
            timeouts: 0,
            suboptimal_acquisitions: 0,
            outdated_acquisitions: 0,
            surface_losses: 0,
            recreation_attempts: 0,
            recreation_failures: 0,
            out_of_memory_failures: 0,
        }
    }
}

/// Deterministic presentation lifecycle and latest-frame policy.
#[derive(Clone, Debug)]
pub struct PresentationLifecycle {
    state: PresentationState,
    pending_frame: Option<PresentationFrame>,
    presenting_frame: Option<PresentationFrame>,
    last_frame_generation: Option<FrameGeneration>,
    configuration_kind: Option<ConfigurationKind>,
    recreation_required: bool,
    telemetry: PresentationTelemetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigurationKind {
    Configure,
    Reconfigure,
}

impl PresentationLifecycle {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: PresentationState::Unconfigured,
            pending_frame: None,
            presenting_frame: None,
            last_frame_generation: None,
            configuration_kind: None,
            recreation_required: false,
            telemetry: PresentationTelemetry::new(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> PresentationState {
        self.state
    }

    #[must_use]
    pub const fn resize_generation(&self) -> Option<ResizeGeneration> {
        self.state.resize_generation()
    }

    #[must_use]
    pub const fn pending_frame(&self) -> Option<PresentationFrame> {
        self.pending_frame
    }

    /// Returns the frame awaiting a backend presentation acknowledgement.
    #[must_use]
    pub const fn presenting_frame(&self) -> Option<PresentationFrame> {
        self.presenting_frame
    }

    #[must_use]
    pub fn telemetry(&self) -> PresentationTelemetry {
        PresentationTelemetry {
            pending_depth: u64::from(self.pending_frame.is_some()),
            ..self.telemetry
        }
    }

    /// Applies a drawable-size change. A zero dimension suspends presentation.
    pub fn resize(&mut self, width: u32, height: u32) -> PresentationAction {
        if let PresentationState::Failed(reason) = self.state {
            return PresentationAction::Fail(reason);
        }

        let Some(generation) = self.next_resize_generation(width, height) else {
            return match self.state {
                PresentationState::Failed(reason) => PresentationAction::Fail(reason),
                _ => PresentationAction::None,
            };
        };
        let Some(extent) = PresentationExtent::new(width, height) else {
            self.recreation_required |= matches!(self.state, PresentationState::Recreate { .. });
            self.abandon_present();
            self.drop_pending_frame();
            self.configuration_kind = None;
            self.state = PresentationState::Suspended {
                resize_generation: generation,
            };
            return PresentationAction::Suspend {
                resize_generation: generation,
            };
        };

        if matches!(self.state, PresentationState::Recreate { .. })
            || matches!(self.state, PresentationState::Suspended { .. }) && self.recreation_required
        {
            self.abandon_present();
            self.drop_pending_frame();
            self.configuration_kind = None;
            self.recreation_required = true;
            self.state = PresentationState::Recreate {
                extent,
                resize_generation: generation,
            };
            return PresentationAction::Recreate {
                extent,
                resize_generation: generation,
            };
        }

        let configuration_kind = match self.state {
            PresentationState::Unconfigured | PresentationState::Suspended { .. } => {
                Some(ConfigurationKind::Configure)
            }
            PresentationState::Configuring { .. } => self.configuration_kind,
            PresentationState::Configured { .. } => Some(ConfigurationKind::Reconfigure),
            PresentationState::Recreate { .. } => {
                self.recreation_required = true;
                self.state = PresentationState::Recreate {
                    extent,
                    resize_generation: generation,
                };
                return PresentationAction::Recreate {
                    extent,
                    resize_generation: generation,
                };
            }
            PresentationState::Failed(reason) => return PresentationAction::Fail(reason),
        };
        self.abandon_present();
        self.drop_pending_frame();
        self.recreation_required = false;
        self.state = PresentationState::Configuring {
            extent,
            resize_generation: generation,
        };
        self.configuration_kind = configuration_kind;
        configuration_kind.map_or(PresentationAction::None, |kind| {
            configuration_action(kind, extent, generation)
        })
    }

    /// Queues a newer frame, replacing any frame not yet presented.
    pub fn submit_frame(&mut self, frame: PresentationFrame) -> FrameDecision {
        let expected_resize = match self.state {
            PresentationState::Configuring {
                resize_generation, ..
            }
            | PresentationState::Configured {
                resize_generation, ..
            }
            | PresentationState::Recreate {
                resize_generation, ..
            } => resize_generation,
            PresentationState::Failed(reason) => {
                self.record_rejected_frame(false);
                return FrameDecision::Rejected(FrameRejection::Failed(reason));
            }
            _ => {
                self.record_rejected_frame(false);
                return FrameDecision::Rejected(FrameRejection::NotConfigured);
            }
        };

        if frame.resize_generation != expected_resize {
            self.record_rejected_frame(true);
            return FrameDecision::Rejected(FrameRejection::StaleResizeGeneration {
                expected: expected_resize,
                actual: frame.resize_generation,
            });
        }
        if let Some(previous) = self.last_frame_generation
            && frame.frame_generation <= previous
        {
            self.record_rejected_frame(true);
            return FrameDecision::Rejected(FrameRejection::NonMonotonicGeneration {
                previous,
                actual: frame.frame_generation,
            });
        }

        self.last_frame_generation = Some(frame.frame_generation);
        increment(&mut self.telemetry.frames_accepted);
        self.telemetry.peak_pending_depth = self.telemetry.peak_pending_depth.max(1);
        if let Some(dropped) = self.pending_frame.replace(frame) {
            increment(&mut self.telemetry.frames_replaced);
            increment(&mut self.telemetry.frames_dropped);
            FrameDecision::Replaced { dropped }
        } else {
            FrameDecision::Queued
        }
    }

    /// Maps a backend acquisition result to a portable lifecycle action.
    ///
    /// Non-fatal callbacks received while a present is outstanding return
    /// [`PresentationAction::RejectAcquisition`]. The backend must discard an
    /// image returned by a successful rejected acquisition. Surface loss and
    /// out-of-memory outcomes remain immediately actionable.
    pub fn handle_acquisition(&mut self, outcome: SurfaceAcquisition) -> PresentationAction {
        let (extent, resize_generation) = match self.state {
            PresentationState::Configured {
                extent,
                resize_generation,
            } => (extent, resize_generation),
            PresentationState::Failed(reason) => return PresentationAction::Fail(reason),
            _ => return PresentationAction::None,
        };

        if self.presenting_frame.is_some() {
            match outcome {
                SurfaceAcquisition::Success => {
                    increment(&mut self.telemetry.successful_acquisitions);
                }
                SurfaceAcquisition::Suboptimal => {
                    increment(&mut self.telemetry.successful_acquisitions);
                    increment(&mut self.telemetry.suboptimal_acquisitions);
                }
                SurfaceAcquisition::Timeout => increment(&mut self.telemetry.timeouts),
                SurfaceAcquisition::Outdated => {
                    increment(&mut self.telemetry.outdated_acquisitions);
                }
                SurfaceAcquisition::Lost | SurfaceAcquisition::OutOfMemory => {}
            }
            if !matches!(
                outcome,
                SurfaceAcquisition::Lost | SurfaceAcquisition::OutOfMemory
            ) {
                return PresentationAction::RejectAcquisition(outcome);
            }
        }

        match outcome {
            SurfaceAcquisition::Success => {
                increment(&mut self.telemetry.successful_acquisitions);
                self.begin_present()
                    .map_or(PresentationAction::None, PresentationAction::Present)
            }
            SurfaceAcquisition::Suboptimal => {
                increment(&mut self.telemetry.successful_acquisitions);
                increment(&mut self.telemetry.suboptimal_acquisitions);
                self.state = PresentationState::Configuring {
                    extent,
                    resize_generation,
                };
                self.configuration_kind = Some(ConfigurationKind::Reconfigure);
                self.begin_present().map_or(
                    PresentationAction::Reconfigure {
                        extent,
                        resize_generation,
                    },
                    |frame| PresentationAction::PresentAndReconfigure {
                        frame,
                        extent,
                        resize_generation,
                    },
                )
            }
            SurfaceAcquisition::Timeout => {
                increment(&mut self.telemetry.timeouts);
                PresentationAction::Retry
            }
            SurfaceAcquisition::Outdated => {
                increment(&mut self.telemetry.outdated_acquisitions);
                self.state = PresentationState::Configuring {
                    extent,
                    resize_generation,
                };
                self.configuration_kind = Some(ConfigurationKind::Reconfigure);
                PresentationAction::Reconfigure {
                    extent,
                    resize_generation,
                }
            }
            SurfaceAcquisition::Lost => {
                increment(&mut self.telemetry.surface_losses);
                self.abandon_present();
                self.configuration_kind = None;
                self.recreation_required = true;
                self.state = PresentationState::Recreate {
                    extent,
                    resize_generation,
                };
                PresentationAction::Recreate {
                    extent,
                    resize_generation,
                }
            }
            SurfaceAcquisition::OutOfMemory => {
                increment(&mut self.telemetry.out_of_memory_failures);
                self.fail(PresentationFailure::OutOfMemory)
            }
        }
    }

    /// Acknowledges completion of the configuration action for `resize_generation`.
    pub fn finish_configuration(
        &mut self,
        resize_generation: ResizeGeneration,
        succeeded: bool,
    ) -> PresentationAction {
        let (extent, expected_generation) = match self.state {
            PresentationState::Configuring {
                extent,
                resize_generation,
            } => (extent, resize_generation),
            PresentationState::Failed(reason) => return PresentationAction::Fail(reason),
            _ => return PresentationAction::None,
        };
        if resize_generation != expected_generation {
            return PresentationAction::None;
        }

        self.configuration_kind = None;
        if succeeded {
            self.state = PresentationState::Configured {
                extent,
                resize_generation,
            };
            PresentationAction::None
        } else {
            self.fail(PresentationFailure::ConfigurationFailed)
        }
    }

    /// Acknowledges whether the backend completed one emitted present action.
    pub fn finish_present(
        &mut self,
        frame: PresentationFrame,
        succeeded: bool,
    ) -> PresentationAction {
        if self.presenting_frame != Some(frame) {
            return match self.state {
                PresentationState::Failed(reason) => PresentationAction::Fail(reason),
                _ => PresentationAction::None,
            };
        }

        self.presenting_frame = None;
        if succeeded {
            increment(&mut self.telemetry.frames_presented);
        } else {
            increment(&mut self.telemetry.frames_dropped);
        }
        PresentationAction::None
    }

    /// Completes the recreation action for `resize_generation`.
    ///
    /// A stale generation is ignored without changing state or telemetry.
    pub fn finish_recreation(
        &mut self,
        resize_generation: ResizeGeneration,
        succeeded: bool,
    ) -> PresentationAction {
        let (extent, expected_generation) = match self.state {
            PresentationState::Recreate {
                extent,
                resize_generation,
            } => (extent, resize_generation),
            PresentationState::Failed(reason) => return PresentationAction::Fail(reason),
            _ => return PresentationAction::None,
        };
        if resize_generation != expected_generation {
            return PresentationAction::None;
        }

        increment(&mut self.telemetry.recreation_attempts);
        if succeeded {
            self.recreation_required = false;
            self.state = PresentationState::Configuring {
                extent,
                resize_generation,
            };
            self.configuration_kind = Some(ConfigurationKind::Configure);
            PresentationAction::Configure {
                extent,
                resize_generation,
            }
        } else {
            increment(&mut self.telemetry.recreation_failures);
            self.fail(PresentationFailure::RecreationFailed)
        }
    }

    fn next_resize_generation(&mut self, width: u32, height: u32) -> Option<ResizeGeneration> {
        let requested = PresentationExtent::new(width, height);
        let unchanged = match (self.state, requested) {
            (PresentationState::Suspended { .. }, None) => true,
            (
                PresentationState::Configured { extent, .. }
                | PresentationState::Configuring { extent, .. }
                | PresentationState::Recreate { extent, .. },
                Some(requested_extent),
            ) => extent == requested_extent,
            _ => false,
        };
        if unchanged {
            return None;
        }

        let current = self
            .state
            .resize_generation()
            .unwrap_or(ResizeGeneration::ZERO);
        let Some(next) = current.get().checked_add(1) else {
            self.fail(PresentationFailure::ResizeGenerationExhausted);
            return None;
        };
        Some(ResizeGeneration::new(next))
    }

    fn begin_present(&mut self) -> Option<PresentationFrame> {
        if self.presenting_frame.is_some() {
            return None;
        }
        let frame = self.pending_frame.take()?;
        self.presenting_frame = Some(frame);
        Some(frame)
    }

    fn abandon_present(&mut self) {
        if self.presenting_frame.take().is_some() {
            increment(&mut self.telemetry.frames_dropped);
        }
    }

    fn drop_pending_frame(&mut self) {
        if self.pending_frame.take().is_some() {
            increment(&mut self.telemetry.frames_dropped);
        }
    }

    fn record_rejected_frame(&mut self, stale: bool) {
        increment(&mut self.telemetry.frames_dropped);
        if stale {
            increment(&mut self.telemetry.stale_frames);
        }
    }

    fn fail(&mut self, reason: PresentationFailure) -> PresentationAction {
        self.abandon_present();
        self.drop_pending_frame();
        self.configuration_kind = None;
        self.recreation_required = false;
        self.state = PresentationState::Failed(reason);
        PresentationAction::Fail(reason)
    }
}

impl Default for PresentationLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

const fn increment(value: &mut u64) {
    *value = value.saturating_add(1);
}

const fn configuration_action(
    kind: ConfigurationKind,
    extent: PresentationExtent,
    resize_generation: ResizeGeneration,
) -> PresentationAction {
    match kind {
        ConfigurationKind::Configure => PresentationAction::Configure {
            extent,
            resize_generation,
        },
        ConfigurationKind::Reconfigure => PresentationAction::Reconfigure {
            extent,
            resize_generation,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(width: u32, height: u32) -> PresentationExtent {
        PresentationExtent::new(width, height).unwrap()
    }

    fn configured() -> PresentationLifecycle {
        let mut lifecycle = PresentationLifecycle::new();
        assert_eq!(
            lifecycle.resize(1_920, 1_080),
            PresentationAction::Configure {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert_eq!(
            lifecycle.state(),
            PresentationState::Configuring {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert_eq!(
            lifecycle.finish_configuration(ResizeGeneration::new(1), true),
            PresentationAction::None
        );
        lifecycle
    }

    fn frame(number: u64, resize: u64) -> PresentationFrame {
        PresentationFrame::new(FrameGeneration::new(number), ResizeGeneration::new(resize))
    }

    #[test]
    fn zero_extent_suspends_and_drops_pending_frame() {
        let mut lifecycle = configured();
        assert_eq!(lifecycle.submit_frame(frame(1, 1)), FrameDecision::Queued);

        assert_eq!(
            lifecycle.resize(0, 1_080),
            PresentationAction::Suspend {
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(
            lifecycle.state(),
            PresentationState::Suspended {
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(lifecycle.telemetry().frames_dropped, 1);
    }

    #[test]
    fn configures_then_reconfigures_for_new_nonzero_extent() {
        let mut lifecycle = PresentationLifecycle::new();
        assert_eq!(
            lifecycle.resize(640, 480),
            PresentationAction::Configure {
                extent: extent(640, 480),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));
        assert_eq!(
            lifecycle.finish_configuration(ResizeGeneration::new(1), true),
            PresentationAction::None
        );
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configured { .. }
        ));
        assert_eq!(
            lifecycle.resize(1_280, 720),
            PresentationAction::Reconfigure {
                extent: extent(1_280, 720),
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));
        assert_eq!(
            lifecycle.finish_configuration(ResizeGeneration::new(1), true),
            PresentationAction::None
        );
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));
        assert_eq!(lifecycle.resize(1_280, 720), PresentationAction::None);
        lifecycle.finish_configuration(ResizeGeneration::new(2), true);
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configured { .. }
        ));
    }

    #[test]
    fn configuration_failure_is_typed_and_drops_the_queued_frame() {
        let mut lifecycle = PresentationLifecycle::new();
        lifecycle.resize(640, 480);
        assert_eq!(lifecycle.submit_frame(frame(1, 1)), FrameDecision::Queued);

        assert_eq!(
            lifecycle.finish_configuration(ResizeGeneration::new(1), false),
            PresentationAction::Fail(PresentationFailure::ConfigurationFailed)
        );
        assert_eq!(
            lifecycle.state(),
            PresentationState::Failed(PresentationFailure::ConfigurationFailed)
        );
        assert_eq!(lifecycle.pending_frame(), None);
        assert_eq!(lifecycle.telemetry().frames_dropped, 1);
    }

    #[test]
    fn successful_acquisition_presents_latest_frame() {
        let mut lifecycle = configured();
        let latest = frame(1, 1);
        assert_eq!(lifecycle.submit_frame(latest), FrameDecision::Queued);
        assert_eq!(lifecycle.telemetry().pending_depth, 1);
        assert_eq!(lifecycle.telemetry().peak_pending_depth, 1);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Success),
            PresentationAction::Present(latest)
        );
        assert_eq!(lifecycle.pending_frame(), None);
        assert_eq!(lifecycle.telemetry().pending_depth, 0);
        assert_eq!(lifecycle.telemetry().peak_pending_depth, 1);
        assert_eq!(lifecycle.presenting_frame(), Some(latest));
        assert_eq!(lifecycle.telemetry().frames_presented, 0);
        assert_eq!(lifecycle.telemetry().successful_acquisitions, 1);
        assert_eq!(
            lifecycle.finish_present(latest, true),
            PresentationAction::None
        );
        assert_eq!(lifecycle.presenting_frame(), None);
        assert_eq!(lifecycle.telemetry().frames_presented, 1);
    }

    #[test]
    fn timeout_retries_without_consuming_latest_frame() {
        let mut lifecycle = configured();
        let latest = frame(1, 1);
        lifecycle.submit_frame(latest);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Timeout),
            PresentationAction::Retry
        );
        assert_eq!(lifecycle.pending_frame(), Some(latest));
        assert_eq!(lifecycle.telemetry().timeouts, 1);
    }

    #[test]
    fn acquisition_while_presenting_is_explicitly_rejected() {
        let mut lifecycle = configured();
        let presenting = frame(1, 1);
        let pending = frame(2, 1);
        lifecycle.submit_frame(presenting);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Success),
            PresentationAction::Present(presenting)
        );
        lifecycle.submit_frame(pending);

        for outcome in [
            SurfaceAcquisition::Success,
            SurfaceAcquisition::Suboptimal,
            SurfaceAcquisition::Timeout,
            SurfaceAcquisition::Outdated,
        ] {
            assert_eq!(
                lifecycle.handle_acquisition(outcome),
                PresentationAction::RejectAcquisition(outcome)
            );
        }
        assert_eq!(lifecycle.presenting_frame(), Some(presenting));
        assert_eq!(lifecycle.pending_frame(), Some(pending));
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configured { .. }
        ));

        lifecycle.finish_present(presenting, true);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Success),
            PresentationAction::Present(pending)
        );
    }

    #[test]
    fn suboptimal_presents_and_requests_reconfiguration() {
        let mut lifecycle = configured();
        let latest = frame(1, 1);
        lifecycle.submit_frame(latest);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Suboptimal),
            PresentationAction::PresentAndReconfigure {
                frame: latest,
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert_eq!(lifecycle.telemetry().suboptimal_acquisitions, 1);
        assert_eq!(lifecycle.telemetry().frames_presented, 0);
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));

        let newer = frame(2, 1);
        assert_eq!(lifecycle.submit_frame(newer), FrameDecision::Queued);
        lifecycle.finish_configuration(ResizeGeneration::new(1), true);
        lifecycle.finish_present(latest, true);
        assert_eq!(lifecycle.telemetry().frames_presented, 1);
        assert_eq!(lifecycle.pending_frame(), Some(newer));
    }

    #[test]
    fn outdated_requests_reconfiguration_without_consuming_frame() {
        let mut lifecycle = configured();
        let latest = frame(1, 1);
        lifecycle.submit_frame(latest);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Outdated),
            PresentationAction::Reconfigure {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert_eq!(lifecycle.pending_frame(), Some(latest));
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));
        let newer = frame(2, 1);
        assert_eq!(
            lifecycle.submit_frame(newer),
            FrameDecision::Replaced { dropped: latest }
        );
        assert_eq!(lifecycle.telemetry().outdated_acquisitions, 1);
    }

    #[test]
    fn lost_surface_gets_one_recreation_attempt() {
        let mut lifecycle = configured();
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Lost),
            PresentationAction::Recreate {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(1)
            }
        );
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Recreate { .. }
        ));
        assert_eq!(lifecycle.telemetry().recreation_attempts, 0);
        assert!(matches!(
            lifecycle.finish_recreation(ResizeGeneration::new(1), true),
            PresentationAction::Configure { .. }
        ));
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configuring { .. }
        ));
        assert_eq!(lifecycle.telemetry().recreation_attempts, 1);
        lifecycle.finish_configuration(ResizeGeneration::new(1), true);
        assert!(matches!(
            lifecycle.state(),
            PresentationState::Configured { .. }
        ));
    }

    #[test]
    fn lost_surface_recreation_survives_zero_extent_suspension() {
        let mut lifecycle = configured();
        lifecycle.handle_acquisition(SurfaceAcquisition::Lost);

        assert_eq!(
            lifecycle.resize(0, 1_080),
            PresentationAction::Suspend {
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(1), true),
            PresentationAction::None
        );
        assert_eq!(
            lifecycle.resize(1_920, 1_080),
            PresentationAction::Recreate {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(3)
            }
        );
        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(3), true),
            PresentationAction::Configure {
                extent: extent(1_920, 1_080),
                resize_generation: ResizeGeneration::new(3)
            }
        );
    }

    #[test]
    fn stale_recreation_completion_after_resize_is_ignored() {
        let mut lifecycle = configured();
        lifecycle.handle_acquisition(SurfaceAcquisition::Lost);

        assert_eq!(
            lifecycle.resize(1_280, 720),
            PresentationAction::Recreate {
                extent: extent(1_280, 720),
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(1), false),
            PresentationAction::None
        );
        assert_eq!(
            lifecycle.state(),
            PresentationState::Recreate {
                extent: extent(1_280, 720),
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(lifecycle.telemetry().recreation_attempts, 0);
        assert_eq!(lifecycle.telemetry().recreation_failures, 0);

        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(2), true),
            PresentationAction::Configure {
                extent: extent(1_280, 720),
                resize_generation: ResizeGeneration::new(2)
            }
        );
        assert_eq!(lifecycle.telemetry().recreation_attempts, 1);
    }

    #[test]
    fn configuring_and_recreating_keep_only_the_newest_current_frame() {
        let mut lifecycle = PresentationLifecycle::new();
        lifecycle.resize(640, 480);
        let first = frame(1, 1);
        let second = frame(2, 1);
        assert_eq!(lifecycle.submit_frame(first), FrameDecision::Queued);
        assert_eq!(
            lifecycle.submit_frame(second),
            FrameDecision::Replaced { dropped: first }
        );
        lifecycle.finish_configuration(ResizeGeneration::new(1), true);

        lifecycle.handle_acquisition(SurfaceAcquisition::Lost);
        let third = frame(3, 1);
        assert_eq!(
            lifecycle.submit_frame(third),
            FrameDecision::Replaced { dropped: second }
        );
        lifecycle.finish_recreation(ResizeGeneration::new(1), true);
        let fourth = frame(4, 1);
        assert_eq!(
            lifecycle.submit_frame(fourth),
            FrameDecision::Replaced { dropped: third }
        );
        assert_eq!(lifecycle.pending_frame(), Some(fourth));
        assert_eq!(lifecycle.telemetry().frames_replaced, 3);
        assert_eq!(lifecycle.telemetry().frames_dropped, 3);
    }

    #[test]
    fn recreation_failure_is_sticky_and_not_counted_twice() {
        let mut lifecycle = configured();
        lifecycle.handle_acquisition(SurfaceAcquisition::Lost);
        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(1), false),
            PresentationAction::Fail(PresentationFailure::RecreationFailed)
        );
        assert_eq!(
            lifecycle.finish_recreation(ResizeGeneration::new(1), false),
            PresentationAction::Fail(PresentationFailure::RecreationFailed)
        );
        assert_eq!(
            lifecycle.resize(640, 480),
            PresentationAction::Fail(PresentationFailure::RecreationFailed)
        );
        assert_eq!(
            lifecycle.state(),
            PresentationState::Failed(PresentationFailure::RecreationFailed)
        );
        assert_eq!(lifecycle.telemetry().recreation_attempts, 1);
        assert_eq!(lifecycle.telemetry().recreation_failures, 1);
    }

    #[test]
    fn failed_and_abandoned_presents_are_dropped() {
        let mut lifecycle = configured();
        let failed = frame(1, 1);
        lifecycle.submit_frame(failed);
        lifecycle.handle_acquisition(SurfaceAcquisition::Success);
        lifecycle.finish_present(failed, false);
        assert_eq!(lifecycle.telemetry().frames_presented, 0);
        assert_eq!(lifecycle.telemetry().frames_dropped, 1);

        let abandoned = frame(2, 1);
        lifecycle.submit_frame(abandoned);
        lifecycle.handle_acquisition(SurfaceAcquisition::Success);
        lifecycle.resize(1_280, 720);
        assert_eq!(lifecycle.presenting_frame(), None);
        assert_eq!(lifecycle.telemetry().frames_dropped, 2);

        lifecycle.finish_present(abandoned, true);
        assert_eq!(lifecycle.telemetry().frames_presented, 0);
        assert_eq!(lifecycle.telemetry().frames_dropped, 2);
    }

    #[test]
    fn latest_frame_replaces_and_accounts_for_dropped_frame() {
        let mut lifecycle = configured();
        let first = frame(1, 1);
        let latest = frame(2, 1);
        assert_eq!(lifecycle.submit_frame(first), FrameDecision::Queued);
        assert_eq!(
            lifecycle.submit_frame(latest),
            FrameDecision::Replaced { dropped: first }
        );
        assert_eq!(lifecycle.pending_frame(), Some(latest));
        assert_eq!(lifecycle.telemetry().frames_accepted, 2);
        assert_eq!(lifecycle.telemetry().frames_replaced, 1);
        assert_eq!(lifecycle.telemetry().frames_dropped, 1);
    }

    #[test]
    fn rejects_stale_resize_and_non_monotonic_frame_generations() {
        let mut lifecycle = configured();
        assert_eq!(lifecycle.submit_frame(frame(10, 1)), FrameDecision::Queued);
        assert!(matches!(
            lifecycle.submit_frame(frame(10, 1)),
            FrameDecision::Rejected(FrameRejection::NonMonotonicGeneration { .. })
        ));
        lifecycle.resize(1_280, 720);
        assert_eq!(
            lifecycle.submit_frame(frame(11, 1)),
            FrameDecision::Rejected(FrameRejection::StaleResizeGeneration {
                expected: ResizeGeneration::new(2),
                actual: ResizeGeneration::new(1)
            })
        );
        assert_eq!(lifecycle.telemetry().stale_frames, 2);
        assert_eq!(lifecycle.telemetry().frames_dropped, 3);
    }

    #[test]
    fn out_of_memory_is_immediately_sticky() {
        let mut lifecycle = configured();
        lifecycle.submit_frame(frame(1, 1));
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::OutOfMemory),
            PresentationAction::Fail(PresentationFailure::OutOfMemory)
        );
        assert_eq!(lifecycle.telemetry().out_of_memory_failures, 1);
        assert_eq!(lifecycle.telemetry().frames_dropped, 1);
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Success),
            PresentationAction::Fail(PresentationFailure::OutOfMemory)
        );
    }

    #[test]
    fn telemetry_counters_saturate() {
        let mut lifecycle = configured();
        lifecycle.telemetry.timeouts = u64::MAX;
        lifecycle.telemetry.frames_dropped = u64::MAX;
        lifecycle.handle_acquisition(SurfaceAcquisition::Timeout);
        lifecycle.submit_frame(frame(1, 0));
        assert_eq!(lifecycle.telemetry().timeouts, u64::MAX);
        assert_eq!(lifecycle.telemetry().frames_dropped, u64::MAX);

        let presented = frame(2, 1);
        lifecycle.submit_frame(presented);
        lifecycle.handle_acquisition(SurfaceAcquisition::Success);
        lifecycle.telemetry.frames_presented = u64::MAX;
        lifecycle.finish_present(presented, true);
        assert_eq!(lifecycle.telemetry().frames_presented, u64::MAX);
    }
}
