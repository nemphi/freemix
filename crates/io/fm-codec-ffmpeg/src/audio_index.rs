use std::collections::VecDeque;
use std::mem::size_of;

use crate::decode::FrameRecord;
use crate::{Error, Limits};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioMetadataIndexTelemetry {
    pub probe_calls: u64,
    pub origin_probe_calls: u64,
    pub resumed_probe_calls: u64,
    pub packet_budget: u64,
    pub peak_packet_budget: usize,
    pub discovered_records: u64,
    pub reused_records: u64,
    pub recomputed_records: u64,
    pub evicted_records: u64,
    pub evicted_checkpoints: u64,
    pub invalidations: u64,
    pub retained_records: usize,
    pub retained_bytes: usize,
    pub retained_checkpoints: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IndexedAudioRecord {
    pub ordinal: usize,
    pub start_sample: usize,
    pub frame: FrameRecord,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioMetadataCheckpoint {
    pub ordinal: usize,
    pub start_sample: usize,
    pub frame: FrameRecord,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioProbePlan {
    pub checkpoint: Option<AudioMetadataCheckpoint>,
    pub packet_budget: usize,
    pub target_ordinal: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct AudioMetadataIndex {
    records: VecDeque<IndexedAudioRecord>,
    checkpoints: VecDeque<AudioMetadataCheckpoint>,
    next_ordinal: usize,
    next_sample: usize,
    end_of_stream: bool,
    limits: Limits,
    telemetry: AudioMetadataIndexTelemetry,
}

impl AudioMetadataIndex {
    pub fn new(limits: Limits) -> Self {
        Self {
            records: VecDeque::new(),
            checkpoints: VecDeque::new(),
            next_ordinal: 0,
            next_sample: 0,
            end_of_stream: false,
            limits,
            telemetry: AudioMetadataIndexTelemetry::default(),
        }
    }

    pub fn telemetry(&self) -> AudioMetadataIndexTelemetry {
        let mut telemetry = self.telemetry;
        telemetry.retained_records = self.records.len();
        telemetry.retained_checkpoints = self.checkpoints.len();
        telemetry.retained_bytes = self.retained_bytes();
        telemetry
    }

    pub fn invalidate(&mut self) {
        self.records.clear();
        self.checkpoints.clear();
        self.next_ordinal = 0;
        self.next_sample = 0;
        self.end_of_stream = false;
        self.telemetry.invalidations = self.telemetry.invalidations.saturating_add(1);
    }

    pub fn contains(&mut self, start: usize, required_end: usize) -> bool {
        let available = required_end <= self.next_ordinal || self.end_of_stream;
        let retained = start == self.next_ordinal
            || self
                .records
                .front()
                .is_some_and(|record| record.ordinal <= start)
                && self
                    .records
                    .back()
                    .is_some_and(|record| record.ordinal >= start);
        if available && retained {
            self.telemetry.reused_records = self.telemetry.reused_records.saturating_add(
                u64::try_from(self.next_ordinal.min(required_end).saturating_sub(start))
                    .unwrap_or(u64::MAX),
            );
        }
        available && retained
    }

    pub fn records_from(&self, start: usize, maximum: usize) -> Vec<IndexedAudioRecord> {
        self.records
            .iter()
            .filter(|record| record.ordinal >= start)
            .take(maximum)
            .cloned()
            .collect()
    }

    pub fn records_through(&self, ordinal: usize, maximum: usize) -> Vec<IndexedAudioRecord> {
        let mut records = self
            .records
            .iter()
            .filter(|record| record.ordinal <= ordinal)
            .rev()
            .take(maximum)
            .cloned()
            .collect::<Vec<_>>();
        records.reverse();
        records
    }

    pub fn sample_at(&self, ordinal: usize) -> Option<usize> {
        if ordinal == self.next_ordinal {
            return Some(self.next_sample);
        }
        self.records
            .iter()
            .find(|record| record.ordinal == ordinal)
            .map(|record| record.start_sample)
    }

    pub const fn end_of_stream(&self) -> bool {
        self.end_of_stream
    }

    pub const fn next_ordinal(&self) -> usize {
        self.next_ordinal
    }

    pub fn probe_plans(
        &self,
        required_end: usize,
        packet_slack: usize,
    ) -> Result<Vec<AudioProbePlan>, Error> {
        let target_ordinal = required_end
            .checked_add(self.limits.audio_metadata_checkpoint_interval)
            .ok_or(Error::InvalidConfig)?;
        let new_records = target_ordinal.saturating_sub(self.next_ordinal);
        if self.next_ordinal == 0 {
            return Ok(vec![AudioProbePlan {
                checkpoint: None,
                packet_budget: target_ordinal
                    .checked_add(packet_slack)
                    .ok_or(Error::InvalidConfig)?,
                target_ordinal,
            }]);
        }
        self.checkpoints
            .iter()
            .rev()
            .take(self.limits.max_audio_metadata_resume_attempts)
            .map(|checkpoint| {
                let overlap = self
                    .next_ordinal
                    .checked_sub(checkpoint.ordinal)
                    .ok_or(Error::InvalidTimeline)?;
                Ok(AudioProbePlan {
                    checkpoint: Some(checkpoint.clone()),
                    packet_budget: overlap
                        .checked_add(new_records)
                        .and_then(|value| value.checked_add(packet_slack))
                        .ok_or(Error::InvalidConfig)?,
                    target_ordinal,
                })
            })
            .collect()
    }

    pub fn note_probe(&mut self, plan: &AudioProbePlan) {
        self.telemetry.probe_calls = self.telemetry.probe_calls.saturating_add(1);
        if plan.checkpoint.is_some() {
            self.telemetry.resumed_probe_calls =
                self.telemetry.resumed_probe_calls.saturating_add(1);
        } else {
            self.telemetry.origin_probe_calls = self.telemetry.origin_probe_calls.saturating_add(1);
        }
        self.telemetry.packet_budget = self
            .telemetry
            .packet_budget
            .saturating_add(u64::try_from(plan.packet_budget).unwrap_or(u64::MAX));
        self.telemetry.peak_packet_budget =
            self.telemetry.peak_packet_budget.max(plan.packet_budget);
    }

    pub fn commit(
        &mut self,
        plan: &AudioProbePlan,
        frames: &[FrameRecord],
        end_of_stream: bool,
    ) -> Result<(), Error> {
        let first = match &plan.checkpoint {
            Some(checkpoint) => frames
                .iter()
                .position(|frame| frame == &checkpoint.frame)
                .ok_or(Error::IncompleteFrameMetadata)?,
            None => 0,
        };
        let first_ordinal = plan
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.ordinal);
        let first_sample = plan
            .checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.start_sample);

        let mut ordinal = first_ordinal;
        let mut sample = first_sample;
        let mut appended = Vec::new();
        let available_end = first_ordinal
            .checked_add(frames.len().saturating_sub(first))
            .ok_or(Error::InvalidTimeline)?;
        for frame in frames.iter().skip(first) {
            if ordinal >= plan.target_ordinal {
                break;
            }
            if let Some(existing) = self.records.iter().find(|record| record.ordinal == ordinal) {
                if existing.start_sample != sample || existing.frame != *frame {
                    return Err(Error::InvalidTimeline);
                }
                self.telemetry.recomputed_records =
                    self.telemetry.recomputed_records.saturating_add(1);
            } else if ordinal < self.next_ordinal {
                return Err(Error::IncompleteFrameMetadata);
            } else {
                appended.push(IndexedAudioRecord {
                    ordinal,
                    start_sample: sample,
                    frame: frame.clone(),
                });
            }
            let samples = frame.sample_count.ok_or(Error::MalformedProbe)?;
            if samples == 0 {
                return Err(Error::InvalidTimeline);
            }
            ordinal = ordinal.checked_add(1).ok_or(Error::InvalidTimeline)?;
            sample = sample.checked_add(samples).ok_or(Error::InvalidTimeline)?;
        }
        if ordinal < self.next_ordinal {
            return Err(Error::IncompleteFrameMetadata);
        }

        for record in appended {
            if record.ordinal != self.next_ordinal || record.start_sample != self.next_sample {
                return Err(Error::InvalidTimeline);
            }
            self.next_ordinal += 1;
            self.next_sample = self
                .next_sample
                .checked_add(record.frame.sample_count.ok_or(Error::MalformedProbe)?)
                .ok_or(Error::InvalidTimeline)?;
            if record.ordinal % self.limits.audio_metadata_checkpoint_interval == 0 {
                self.checkpoints.push_back(AudioMetadataCheckpoint {
                    ordinal: record.ordinal,
                    start_sample: record.start_sample,
                    frame: record.frame.clone(),
                });
            }
            self.records.push_back(record);
            self.telemetry.discovered_records = self.telemetry.discovered_records.saturating_add(1);
        }
        self.end_of_stream = end_of_stream && ordinal == available_end;
        self.evict();
        Ok(())
    }

    fn evict(&mut self) {
        while self.checkpoints.len() > self.limits.max_audio_metadata_checkpoints {
            self.checkpoints.pop_front();
            self.telemetry.evicted_checkpoints =
                self.telemetry.evicted_checkpoints.saturating_add(1);
        }
        while self.records.len() > self.limits.max_audio_metadata_records
            || self.retained_bytes() > self.limits.max_audio_metadata_bytes
        {
            self.records.pop_front();
            self.telemetry.evicted_records = self.telemetry.evicted_records.saturating_add(1);
        }
    }

    fn retained_bytes(&self) -> usize {
        self.records
            .len()
            .saturating_mul(size_of::<IndexedAudioRecord>())
            .saturating_add(
                self.checkpoints
                    .len()
                    .saturating_mul(size_of::<AudioMetadataCheckpoint>()),
            )
    }
}
