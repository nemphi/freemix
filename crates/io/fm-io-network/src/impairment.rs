use core::fmt;

const ONE_MILLION: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImpairmentModel {
    latency_ms: u64,
    jitter_ms: u64,
    loss_ppm: u32,
    reorder_ppm: u32,
    duplicate_ppm: u32,
    bandwidth_limit_bps: Option<u64>,
    disconnected: bool,
    dns_failure: bool,
    tls_failure: bool,
}

impl ImpairmentModel {
    /// Creates a deterministic model for fake adapters and simulation.
    ///
    /// The model only describes outcomes; it never sleeps or performs I/O.
    ///
    /// # Errors
    ///
    /// Rejects probabilities over one million parts per million and a zero bandwidth limit.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        latency_ms: u64,
        jitter_ms: u64,
        loss_ppm: u32,
        reorder_ppm: u32,
        duplicate_ppm: u32,
        bandwidth_limit_bps: Option<u64>,
        disconnected: bool,
        dns_failure: bool,
        tls_failure: bool,
    ) -> Result<Self, ImpairmentError> {
        if loss_ppm > ONE_MILLION || reorder_ppm > ONE_MILLION || duplicate_ppm > ONE_MILLION {
            return Err(ImpairmentError::ProbabilityOutOfRange);
        }
        if matches!(bandwidth_limit_bps, Some(0)) {
            return Err(ImpairmentError::ZeroBandwidth);
        }
        Ok(Self {
            latency_ms,
            jitter_ms,
            loss_ppm,
            reorder_ppm,
            duplicate_ppm,
            bandwidth_limit_bps,
            disconnected,
            dns_failure,
            tls_failure,
        })
    }

    /// Produces a stable decision for a packet sequence without random state.
    #[must_use]
    pub fn evaluate(self, sequence: u64) -> ImpairmentDecision {
        let jitter = if self.jitter_ms == 0 {
            0
        } else {
            let span = self.jitter_ms.saturating_mul(2).saturating_add(1);
            i128::from(mix(sequence) % span).saturating_sub(i128::from(self.jitter_ms))
        };
        let delay = i128::from(self.latency_ms).saturating_add(jitter).max(0);
        ImpairmentDecision {
            delay_ms: u64::try_from(delay).unwrap_or(u64::MAX),
            dropped: selected(sequence, 0x21, self.loss_ppm),
            reordered: selected(sequence, 0x43, self.reorder_ppm),
            duplicated: selected(sequence, 0x65, self.duplicate_ppm),
            bandwidth_limit_bps: self.bandwidth_limit_bps,
            disconnected: self.disconnected,
            dns_failure: self.dns_failure,
            tls_failure: self.tls_failure,
        }
    }
}

fn selected(sequence: u64, salt: u64, probability_ppm: u32) -> bool {
    probability_ppm != 0
        && mix(sequence ^ salt) % u64::from(ONE_MILLION) < u64::from(probability_ppm)
}

const fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ImpairmentDecision {
    pub delay_ms: u64,
    pub dropped: bool,
    pub reordered: bool,
    pub duplicated: bool,
    pub bandwidth_limit_bps: Option<u64>,
    pub disconnected: bool,
    pub dns_failure: bool,
    pub tls_failure: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImpairmentTelemetry {
    evaluated_packets: u64,
    dropped_packets: u64,
    reordered_packets: u64,
    duplicated_packets: u64,
    disconnect_events: u64,
    dns_failures: u64,
    tls_failures: u64,
    total_delay_ms: u128,
    maximum_delay_ms: u64,
    bandwidth_limit_bps: Option<u64>,
}

impl ImpairmentTelemetry {
    pub fn observe(&mut self, decision: ImpairmentDecision) {
        self.evaluated_packets = self.evaluated_packets.saturating_add(1);
        self.dropped_packets = self
            .dropped_packets
            .saturating_add(u64::from(decision.dropped));
        self.reordered_packets = self
            .reordered_packets
            .saturating_add(u64::from(decision.reordered));
        self.duplicated_packets = self
            .duplicated_packets
            .saturating_add(u64::from(decision.duplicated));
        self.disconnect_events = self
            .disconnect_events
            .saturating_add(u64::from(decision.disconnected));
        self.dns_failures = self
            .dns_failures
            .saturating_add(u64::from(decision.dns_failure));
        self.tls_failures = self
            .tls_failures
            .saturating_add(u64::from(decision.tls_failure));
        self.total_delay_ms = self
            .total_delay_ms
            .saturating_add(u128::from(decision.delay_ms));
        self.maximum_delay_ms = self.maximum_delay_ms.max(decision.delay_ms);
        self.bandwidth_limit_bps = decision.bandwidth_limit_bps;
    }

    #[must_use]
    pub const fn evaluated_packets(self) -> u64 {
        self.evaluated_packets
    }

    #[must_use]
    pub const fn dropped_packets(self) -> u64 {
        self.dropped_packets
    }

    #[must_use]
    pub const fn reordered_packets(self) -> u64 {
        self.reordered_packets
    }

    #[must_use]
    pub const fn duplicated_packets(self) -> u64 {
        self.duplicated_packets
    }

    #[must_use]
    pub const fn disconnect_events(self) -> u64 {
        self.disconnect_events
    }

    #[must_use]
    pub const fn dns_failures(self) -> u64 {
        self.dns_failures
    }

    #[must_use]
    pub const fn tls_failures(self) -> u64 {
        self.tls_failures
    }

    #[must_use]
    pub const fn maximum_delay_ms(self) -> u64 {
        self.maximum_delay_ms
    }

    #[must_use]
    pub fn average_delay_ms(self) -> Option<u64> {
        (self.evaluated_packets != 0).then(|| {
            u64::try_from(self.total_delay_ms / u128::from(self.evaluated_packets))
                .unwrap_or(u64::MAX)
        })
    }

    #[must_use]
    pub const fn bandwidth_limit_bps(self) -> Option<u64> {
        self.bandwidth_limit_bps
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImpairmentError {
    ProbabilityOutOfRange,
    ZeroBandwidth,
}

impl fmt::Display for ImpairmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProbabilityOutOfRange => "impairment probability exceeds one million ppm",
            Self::ZeroBandwidth => "impairment bandwidth limit must be nonzero",
        })
    }
}

impl std::error::Error for ImpairmentError {}
