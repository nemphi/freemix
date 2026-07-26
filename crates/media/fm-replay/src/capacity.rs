use std::{fs, io, path::PathBuf};

use fm_record::{RecorderId, RecoveryReport};

use crate::{RetentionReport, RollingSegmentCatalog, SegmentId, StorageRootId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacitySample {
    pub free_bytes: u64,
    pub bytes_written: u64,
    pub elapsed_nanos: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityPrediction {
    pub bytes_per_second: u64,
    pub seconds_until_full: Option<u64>,
    pub seconds_until_low_space: Option<u64>,
    pub low_space: bool,
}

impl CapacityPrediction {
    #[must_use]
    pub fn from_sample(sample: CapacitySample, low_space_bytes: u64) -> Self {
        let bytes_per_second = if sample.elapsed_nanos == 0 {
            0
        } else {
            let rate = u128::from(sample.bytes_written).saturating_mul(1_000_000_000)
                / u128::from(sample.elapsed_nanos);
            u64::try_from(rate).unwrap_or(u64::MAX)
        };
        let seconds_until_full =
            (bytes_per_second != 0).then(|| sample.free_bytes / bytes_per_second);
        let seconds_until_low_space = (bytes_per_second != 0)
            .then(|| sample.free_bytes.saturating_sub(low_space_bytes) / bytes_per_second);
        Self {
            bytes_per_second,
            seconds_until_full,
            seconds_until_low_space,
            low_space: sample.free_bytes <= low_space_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LowSpaceAction {
    DeleteOldestUnprotected,
    StopRecording,
    RejectNewWrites,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LowSpacePolicy {
    pub reserve_bytes: u64,
    pub action: LowSpaceAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LowSpaceDecision {
    Continue,
    Reclaimed(RetentionReport),
    ProtectionBlocked(RetentionReport),
    StopRecording,
    RejectNewWrites,
}

impl LowSpacePolicy {
    /// Evaluates an incoming allocation against observed free space.
    ///
    /// Protected/open segments are respected when deletion is configured.
    ///
    /// # Errors
    ///
    /// Catalog reclamation fails when the storage root is unknown.
    pub fn apply(
        self,
        catalog: &mut RollingSegmentCatalog,
        root_id: StorageRootId,
        free_bytes: u64,
        incoming_bytes: u64,
    ) -> Result<LowSpaceDecision, crate::ReplayError> {
        let required = self.reserve_bytes.saturating_add(incoming_bytes);
        if free_bytes >= required {
            return Ok(LowSpaceDecision::Continue);
        }
        Ok(match self.action {
            LowSpaceAction::DeleteOldestUnprotected => {
                let report = catalog.reclaim(root_id, required - free_bytes)?;
                if report.blocked_bytes == 0 {
                    LowSpaceDecision::Reclaimed(report)
                } else {
                    LowSpaceDecision::ProtectionBlocked(report)
                }
            }
            LowSpaceAction::StopRecording => LowSpaceDecision::StopRecording,
            LowSpaceAction::RejectNewWrites => LowSpaceDecision::RejectNewWrites,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplayRecoveryReport {
    pub missing_segments: Vec<SegmentId>,
    pub orphan_files: Vec<PathBuf>,
    pub recorder_reports: Vec<(RecorderId, RecoveryReport)>,
    pub recorder_truncated_bytes: u64,
}

/// Compares catalog metadata with immediate files in each storage root and
/// aggregates the authoritative `fm-record` repair reports.
///
/// # Errors
///
/// Returns an I/O error if a configured root cannot be enumerated.
pub fn inspect_recovery(
    catalog: &RollingSegmentCatalog,
    mut recorder_reports: Vec<(RecorderId, RecoveryReport)>,
) -> io::Result<ReplayRecoveryReport> {
    let mut report = ReplayRecoveryReport::default();
    for segment in catalog.segments() {
        if !segment.path.is_file() {
            report.missing_segments.push(segment.id);
        }
    }
    for root in catalog.roots() {
        let known = catalog.known_paths(root.id);
        let mut files = fs::read_dir(&root.path)?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_type().ok()?.is_file().then(|| entry.path()))
            .filter(|path| !known.contains(path.as_path()))
            .collect::<Vec<_>>();
        files.sort();
        report.orphan_files.extend(files);
    }
    report.missing_segments.sort_unstable();
    report.orphan_files.sort();
    recorder_reports.sort_by_key(|(id, _)| *id);
    report.recorder_truncated_bytes = recorder_reports.iter().fold(0_u64, |total, (_, item)| {
        let segment_bytes = item.segments.iter().fold(0_u64, |sum, segment| {
            sum.saturating_add(segment.truncated_bytes)
        });
        total
            .saturating_add(item.manifest_truncated_bytes)
            .saturating_add(segment_bytes)
    });
    report.recorder_reports = recorder_reports;
    Ok(report)
}
