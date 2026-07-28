use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Instant;

use fm_frame::{
    AlphaMode, AudioBlock, Channel, ChannelLayout, ChromaLocation, ClockDomainId, ColorMetadata,
    ColorPrimaries, CpuVideoFrame, CpuVideoPayload, CpuVideoPlane, MatrixCoefficients,
    MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PixelFormat, SampleRate, SequenceNumber, SignalRange, TimeBase, TransferFunction,
    VideoDimensions, VideoFrameMetadata,
};
use serde_json::{Map, Value};

use crate::audio_index::{AudioMetadataIndex, AudioProbePlan};
use crate::audio_seek::{
    AudioSeek, parse_input_start_microseconds, sample_pts as audio_sample_pts,
    timestamp_microseconds_floor, validate_diagnostic as validate_seek_diagnostic,
};
use crate::{Adapter, Error, LimitKind, Source, StreamInfo, StreamSelector, Tool, Unsupported};

const AUDIO_POSITION_PROBE_BLOCKS: usize = 32;
const AUDIO_SEEK_CANDIDATES: usize = 8;

/// One selected, non-empty bounded stream sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceRequest {
    pub selector: StreamSelector,
    pub count: NonZeroU32,
}

/// Video and audio requested from the same source and caller clock domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeRequest {
    pub clock_domain: ClockDomainId,
    pub video: Option<SequenceRequest>,
    pub audio: Option<SequenceRequest>,
}

/// Finite decoded sequences. An unrequested media type is empty.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedSequence {
    pub video: Vec<CpuVideoFrame>,
    pub audio: Vec<AudioBlock>,
}

/// One bounded page from a sequential local-video cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedVideoWindow {
    pub frames: Vec<CpuVideoFrame>,
    pub end_of_stream: bool,
}

/// One bounded page from a sequential local-audio cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudioWindow {
    pub blocks: Vec<AudioBlock>,
    pub end_of_stream: bool,
}

/// Result of bounded metadata-only positioning on a local audio cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioCursorPosition {
    pub skipped_blocks: usize,
    pub skipped_samples: usize,
    pub next_block: usize,
    pub next_sample: usize,
    pub end_of_stream: bool,
}

/// A bounded sequential cursor over one fixed local video source and stream.
pub struct LocalVideoDecoder {
    adapter: Adapter,
    source: Source,
    stream: StreamInfo,
    clock_domain: ClockDomainId,
    ordinal: usize,
    end_of_stream: bool,
}

/// A bounded sequential cursor over one fixed local audio source and stream.
pub struct LocalAudioDecoder {
    adapter: Adapter,
    source: Source,
    stream: StreamInfo,
    input_start_microseconds: i64,
    clock_domain: ClockDomainId,
    ordinal: usize,
    absolute_sample_position: usize,
    end_of_stream: bool,
    metadata_index: AudioMetadataIndex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FrameRecord {
    pub pts: i64,
    pub duration: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pixel_format: Option<String>,
    pub interlaced: Option<bool>,
    pub sample_count: Option<usize>,
}

struct PreparedVideo {
    stream: StreamInfo,
    records: Vec<FrameRecord>,
    count: usize,
    width: u32,
    height: u32,
    stride: usize,
    frame_bytes: usize,
    total_bytes: usize,
    metadata: Option<VideoFrameMetadata>,
    start: usize,
    end_of_stream: bool,
}

struct FrameMetadata {
    records: Vec<FrameRecord>,
    end_of_stream: bool,
}

struct PreparedAudio {
    stream: StreamInfo,
    records: Vec<FrameRecord>,
    count: usize,
    sample_rate: u32,
    layout_name: &'static str,
    layout: ChannelLayout,
    channels: usize,
    total_samples: usize,
    total_bytes: usize,
    start: usize,
    end_sample: usize,
    seeks: Vec<AudioSeek>,
    end_of_stream: bool,
}

#[derive(Clone, Copy)]
struct AudioSeekTimeline<'a> {
    records: &'a [FrameRecord],
    sample_positions: &'a [usize],
    start: usize,
    start_sample: usize,
    sample_rate: u32,
    time_base: TimeBase,
    input_start_microseconds: Option<i64>,
}

#[derive(Clone, Copy)]
struct AudioWindowRequest {
    start: usize,
    count: usize,
    requirement: CountRequirement,
    input_start_microseconds: Option<i64>,
    max_samples: usize,
    max_decoded_bytes: usize,
}

#[derive(Clone, Copy)]
struct AudioIndexFormat {
    sample_rate: u32,
    time_base: TimeBase,
}

#[derive(Clone, Copy)]
struct IndexedSeekRequest {
    start: usize,
    start_sample: usize,
    format: AudioIndexFormat,
    input_start_microseconds: Option<i64>,
    limits: crate::Limits,
    selected_samples: usize,
    channels: usize,
}

#[derive(Clone, Copy)]
enum CountRequirement {
    Exact,
    UpTo,
    CursorUpTo,
}

const FRAME_PACKET_SLACK: usize = 64;

impl Adapter {
    /// Opens a sequential cursor over one selected local video stream.
    ///
    /// The canonical source fingerprint, selected stream, and clock domain are
    /// fixed for the lifetime of the cursor.
    ///
    /// # Errors
    ///
    /// Returns a typed path, source-change, tool, probe, or selector error.
    pub fn open_local_video(
        &self,
        path: impl AsRef<Path>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
    ) -> Result<LocalVideoDecoder, Error> {
        let source = self.open_source(path.as_ref())?;
        let probe = self.probe_source(&source)?;
        let stream = probe.select_video(selector)?.clone();
        Ok(LocalVideoDecoder {
            adapter: self.clone(),
            source,
            stream,
            clock_domain,
            ordinal: 0,
            end_of_stream: false,
        })
    }

    /// Opens a sequential cursor over one selected local audio stream.
    ///
    /// The canonical source fingerprint, selected stream, and clock domain are
    /// fixed for the lifetime of the cursor.
    ///
    /// # Errors
    ///
    /// Returns a typed path, source-change, tool, probe, or selector error.
    pub fn open_local_audio(
        &self,
        path: impl AsRef<Path>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
    ) -> Result<LocalAudioDecoder, Error> {
        let source = self.open_source(path.as_ref())?;
        let probe = self.probe_source(&source)?;
        let stream = probe.select_audio(selector)?.clone();
        let input_start_microseconds =
            parse_input_start_microseconds(probe.format.start_time.as_deref())?;
        Ok(LocalAudioDecoder {
            adapter: self.clone(),
            source,
            stream,
            input_start_microseconds,
            clock_domain,
            ordinal: 0,
            absolute_sample_position: 0,
            end_of_stream: false,
            metadata_index: AudioMetadataIndex::new(self.limits),
        })
    }

    /// Decodes only the requested leading video frames and/or audio blocks.
    ///
    /// # Errors
    ///
    /// Returns a typed path, tool, probe, stream, timeline, unsupported-format,
    /// resource-limit, process, or output-contract error.
    pub fn decode_local(
        &self,
        path: impl AsRef<Path>,
        request: DecodeRequest,
    ) -> Result<DecodedSequence, Error> {
        self.decode_local_with_requirement(path.as_ref(), request, CountRequirement::Exact)
    }

    /// Decodes up to the requested leading video frames and/or audio blocks.
    ///
    /// Each requested stream must contain at least one decodable output. A
    /// shorter stream returns every available leading output through end of
    /// stream instead of returning [`Error::MissingFrames`].
    ///
    /// # Errors
    ///
    /// Returns a typed path, tool, probe, stream, timeline, unsupported-format,
    /// resource-limit, process, or output-contract error.
    pub fn decode_local_up_to(
        &self,
        path: impl AsRef<Path>,
        request: DecodeRequest,
    ) -> Result<DecodedSequence, Error> {
        self.decode_local_with_requirement(path.as_ref(), request, CountRequirement::UpTo)
    }

    fn decode_local_with_requirement(
        &self,
        path: &Path,
        request: DecodeRequest,
        requirement: CountRequirement,
    ) -> Result<DecodedSequence, Error> {
        if request.video.is_none() && request.audio.is_none() {
            return Err(Error::InvalidConfig);
        }
        let source = self.open_source(path)?;
        let probe = self.probe_source(&source)?;

        let video = request
            .video
            .map(|sequence| {
                let stream = probe.select_video(sequence.selector)?.clone();
                self.prepare_video(&source, stream, sequence, requirement)
            })
            .transpose()?;
        let audio = request
            .audio
            .map(|sequence| {
                let stream = probe.select_audio(sequence.selector)?.clone();
                self.prepare_audio(&source, stream, sequence, requirement)
            })
            .transpose()?;
        let decoded_bytes = video
            .as_ref()
            .map_or(0, |video| video.total_bytes)
            .checked_add(audio.as_ref().map_or(0, |audio| audio.total_bytes))
            .ok_or_else(|| decoded_limit(u64::MAX, self.limits().max_total_decoded_bytes))?;
        if decoded_bytes > self.limits().max_total_decoded_bytes {
            return Err(decoded_limit(
                u64::try_from(decoded_bytes).unwrap_or(u64::MAX),
                self.limits().max_total_decoded_bytes,
            ));
        }

        let video = video
            .map(|prepared| self.decode_video(&source, &prepared, request.clock_domain))
            .transpose()?
            .unwrap_or_default();
        let audio = audio
            .map(|prepared| self.decode_audio(&source, &prepared, request.clock_domain))
            .transpose()?
            .unwrap_or_default();
        Ok(DecodedSequence { video, audio })
    }

    fn prepare_video(
        &self,
        source: &Source,
        stream: StreamInfo,
        request: SequenceRequest,
        requirement: CountRequirement,
    ) -> Result<PreparedVideo, Error> {
        let count_u32 = request.count.get();
        if count_u32 > self.limits().max_video_frames {
            return Err(Error::LimitExceeded {
                kind: LimitKind::VideoFrames,
                actual: u64::from(count_u32),
                maximum: u64::from(self.limits().max_video_frames),
            });
        }
        let requested_count = usize::try_from(count_u32).map_err(|_| Error::InvalidConfig)?;
        self.prepare_video_window(source, stream, 0, requested_count, requirement)
    }

    fn prepare_video_window(
        &self,
        source: &Source,
        stream: StreamInfo,
        start: usize,
        requested_count: usize,
        requirement: CountRequirement,
    ) -> Result<PreparedVideo, Error> {
        let width = stream.width.ok_or(Error::MalformedProbe)?;
        let height = stream.height.ok_or(Error::MalformedProbe)?;
        check_dimension(width, self.limits().max_width, LimitKind::Width)?;
        check_dimension(height, self.limits().max_height, LimitKind::Height)?;
        if !matches!(stream.sample_aspect_ratio.as_deref(), Some("1:1" | "1/1")) {
            return Err(Error::Unsupported(Unsupported::NonSquarePixels));
        }
        if stream
            .rotation_degrees
            .is_some_and(|rotation| rotation % 360 != 0)
        {
            return Err(Error::Unsupported(Unsupported::Rotation));
        }
        if stream
            .field_order
            .as_deref()
            .is_some_and(|order| order != "progressive" && order != "unknown")
        {
            return Err(Error::Unsupported(Unsupported::InterlacedVideo));
        }
        let pixel_format = stream
            .pixel_format
            .as_deref()
            .ok_or(Error::MalformedProbe)?;
        if !supported_non_alpha_pixel_format(pixel_format) {
            return Err(Error::Unsupported(if source_has_alpha(pixel_format) {
                Unsupported::AlphaVideo
            } else {
                Unsupported::PixelFormat
            }));
        }
        if stream
            .color_transfer
            .as_deref()
            .is_some_and(is_hdr_transfer)
        {
            return Err(Error::Unsupported(Unsupported::HdrTransfer));
        }
        let metadata = self.frame_records(source, stream.index, start, requested_count)?;
        validate_pts_order(&metadata.records)?;
        let requested_end = start
            .checked_add(requested_count)
            .ok_or(Error::InvalidConfig)?;
        let end_of_stream = metadata.end_of_stream && metadata.records.len() <= requested_end;
        let records = metadata
            .records
            .get(start.min(metadata.records.len())..)
            .ok_or(Error::MalformedProbe)?
            .to_vec();
        let count = resolved_count(
            &records,
            requested_count,
            requirement,
            metadata.end_of_stream,
        )?;
        validate_timeline(&records, count)?;
        for record in records.iter().take(count) {
            if record.width != Some(width)
                || record.height != Some(height)
                || record.pixel_format.as_deref() != Some(pixel_format)
            {
                return Err(Error::Unsupported(Unsupported::UnstableVideoFormat));
            }
            if record.interlaced != Some(false) {
                return Err(Error::Unsupported(Unsupported::InterlacedVideo));
            }
        }
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| decoded_limit(u64::MAX, self.limits().max_total_decoded_bytes))?;
        let frame_bytes = stride
            .checked_mul(usize::try_from(height).map_err(|_| Error::MalformedProbe)?)
            .ok_or_else(|| decoded_limit(u64::MAX, self.limits().max_total_decoded_bytes))?;
        let total_bytes = frame_bytes
            .checked_mul(count)
            .ok_or_else(|| decoded_limit(u64::MAX, self.limits().max_total_decoded_bytes))?;
        if total_bytes > self.limits().max_total_decoded_bytes {
            return Err(decoded_limit(
                u64::try_from(total_bytes).unwrap_or(u64::MAX),
                self.limits().max_total_decoded_bytes,
            ));
        }
        Ok(PreparedVideo {
            metadata: mapped_video_metadata(&stream),
            stream,
            records,
            count,
            width,
            height,
            stride,
            frame_bytes,
            total_bytes,
            start,
            end_of_stream,
        })
    }

    fn prepare_audio(
        &self,
        source: &Source,
        stream: StreamInfo,
        request: SequenceRequest,
        requirement: CountRequirement,
    ) -> Result<PreparedAudio, Error> {
        let count_u32 = request.count.get();
        if count_u32 > self.limits().max_audio_blocks {
            return Err(Error::LimitExceeded {
                kind: LimitKind::AudioBlocks,
                actual: u64::from(count_u32),
                maximum: u64::from(self.limits().max_audio_blocks),
            });
        }
        let requested_count = usize::try_from(count_u32).map_err(|_| Error::InvalidConfig)?;
        self.prepare_audio_window(
            source,
            stream,
            AudioWindowRequest {
                start: 0,
                count: requested_count,
                requirement,
                input_start_microseconds: None,
                max_samples: self.limits().max_audio_samples,
                max_decoded_bytes: self.limits().max_total_decoded_bytes,
            },
        )
    }

    fn prepare_audio_window(
        &self,
        source: &Source,
        stream: StreamInfo,
        request: AudioWindowRequest,
    ) -> Result<PreparedAudio, Error> {
        let AudioWindowRequest {
            start,
            count: requested_count,
            requirement,
            input_start_microseconds,
            max_samples,
            max_decoded_bytes,
        } = request;
        let sample_rate = stream.sample_rate.ok_or(Error::MalformedProbe)?;
        let channels = stream.channels.ok_or(Error::MalformedProbe)?;
        let (layout_name, layout) = map_layout(stream.channel_layout.as_deref(), channels)?;
        let channels = usize::try_from(channels).map_err(|_| Error::MalformedProbe)?;
        let metadata = self.frame_records(source, stream.index, start, requested_count)?;
        validate_pts_order(&metadata.records)?;
        let requested_end = start
            .checked_add(requested_count)
            .ok_or(Error::InvalidConfig)?;
        let end_of_stream = metadata.end_of_stream && metadata.records.len() <= requested_end;
        let records = metadata
            .records
            .get(start.min(metadata.records.len())..)
            .ok_or(Error::MalformedProbe)?
            .to_vec();
        let count = resolved_count(
            &records,
            requested_count,
            requirement,
            metadata.end_of_stream,
        )?;
        validate_timeline(&records, count)?;
        let time_base = stream.time_base.ok_or(Error::MalformedProbe)?;
        let selected_end = start.checked_add(count).ok_or(Error::InvalidTimeline)?;
        let mut absolute_sample_position = 0_usize;
        let mut sample_positions = Vec::with_capacity(selected_end.saturating_add(1));
        for (index, record) in metadata.records.iter().take(selected_end).enumerate() {
            sample_positions.push(absolute_sample_position);
            let samples = record.sample_count.ok_or(Error::MalformedProbe)?;
            if samples == 0 {
                return Err(Error::InvalidTimeline);
            }
            absolute_sample_position = absolute_sample_position
                .checked_add(samples)
                .ok_or(Error::InvalidTimeline)?;
            validate_audio_span(record, samples, sample_rate, time_base)?;
            if let Some(next) = metadata.records.get(index + 1) {
                validate_audio_continuity(record, next, samples, sample_rate, time_base)?;
            }
        }
        sample_positions.push(absolute_sample_position);
        let start_sample = *sample_positions.get(start).ok_or(Error::InvalidTimeline)?;
        let end_sample = absolute_sample_position;
        let total_samples = end_sample
            .checked_sub(start_sample)
            .ok_or(Error::InvalidTimeline)?;
        let total_bytes =
            check_audio_window_limits(total_samples, channels, max_samples, max_decoded_bytes)?;
        let seek_limits = crate::Limits {
            max_audio_samples: max_samples,
            max_total_decoded_bytes: max_decoded_bytes,
            ..self.limits()
        };
        let seeks = audio_seek_plan(
            AudioSeekTimeline {
                records: &metadata.records,
                sample_positions: &sample_positions,
                start,
                start_sample,
                sample_rate,
                time_base,
                input_start_microseconds,
            },
            seek_limits,
            total_samples,
            channels,
        )?;
        if seeks.is_empty() {
            return Err(Error::Unsupported(Unsupported::AudioSeek));
        }
        Ok(PreparedAudio {
            stream,
            records,
            count,
            sample_rate,
            layout_name,
            layout,
            channels,
            total_samples,
            total_bytes,
            start,
            end_sample,
            seeks,
            end_of_stream,
        })
    }

    fn prepare_indexed_audio_window(
        &self,
        source: &Source,
        stream: StreamInfo,
        index: &mut AudioMetadataIndex,
        request: AudioWindowRequest,
    ) -> Result<PreparedAudio, Error> {
        let AudioWindowRequest {
            start,
            count: requested_count,
            requirement,
            input_start_microseconds,
            max_samples,
            max_decoded_bytes,
        } = request;
        let sample_rate = stream.sample_rate.ok_or(Error::MalformedProbe)?;
        let channels = stream.channels.ok_or(Error::MalformedProbe)?;
        let (layout_name, layout) = map_layout(stream.channel_layout.as_deref(), channels)?;
        let channels = usize::try_from(channels).map_err(|_| Error::MalformedProbe)?;
        let time_base = stream.time_base.ok_or(Error::MalformedProbe)?;
        let required_end = start
            .checked_add(requested_count)
            .and_then(|value| value.checked_add(1))
            .ok_or(Error::InvalidConfig)?;

        self.ensure_audio_index(
            source,
            stream.index,
            index,
            start,
            required_end,
            AudioIndexFormat {
                sample_rate,
                time_base,
            },
        )?;
        let indexed = index.records_from(start, requested_count.saturating_add(1));
        let records = indexed
            .iter()
            .map(|record| record.frame.clone())
            .collect::<Vec<_>>();
        let count = resolved_count(
            &records,
            requested_count,
            requirement,
            index.end_of_stream(),
        )?;
        validate_timeline(&records, count)?;
        let start_sample = index
            .sample_at(start)
            .ok_or(Error::IncompleteFrameMetadata)?;
        let end_sample = indexed
            .get(count)
            .map(|record| record.start_sample)
            .or_else(|| {
                indexed.get(count.checked_sub(1)?).and_then(|record| {
                    record
                        .frame
                        .sample_count
                        .and_then(|samples| record.start_sample.checked_add(samples))
                })
            })
            .unwrap_or(start_sample);
        let total_samples = end_sample
            .checked_sub(start_sample)
            .ok_or(Error::InvalidTimeline)?;
        let total_bytes =
            check_audio_window_limits(total_samples, channels, max_samples, max_decoded_bytes)?;

        let seek_limits = crate::Limits {
            max_audio_samples: max_samples,
            max_total_decoded_bytes: max_decoded_bytes,
            ..self.limits()
        };
        let seeks = indexed_audio_seek_plan(
            index,
            &IndexedSeekRequest {
                start,
                start_sample,
                format: AudioIndexFormat {
                    sample_rate,
                    time_base,
                },
                input_start_microseconds,
                limits: seek_limits,
                selected_samples: total_samples,
                channels,
            },
        )?;
        if seeks.is_empty() {
            return Err(Error::Unsupported(Unsupported::AudioSeek));
        }
        Ok(PreparedAudio {
            stream,
            records,
            count,
            sample_rate,
            layout_name,
            layout,
            channels,
            total_samples,
            total_bytes,
            start,
            end_sample,
            seeks,
            end_of_stream: index.end_of_stream() && start + count == index.next_ordinal(),
        })
    }

    fn ensure_audio_index(
        &self,
        source: &Source,
        stream_index: u32,
        index: &mut AudioMetadataIndex,
        start: usize,
        required_end: usize,
        format: AudioIndexFormat,
    ) -> Result<(), Error> {
        self.check_source(source)?;
        let deadline = Instant::now()
            .checked_add(self.limits().frame_metadata_timeout)
            .ok_or(Error::InvalidConfig)?;
        while !index.contains(start, required_end) {
            let plans = index.probe_plans(required_end, FRAME_PACKET_SLACK)?;
            let mut last_resume_error = None;
            let previous_frontier = index.next_ordinal();
            for plan in plans {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(Error::ProcessTimedOut {
                        tool: Tool::Ffprobe,
                    })?;
                match self.extend_audio_index(source, stream_index, index, &plan, format, remaining)
                {
                    Ok(()) => {
                        last_resume_error = None;
                        break;
                    }
                    Err(error @ (Error::IncompleteFrameMetadata | Error::InvalidTimeline))
                        if plan.checkpoint.is_some() =>
                    {
                        last_resume_error = Some(error);
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Some(error) = last_resume_error {
                return Err(error);
            }
            if index.next_ordinal() == previous_frontier && !index.end_of_stream() {
                return Err(Error::IncompleteFrameMetadata);
            }
        }
        self.check_source(source)
    }

    fn extend_audio_index(
        &self,
        source: &Source,
        stream_index: u32,
        index: &mut AudioMetadataIndex,
        plan: &AudioProbePlan,
        format: AudioIndexFormat,
        timeout: std::time::Duration,
    ) -> Result<(), Error> {
        let interval_start = plan
            .checkpoint
            .as_ref()
            .map(|checkpoint| {
                let value = timestamp_microseconds_floor(checkpoint.frame.pts, format.time_base)?;
                (value >= 0)
                    .then_some(value)
                    .ok_or(Error::IncompleteFrameMetadata)
            })
            .transpose()?;
        let args = audio_frame_probe_args(
            &source.path,
            stream_index,
            plan.packet_budget,
            interval_start,
        );
        index.note_probe(plan);
        let output = self.run_source(
            source,
            Tool::Ffprobe,
            &args,
            timeout,
            self.limits().max_frame_metadata_stdout_bytes,
        )?;
        let metadata = parse_frame_records(
            &output.stdout,
            stream_index,
            plan.packet_budget,
            plan.packet_budget,
        )?;
        validate_pts_order(&metadata.records)?;
        for (position, record) in metadata.records.iter().enumerate() {
            let samples = record.sample_count.ok_or(Error::MalformedProbe)?;
            if samples == 0 {
                return Err(Error::InvalidTimeline);
            }
            validate_audio_span(record, samples, format.sample_rate, format.time_base)?;
            if let Some(next) = metadata.records.get(position + 1) {
                validate_audio_continuity(
                    record,
                    next,
                    samples,
                    format.sample_rate,
                    format.time_base,
                )?;
            }
        }
        let mut candidate = index.clone();
        candidate.commit(plan, &metadata.records, metadata.end_of_stream)?;
        *index = candidate;
        Ok(())
    }

    fn frame_records(
        &self,
        source: &Source,
        stream_index: u32,
        start: usize,
        count: usize,
    ) -> Result<FrameMetadata, Error> {
        let prefix = start
            .checked_add(count)
            .and_then(|value| value.checked_add(1))
            .ok_or(Error::InvalidConfig)?;
        let packet_budget = prefix
            .checked_add(FRAME_PACKET_SLACK)
            .ok_or(Error::InvalidConfig)?;
        let args = frame_probe_args(&source.path, stream_index, packet_budget);
        let output = self.run_source(
            source,
            Tool::Ffprobe,
            &args,
            self.limits().frame_metadata_timeout,
            self.limits().max_frame_metadata_stdout_bytes,
        )?;
        parse_frame_records(&output.stdout, stream_index, prefix, packet_budget)
    }

    fn decode_video(
        &self,
        source: &Source,
        prepared: &PreparedVideo,
        clock_domain: ClockDomainId,
    ) -> Result<Vec<CpuVideoFrame>, Error> {
        if prepared.count == 0 {
            return Ok(Vec::new());
        }
        let args = video_decode_args(
            &source.path,
            prepared.stream.index,
            prepared.start,
            prepared.count,
        )?;
        let output = self.run_source(
            source,
            Tool::Ffmpeg,
            &args,
            self.limits().decode_timeout,
            prepared.total_bytes,
        )?;
        if output.stdout.len() != prepared.total_bytes {
            return Err(Error::OutputMismatch {
                expected: prepared.total_bytes,
                actual: output.stdout.len(),
            });
        }
        let time_base = prepared.stream.time_base.ok_or(Error::MalformedProbe)?;
        let dimensions = VideoDimensions::new(prepared.width, prepared.height)
            .ok_or(Error::FrameConstruction)?;
        prepared
            .records
            .iter()
            .take(prepared.count)
            .enumerate()
            .map(|(index, record)| {
                let duration_ticks = duration_ticks(&prepared.records, index)?;
                let timing = media_timing(
                    record.pts,
                    duration_ticks,
                    time_base,
                    clock_domain,
                    prepared.start + index,
                )?;
                let start = index
                    .checked_mul(prepared.frame_bytes)
                    .ok_or(Error::FrameConstruction)?;
                let end = start
                    .checked_add(prepared.frame_bytes)
                    .ok_or(Error::FrameConstruction)?;
                let bytes = output
                    .stdout
                    .get(start..end)
                    .ok_or(Error::OutputMismatch {
                        expected: prepared.total_bytes,
                        actual: output.stdout.len(),
                    })?
                    .to_vec();
                let plane = CpuVideoPlane::new(prepared.stride, bytes)
                    .map_err(|_| Error::FrameConstruction)?;
                let payload = CpuVideoPayload::new(PixelFormat::Rgba8, dimensions, vec![plane])
                    .map_err(|_| Error::FrameConstruction)?;
                let frame = CpuVideoFrame::new(timing, payload);
                match prepared.metadata {
                    Some(metadata) => frame
                        .with_metadata(metadata)
                        .map_err(|_| Error::FrameConstruction),
                    None => Ok(frame),
                }
            })
            .collect()
    }

    fn decode_audio(
        &self,
        source: &Source,
        prepared: &PreparedAudio,
        clock_domain: ClockDomainId,
    ) -> Result<Vec<AudioBlock>, Error> {
        if prepared.count == 0 {
            return Ok(Vec::new());
        }
        let output = self.run_audio_window(source, prepared)?;
        let samples = output
            .stdout
            .chunks_exact(size_of::<f32>())
            .map(|bytes| {
                let bytes: [u8; 4] = bytes.try_into().map_err(|_| Error::OutputMismatch {
                    expected: prepared.total_bytes,
                    actual: output.stdout.len(),
                })?;
                let sample = f32::from_le_bytes(bytes);
                if sample.is_finite() {
                    Ok(sample)
                } else {
                    Err(Error::NonFiniteAudio)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if samples.len() != prepared.total_samples * prepared.channels {
            return Err(Error::OutputMismatch {
                expected: prepared.total_bytes,
                actual: output.stdout.len(),
            });
        }

        let time_base = prepared.stream.time_base.ok_or(Error::MalformedProbe)?;
        let sample_rate = SampleRate::new(prepared.sample_rate).ok_or(Error::MalformedProbe)?;
        let mut sample_offset = 0_usize;
        prepared
            .records
            .iter()
            .take(prepared.count)
            .enumerate()
            .map(|(sequence, record)| {
                let sample_count = record.sample_count.ok_or(Error::MalformedProbe)?;
                let interleaved_count = sample_count
                    .checked_mul(prepared.channels)
                    .ok_or(Error::FrameConstruction)?;
                let end = sample_offset
                    .checked_add(interleaved_count)
                    .ok_or(Error::FrameConstruction)?;
                let interleaved = samples
                    .get(sample_offset..end)
                    .ok_or(Error::OutputMismatch {
                        expected: prepared.total_bytes,
                        actual: output.stdout.len(),
                    })?;
                sample_offset = end;
                let mut planes = (0..prepared.channels)
                    .map(|_| Vec::with_capacity(sample_count))
                    .collect::<Vec<_>>();
                for frame in interleaved.chunks_exact(prepared.channels) {
                    for (plane, sample) in planes.iter_mut().zip(frame) {
                        plane.push(*sample);
                    }
                }
                let duration_ticks = duration_ticks(&prepared.records, sequence)?;
                let timing = media_timing(
                    record.pts,
                    duration_ticks,
                    time_base,
                    clock_domain,
                    prepared.start + sequence,
                )?;
                AudioBlock::new(timing, sample_rate, prepared.layout.clone(), planes)
                    .map_err(|_| Error::FrameConstruction)
            })
            .collect()
    }

    fn run_audio_window(
        &self,
        source: &Source,
        prepared: &PreparedAudio,
    ) -> Result<crate::RunOutput, Error> {
        let deadline = Instant::now()
            .checked_add(self.limits().decode_timeout)
            .ok_or(Error::InvalidConfig)?;
        let mut decoded = None;
        let mut last_anchor_error = None;
        for seek in &prepared.seeks {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::ProcessTimedOut { tool: Tool::Ffmpeg })?;
            if remaining.is_zero() {
                return Err(Error::ProcessTimedOut { tool: Tool::Ffmpeg });
            }
            let args = audio_decode_args(
                &source.path,
                prepared.stream.index,
                *seek,
                prepared.total_samples,
                prepared.layout_name,
            )?;
            let output =
                match self.run_source(source, Tool::Ffmpeg, &args, remaining, prepared.total_bytes)
                {
                    Ok(output) => output,
                    Err(error @ Error::ProcessFailed { .. }) => {
                        last_anchor_error = Some(error);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            if let Some(expected_first_pts) = seek.expected_first_pts
                && let Err(error) = validate_seek_diagnostic(
                    &output.stderr,
                    expected_first_pts,
                    prepared.sample_rate,
                )
            {
                last_anchor_error = Some(error);
                continue;
            }
            if output.stdout.len() != prepared.total_bytes {
                last_anchor_error = Some(Error::OutputMismatch {
                    expected: prepared.total_bytes,
                    actual: output.stdout.len(),
                });
                continue;
            }
            decoded = Some(output);
            break;
        }
        let output = decoded.ok_or_else(|| {
            if prepared
                .seeks
                .iter()
                .any(|seek| seek.input_microseconds.is_some())
            {
                Error::Unsupported(Unsupported::AudioSeek)
            } else {
                last_anchor_error.unwrap_or(Error::Unsupported(Unsupported::AudioSeek))
            }
        })?;
        Ok(output)
    }
}

impl LocalVideoDecoder {
    /// Decodes the next non-overlapping window, shortening only at proven EOS.
    ///
    /// Cursor state advances only after the complete window has been decoded
    /// and constructed successfully.
    ///
    /// # Errors
    ///
    /// Returns the same typed video errors as [`Adapter::decode_local_up_to`].
    pub fn decode_up_to(&mut self, count: NonZeroU32) -> Result<DecodedVideoWindow, Error> {
        if self.end_of_stream {
            return Ok(DecodedVideoWindow {
                frames: Vec::new(),
                end_of_stream: true,
            });
        }

        if count.get() > self.adapter.limits().max_video_frames {
            return Err(Error::LimitExceeded {
                kind: LimitKind::VideoFrames,
                actual: u64::from(count.get()),
                maximum: u64::from(self.adapter.limits().max_video_frames),
            });
        }
        let requested = usize::try_from(count.get()).map_err(|_| Error::InvalidConfig)?;
        self.ordinal
            .checked_add(requested)
            .ok_or(Error::InvalidTimeline)?;

        let prepared = self.adapter.prepare_video_window(
            &self.source,
            self.stream.clone(),
            self.ordinal,
            requested,
            CountRequirement::CursorUpTo,
        )?;
        let end_of_stream = prepared.end_of_stream;
        let frames = self
            .adapter
            .decode_video(&self.source, &prepared, self.clock_domain)?;
        let next_ordinal = self
            .ordinal
            .checked_add(frames.len())
            .ok_or(Error::InvalidTimeline)?;

        self.ordinal = next_ordinal;
        self.end_of_stream = end_of_stream;
        Ok(DecodedVideoWindow {
            frames,
            end_of_stream,
        })
    }
}

impl LocalAudioDecoder {
    /// Returns bounded metadata-index work and retention telemetry.
    #[must_use]
    pub fn metadata_index_telemetry(&self) -> crate::AudioMetadataIndexTelemetry {
        self.metadata_index.telemetry()
    }

    fn check_source_identity(&mut self) -> Result<(), Error> {
        match self.adapter.check_source(&self.source) {
            Ok(()) => Ok(()),
            Err(error @ Error::SourceChanged) => {
                self.metadata_index.invalidate();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    fn prepare_indexed_window(
        &mut self,
        start: usize,
        count: usize,
        max_samples: usize,
        max_decoded_bytes: usize,
    ) -> Result<PreparedAudio, Error> {
        let prepared = self.adapter.prepare_indexed_audio_window(
            &self.source,
            self.stream.clone(),
            &mut self.metadata_index,
            AudioWindowRequest {
                start,
                count,
                requirement: CountRequirement::CursorUpTo,
                input_start_microseconds: Some(self.input_start_microseconds),
                max_samples,
                max_decoded_bytes,
            },
        );
        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(error @ Error::SourceChanged) => {
                self.metadata_index.invalidate();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Skips complete audio blocks ending at or before `target_sample` without
    /// decoding their PCM. The block containing the target remains next.
    ///
    /// Metadata probing is bounded by `max_skip_blocks`; cursor state changes
    /// only after the full positioning operation succeeds.
    ///
    /// # Errors
    ///
    /// Returns a typed metadata, timeline, process, or block-limit error. A
    /// target beyond the bounded probe returns [`Error::LimitExceeded`] and
    /// leaves the cursor unchanged.
    pub fn skip_complete_blocks_to_sample_bounded(
        &mut self,
        target_sample: usize,
        max_skip_blocks: usize,
    ) -> Result<AudioCursorPosition, Error> {
        if max_skip_blocks == 0 {
            return Err(Error::InvalidConfig);
        }
        self.check_source_identity()?;
        if target_sample <= self.absolute_sample_position || self.end_of_stream {
            return Ok(AudioCursorPosition {
                skipped_blocks: 0,
                skipped_samples: 0,
                next_block: self.ordinal,
                next_sample: self.absolute_sample_position,
                end_of_stream: self.end_of_stream,
            });
        }

        let initial_ordinal = self.ordinal;
        let initial_sample = self.absolute_sample_position;
        let mut ordinal = initial_ordinal;
        let mut sample = initial_sample;
        let mut end_of_stream = false;
        let per_probe = usize::try_from(self.adapter.limits().max_audio_blocks)
            .map_err(|_| Error::InvalidConfig)?
            .min(AUDIO_POSITION_PROBE_BLOCKS);

        while sample < target_sample && ordinal - initial_ordinal < max_skip_blocks {
            let remaining = max_skip_blocks - (ordinal - initial_ordinal);
            let requested = remaining.min(per_probe);
            let prepared = self.prepare_indexed_window(
                ordinal,
                requested,
                self.adapter.limits().max_audio_samples,
                self.adapter.limits().max_total_decoded_bytes,
            )?;
            let prepared_start_sample = prepared
                .end_sample
                .checked_sub(prepared.total_samples)
                .ok_or(Error::InvalidTimeline)?;
            if prepared_start_sample != sample {
                return Err(Error::InvalidTimeline);
            }
            let mut skipped = 0_usize;
            let mut skipped_samples = 0_usize;
            for record in prepared.records.iter().take(prepared.count) {
                let block_samples = record.sample_count.ok_or(Error::MalformedProbe)?;
                let block_end = sample
                    .checked_add(skipped_samples)
                    .and_then(|position| position.checked_add(block_samples))
                    .ok_or(Error::InvalidTimeline)?;
                if block_end > target_sample {
                    break;
                }
                skipped += 1;
                skipped_samples = skipped_samples
                    .checked_add(block_samples)
                    .ok_or(Error::InvalidTimeline)?;
            }
            if skipped == 0 {
                break;
            }
            ordinal = ordinal.checked_add(skipped).ok_or(Error::InvalidTimeline)?;
            sample = sample
                .checked_add(skipped_samples)
                .ok_or(Error::InvalidTimeline)?;
            end_of_stream = prepared.end_of_stream && skipped == prepared.count;
            if skipped < prepared.count || end_of_stream {
                break;
            }
        }

        if sample < target_sample && !end_of_stream && ordinal - initial_ordinal == max_skip_blocks
        {
            return Err(Error::LimitExceeded {
                kind: LimitKind::AudioBlocks,
                actual: u64::try_from(max_skip_blocks)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
                maximum: u64::try_from(max_skip_blocks).unwrap_or(u64::MAX),
            });
        }
        self.check_source_identity()?;
        self.ordinal = ordinal;
        self.absolute_sample_position = sample;
        self.end_of_stream = end_of_stream;
        Ok(AudioCursorPosition {
            skipped_blocks: ordinal - initial_ordinal,
            skipped_samples: sample - initial_sample,
            next_block: ordinal,
            next_sample: sample,
            end_of_stream,
        })
    }

    /// Decodes the next non-overlapping window, shortening only at proven EOS.
    ///
    /// Cursor state advances only after the complete window has been decoded
    /// and constructed successfully.
    ///
    /// # Errors
    ///
    /// Returns the same typed audio errors as [`Adapter::decode_local_up_to`].
    pub fn decode_up_to(&mut self, count: NonZeroU32) -> Result<DecodedAudioWindow, Error> {
        self.decode_up_to_bounded(
            count,
            self.adapter.limits().max_audio_samples,
            self.adapter.limits().max_total_decoded_bytes,
        )
    }

    /// Decodes the next non-overlapping window with additional per-page sample
    /// and decoded-byte bounds.
    ///
    /// Cursor state is unchanged when either page bound is exceeded. Configured
    /// adapter limits also apply per page; the tighter bound wins.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] for a zero page bound, a typed limit
    /// error when the prepared page is too large, or the same errors as
    /// [`Self::decode_up_to`].
    pub fn decode_up_to_bounded(
        &mut self,
        count: NonZeroU32,
        max_page_samples: usize,
        max_page_decoded_bytes: usize,
    ) -> Result<DecodedAudioWindow, Error> {
        if max_page_samples == 0 || max_page_decoded_bytes == 0 {
            return Err(Error::InvalidConfig);
        }
        self.check_source_identity()?;
        if self.end_of_stream {
            return Ok(DecodedAudioWindow {
                blocks: Vec::new(),
                end_of_stream: true,
            });
        }

        if count.get() > self.adapter.limits().max_audio_blocks {
            return Err(Error::LimitExceeded {
                kind: LimitKind::AudioBlocks,
                actual: u64::from(count.get()),
                maximum: u64::from(self.adapter.limits().max_audio_blocks),
            });
        }
        let requested = usize::try_from(count.get()).map_err(|_| Error::InvalidConfig)?;
        self.ordinal
            .checked_add(requested)
            .ok_or(Error::InvalidTimeline)?;

        let prepared = self.prepare_indexed_window(
            self.ordinal,
            requested,
            max_page_samples.min(self.adapter.limits().max_audio_samples),
            max_page_decoded_bytes.min(self.adapter.limits().max_total_decoded_bytes),
        )?;
        let prepared_start_sample = prepared
            .end_sample
            .checked_sub(prepared.total_samples)
            .ok_or(Error::InvalidTimeline)?;
        if prepared_start_sample != self.absolute_sample_position {
            return Err(Error::InvalidTimeline);
        }
        let next_absolute_sample_position = self
            .absolute_sample_position
            .checked_add(prepared.total_samples)
            .ok_or(Error::InvalidTimeline)?;
        if next_absolute_sample_position != prepared.end_sample {
            return Err(Error::InvalidTimeline);
        }
        let end_of_stream = prepared.end_of_stream;
        let blocks = match self
            .adapter
            .decode_audio(&self.source, &prepared, self.clock_domain)
        {
            Ok(blocks) => blocks,
            Err(error @ Error::SourceChanged) => {
                self.metadata_index.invalidate();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let next_ordinal = self
            .ordinal
            .checked_add(blocks.len())
            .ok_or(Error::InvalidTimeline)?;

        self.ordinal = next_ordinal;
        self.absolute_sample_position = next_absolute_sample_position;
        self.end_of_stream = end_of_stream;
        Ok(DecodedAudioWindow {
            blocks,
            end_of_stream,
        })
    }
}

fn frame_probe_args(path: &Path, stream_index: u32, packet_budget: usize) -> Vec<OsString> {
    frame_probe_args_with_interval(path, stream_index, packet_budget, None)
}

fn audio_frame_probe_args(
    path: &Path,
    stream_index: u32,
    packet_budget: usize,
    start_microseconds: Option<i64>,
) -> Vec<OsString> {
    frame_probe_args_with_interval(path, stream_index, packet_budget, start_microseconds)
}

fn frame_probe_args_with_interval(
    path: &Path,
    stream_index: u32,
    packet_budget: usize,
    start_microseconds: Option<i64>,
) -> Vec<OsString> {
    let interval = start_microseconds.map_or_else(
        || format!("%+#{packet_budget}"),
        |microseconds| {
            format!(
                "{}.{:06}%+#{packet_budget}",
                microseconds / 1_000_000,
                microseconds % 1_000_000
            )
        },
    );
    [
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-select_streams"),
        OsString::from(stream_index.to_string()),
        OsString::from("-count_packets"),
        OsString::from("-show_frames"),
        OsString::from("-show_streams"),
        OsString::from("-show_entries"),
        OsString::from(
            "frame=stream_index,best_effort_timestamp,pts,duration,pkt_duration,nb_samples,width,height,pix_fmt,interlaced_frame:stream=index,nb_read_packets",
        ),
        OsString::from("-read_intervals"),
        OsString::from(interval),
        OsString::from("-of"),
        OsString::from("json"),
        path.as_os_str().to_owned(),
    ]
    .into()
}

fn video_decode_args(
    path: &Path,
    stream_index: u32,
    start: usize,
    count: usize,
) -> Result<Vec<OsString>, Error> {
    let end = start.checked_add(count).ok_or(Error::InvalidTimeline)?;
    Ok([
        OsString::from("-nostdin"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-noautorotate"),
        OsString::from("-i"),
        path.as_os_str().to_owned(),
        OsString::from("-map"),
        OsString::from(format!("0:{stream_index}")),
        OsString::from("-vf"),
        OsString::from(format!("trim=start_frame={start}:end_frame={end}")),
        OsString::from("-frames:v"),
        OsString::from(count.to_string()),
        OsString::from("-fps_mode"),
        OsString::from("passthrough"),
        OsString::from("-an"),
        OsString::from("-sn"),
        OsString::from("-dn"),
        OsString::from("-pix_fmt"),
        OsString::from("rgba"),
        OsString::from("-f"),
        OsString::from("rawvideo"),
        OsString::from("pipe:1"),
    ]
    .into())
}

fn audio_decode_args(
    path: &Path,
    stream_index: u32,
    seek: AudioSeek,
    sample_count: usize,
    layout: &str,
) -> Result<Vec<OsString>, Error> {
    let end_sample = seek
        .correction_samples
        .checked_add(sample_count)
        .ok_or(Error::InvalidTimeline)?;
    let mut args = vec![
        OsString::from("-nostdin"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from(if seek.expected_first_pts.is_some() {
            "info"
        } else {
            "error"
        }),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
    ];
    if let Some(input_microseconds) = seek.input_microseconds {
        args.extend([
            OsString::from("-copyts"),
            OsString::from("-ss"),
            OsString::from(format!("{input_microseconds}us")),
        ]);
    }
    args.extend([OsString::from("-i"), path.as_os_str().to_owned()]);
    if seek.expected_first_pts.is_some() {
        args.extend([
            OsString::from("-filter_complex"),
            OsString::from(format!(
                "[0:{stream_index}]asplit=2[main][verify];\
                 [verify]atrim=end_sample=1,ashowinfo,anullsink;\
                 [main]atrim=start_sample={}:end_sample={end_sample}[out]",
                seek.correction_samples,
            )),
            OsString::from("-map"),
            OsString::from("[out]"),
        ]);
    } else {
        args.extend([
            OsString::from("-map"),
            OsString::from(format!("0:{stream_index}")),
            OsString::from("-af"),
            OsString::from(format!(
                "atrim=start_sample={}:end_sample={end_sample}",
                seek.correction_samples,
            )),
        ]);
    }
    args.extend([
        OsString::from("-vn"),
        OsString::from("-sn"),
        OsString::from("-dn"),
        OsString::from("-channel_layout"),
        OsString::from(layout),
        OsString::from("-c:a"),
        OsString::from("pcm_f32le"),
        OsString::from("-f"),
        OsString::from("f32le"),
        OsString::from("pipe:1"),
    ]);
    Ok(args)
}

fn check_audio_window_limits(
    total_samples: usize,
    channels: usize,
    max_samples: usize,
    max_decoded_bytes: usize,
) -> Result<usize, Error> {
    if total_samples > max_samples {
        return Err(Error::LimitExceeded {
            kind: LimitKind::AudioSamples,
            actual: u64::try_from(total_samples).unwrap_or(u64::MAX),
            maximum: u64::try_from(max_samples).unwrap_or(u64::MAX),
        });
    }
    let total_bytes = total_samples
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or_else(|| decoded_limit(u64::MAX, max_decoded_bytes))?;
    if total_bytes > max_decoded_bytes {
        return Err(decoded_limit(
            u64::try_from(total_bytes).unwrap_or(u64::MAX),
            max_decoded_bytes,
        ));
    }
    Ok(total_bytes)
}

fn audio_correction_budget(
    limits: crate::Limits,
    selected_samples: usize,
    channels: usize,
    sample_rate: u32,
) -> Result<usize, Error> {
    let bytes_per_sample_frame = channels
        .checked_mul(size_of::<f32>())
        .ok_or(Error::InvalidTimeline)?;
    let byte_sample_limit = limits.max_total_decoded_bytes / bytes_per_sample_frame;
    let timeout_sample_limit = limits
        .decode_timeout
        .as_nanos()
        .checked_mul(u128::from(sample_rate))
        .and_then(|value| value.checked_div(1_000_000_000))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(Error::InvalidTimeline)?;
    let operation_sample_limit = limits
        .max_audio_samples
        .min(byte_sample_limit)
        .min(timeout_sample_limit);
    Ok(operation_sample_limit.saturating_sub(selected_samples))
}

fn audio_seek_plan(
    timeline: AudioSeekTimeline<'_>,
    limits: crate::Limits,
    selected_samples: usize,
    channels: usize,
) -> Result<Vec<AudioSeek>, Error> {
    let AudioSeekTimeline {
        records,
        sample_positions,
        start,
        start_sample,
        sample_rate,
        time_base,
        input_start_microseconds,
    } = timeline;
    let Some(input_start_microseconds) = input_start_microseconds else {
        return Ok(vec![AudioSeek {
            input_microseconds: None,
            expected_first_pts: None,
            correction_samples: start_sample,
        }]);
    };
    if start == 0 {
        return Ok(vec![AudioSeek {
            input_microseconds: None,
            expected_first_pts: None,
            correction_samples: 0,
        }]);
    }
    let correction_budget =
        audio_correction_budget(limits, selected_samples, channels, sample_rate)?;
    if start_sample <= correction_budget {
        return Ok(vec![AudioSeek {
            input_microseconds: None,
            expected_first_pts: None,
            correction_samples: start_sample,
        }]);
    }

    let first_pts = records
        .first()
        .ok_or(Error::InvalidTimeline)
        .and_then(|record| audio_sample_pts(record.pts, sample_rate, time_base))?;
    if first_pts < 0 || input_start_microseconds < 0 {
        return Err(Error::Unsupported(Unsupported::NegativeAudioAnchor));
    }

    let anchors = audio_anchor_indices(sample_positions, start, start_sample, correction_budget)?;
    let target_pts = records
        .get(start)
        .ok_or(Error::InvalidTimeline)
        .and_then(|record| audio_sample_pts(record.pts, sample_rate, time_base))?;
    let mut seeks = Vec::with_capacity(anchors.len());
    for anchor in anchors {
        let anchor_sample = *sample_positions.get(anchor).ok_or(Error::InvalidTimeline)?;
        let correction_samples = start_sample
            .checked_sub(anchor_sample)
            .ok_or(Error::InvalidTimeline)?;
        let anchor_record = records.get(anchor).ok_or(Error::InvalidTimeline)?;
        let expected_first_pts = audio_sample_pts(anchor_record.pts, sample_rate, time_base)?;
        if target_pts
            .checked_sub(expected_first_pts)
            .and_then(|samples| usize::try_from(samples).ok())
            != Some(correction_samples)
        {
            return Err(Error::InvalidTimeline);
        }
        let input_microseconds = timestamp_microseconds_floor(anchor_record.pts, time_base)?
            .checked_sub(input_start_microseconds)
            .ok_or(Error::InvalidTimeline)?;
        if input_microseconds > 0 {
            seeks.push(AudioSeek {
                input_microseconds: Some(
                    u64::try_from(input_microseconds).map_err(|_| Error::InvalidTimeline)?,
                ),
                expected_first_pts: Some(expected_first_pts),
                correction_samples,
            });
        }
    }
    if seeks.is_empty() {
        Err(Error::Unsupported(Unsupported::AudioSeek))
    } else {
        Ok(seeks)
    }
}

fn indexed_audio_seek_plan(
    index: &AudioMetadataIndex,
    request: &IndexedSeekRequest,
) -> Result<Vec<AudioSeek>, Error> {
    let &IndexedSeekRequest {
        start,
        start_sample,
        format,
        input_start_microseconds,
        limits,
        selected_samples,
        channels,
    } = request;
    let anchors = index.records_through(start, usize::MAX);
    let records = anchors
        .iter()
        .map(|record| record.frame.clone())
        .collect::<Vec<_>>();
    let sample_positions = anchors
        .iter()
        .map(|record| record.start_sample)
        .chain(std::iter::once(
            anchors
                .last()
                .and_then(|record| {
                    record
                        .frame
                        .sample_count
                        .and_then(|samples| record.start_sample.checked_add(samples))
                })
                .unwrap_or(start_sample),
        ))
        .collect::<Vec<_>>();
    let local_start = anchors
        .iter()
        .position(|record| record.ordinal == start)
        .ok_or(Error::IncompleteFrameMetadata)?;
    audio_seek_plan(
        AudioSeekTimeline {
            records: &records,
            sample_positions: &sample_positions,
            start: local_start,
            start_sample,
            sample_rate: format.sample_rate,
            time_base: format.time_base,
            input_start_microseconds,
        },
        limits,
        selected_samples,
        channels,
    )
}

fn audio_anchor_indices(
    sample_positions: &[usize],
    start: usize,
    start_sample: usize,
    correction_budget: usize,
) -> Result<Vec<usize>, Error> {
    let mut earliest = start;
    while earliest > 0 {
        let candidate = earliest - 1;
        let candidate_sample = *sample_positions
            .get(candidate)
            .ok_or(Error::InvalidTimeline)?;
        let correction = start_sample
            .checked_sub(candidate_sample)
            .ok_or(Error::InvalidTimeline)?;
        if correction > correction_budget {
            break;
        }
        earliest = candidate;
    }

    let available = start
        .checked_sub(earliest)
        .and_then(|span| span.checked_add(1))
        .ok_or(Error::InvalidTimeline)?;
    let count = available.min(AUDIO_SEEK_CANDIDATES);
    if count == 1 {
        return Ok(vec![start]);
    }

    let span = start.checked_sub(earliest).ok_or(Error::InvalidTimeline)?;
    let mut anchors = Vec::with_capacity(count);
    for position in 0..count {
        let offset = span
            .checked_mul(position)
            .and_then(|value| value.checked_div(count - 1))
            .ok_or(Error::InvalidTimeline)?;
        let anchor = earliest.checked_add(offset).ok_or(Error::InvalidTimeline)?;
        if anchors.last() != Some(&anchor) {
            anchors.push(anchor);
        }
    }
    Ok(anchors)
}

fn parse_frame_records(
    bytes: &[u8],
    stream_index: u32,
    maximum: usize,
    packet_budget: usize,
) -> Result<FrameMetadata, Error> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| Error::MalformedProbe)?;
    let frames = root
        .get("frames")
        .and_then(Value::as_array)
        .ok_or(Error::MalformedProbe)?;
    let records = frames
        .iter()
        .take(maximum)
        .map(|frame| parse_frame(frame, stream_index))
        .collect::<Result<Vec<_>, _>>()?;
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .ok_or(Error::MalformedProbe)?;
    if streams.len() != 1 {
        return Err(Error::MalformedProbe);
    }
    let stream = streams[0].as_object().ok_or(Error::MalformedProbe)?;
    if u32_value(stream, "index")? != Some(stream_index) {
        return Err(Error::MalformedProbe);
    }
    let packets =
        usize::try_from(u64_value(stream, "nb_read_packets")?.ok_or(Error::MalformedProbe)?)
            .map_err(|_| Error::MalformedProbe)?;
    if packets > packet_budget {
        return Err(Error::MalformedProbe);
    }
    Ok(FrameMetadata {
        records,
        end_of_stream: packets < packet_budget,
    })
}

fn parse_frame(value: &Value, expected_stream: u32) -> Result<FrameRecord, Error> {
    let object = value.as_object().ok_or(Error::MalformedProbe)?;
    let stream = u32_value(object, "stream_index")?.ok_or(Error::MalformedProbe)?;
    if stream != expected_stream {
        return Err(Error::MalformedProbe);
    }
    let pts = i64_value(object, "best_effort_timestamp")?
        .or(i64_value(object, "pts")?)
        .ok_or(Error::InvalidTimeline)?;
    let duration = i64_value(object, "duration")?.or(i64_value(object, "pkt_duration")?);
    Ok(FrameRecord {
        pts,
        duration,
        width: u32_value(object, "width")?,
        height: u32_value(object, "height")?,
        pixel_format: string_value(object, "pix_fmt")?,
        interlaced: u32_value(object, "interlaced_frame")?
            .map(|value| match value {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(Error::MalformedProbe),
            })
            .transpose()?,
        sample_count: u32_value(object, "nb_samples")?
            .map(|value| usize::try_from(value).map_err(|_| Error::MalformedProbe))
            .transpose()?,
    })
}

fn validate_timeline(records: &[FrameRecord], count: usize) -> Result<(), Error> {
    if records.len() < count {
        return Err(Error::MissingFrames);
    }
    for pair in records.windows(2) {
        if pair[1].pts <= pair[0].pts {
            return Err(Error::InvalidTimeline);
        }
    }
    for index in 0..count {
        duration_ticks(records, index)?;
    }
    Ok(())
}

fn validate_pts_order(records: &[FrameRecord]) -> Result<(), Error> {
    if records.windows(2).any(|pair| pair[1].pts <= pair[0].pts) {
        Err(Error::InvalidTimeline)
    } else {
        Ok(())
    }
}

fn resolved_count(
    records: &[FrameRecord],
    requested: usize,
    requirement: CountRequirement,
    end_of_stream: bool,
) -> Result<usize, Error> {
    if records.len() < requested && !end_of_stream {
        return Err(Error::IncompleteFrameMetadata);
    }
    match requirement {
        CountRequirement::Exact if records.len() < requested => Err(Error::MissingFrames),
        CountRequirement::Exact => Ok(requested),
        CountRequirement::UpTo => match records.len().min(requested) {
            0 => Err(Error::MissingFrames),
            count => Ok(count),
        },
        CountRequirement::CursorUpTo => Ok(records.len().min(requested)),
    }
}

fn duration_ticks(records: &[FrameRecord], index: usize) -> Result<u64, Error> {
    let record = records.get(index).ok_or(Error::MissingFrames)?;
    if let Some(duration) = record.duration.filter(|duration| *duration > 0) {
        return u64::try_from(duration).map_err(|_| Error::InvalidTimeline);
    }
    let next = records.get(index + 1).ok_or(Error::InvalidTimeline)?;
    let duration = next
        .pts
        .checked_sub(record.pts)
        .ok_or(Error::InvalidTimeline)?;
    u64::try_from(duration)
        .ok()
        .filter(|duration| *duration > 0)
        .ok_or(Error::InvalidTimeline)
}

fn validate_audio_continuity(
    current: &FrameRecord,
    next: &FrameRecord,
    samples: usize,
    sample_rate: u32,
    time_base: TimeBase,
) -> Result<(), Error> {
    let delta = next
        .pts
        .checked_sub(current.pts)
        .ok_or(Error::InvalidTimeline)?;
    let left = i128::from(delta)
        .checked_mul(i128::from(time_base.numerator()))
        .and_then(|value| value.checked_mul(i128::from(sample_rate)))
        .ok_or(Error::InvalidTimeline)?;
    let right = i128::try_from(samples)
        .ok()
        .and_then(|value| value.checked_mul(i128::from(time_base.denominator())))
        .ok_or(Error::InvalidTimeline)?;
    if left == right {
        Ok(())
    } else {
        Err(Error::InvalidTimeline)
    }
}

fn validate_audio_span(
    record: &FrameRecord,
    samples: usize,
    sample_rate: u32,
    time_base: TimeBase,
) -> Result<(), Error> {
    let duration = record.duration.filter(|duration| *duration > 0);
    let Some(duration) = duration else {
        return Ok(());
    };
    let left = i128::from(duration)
        .checked_mul(i128::from(time_base.numerator()))
        .and_then(|value| value.checked_mul(i128::from(sample_rate)))
        .ok_or(Error::InvalidTimeline)?;
    let right = i128::try_from(samples)
        .ok()
        .and_then(|value| value.checked_mul(i128::from(time_base.denominator())))
        .ok_or(Error::InvalidTimeline)?;
    if left == right {
        Ok(())
    } else {
        Err(Error::InvalidTimeline)
    }
}

fn media_timing(
    pts: i64,
    duration_ticks: u64,
    time_base: TimeBase,
    clock_domain: ClockDomainId,
    sequence: usize,
) -> Result<MediaTiming, Error> {
    let original = OriginalTimestamp::new(MediaTimestamp::new(pts), time_base);
    let (normalized, duration) = normalized_interval(pts, duration_ticks, time_base)?;
    let sequence = u64::try_from(sequence).map_err(|_| Error::InvalidTimeline)?;
    MediaTiming::new(
        original,
        normalized,
        duration,
        clock_domain,
        SequenceNumber::new(sequence),
    )
    .map_err(|_| Error::InvalidTimeline)
}

fn normalized_interval(
    pts: i64,
    duration_ticks: u64,
    time_base: TimeBase,
) -> Result<(NormalizedTimestamp, NormalizedDuration), Error> {
    let duration_ticks = i64::try_from(duration_ticks).map_err(|_| Error::InvalidTimeline)?;
    let endpoint = pts
        .checked_add(duration_ticks)
        .ok_or(Error::InvalidTimeline)?;
    let start = OriginalTimestamp::new(MediaTimestamp::new(pts), time_base)
        .normalize()
        .map_err(|_| Error::InvalidTimeline)?;
    let end = OriginalTimestamp::new(MediaTimestamp::new(endpoint), time_base)
        .normalize()
        .map_err(|_| Error::InvalidTimeline)?;
    let duration = end
        .as_nanos()
        .checked_sub(start.as_nanos())
        .and_then(|duration| u64::try_from(duration).ok())
        .ok_or(Error::InvalidTimeline)?;
    let duration = NormalizedDuration::from_nanos(duration).map_err(|_| Error::InvalidTimeline)?;
    Ok((start, duration))
}

fn map_layout(
    raw_layout: Option<&str>,
    channels: u32,
) -> Result<(&'static str, ChannelLayout), Error> {
    let name = match (raw_layout, channels) {
        (Some("mono") | None, 1) => "mono",
        (Some("stereo") | None, 2) => "stereo",
        (Some("2.1"), 3) => "2.1",
        (Some("3.0"), 3) => "3.0",
        (Some("3.1"), 4) => "3.1",
        (Some("quad(side)"), 4) => "quad(side)",
        (Some("5.0(side)"), 5) => "5.0(side)",
        (Some("5.1(side)"), 6) => "5.1(side)",
        _ => return Err(Error::Unsupported(Unsupported::AudioLayout)),
    };
    let channels = match name {
        "mono" => vec![Channel::Mono],
        "stereo" => vec![Channel::Left, Channel::Right],
        "2.1" => vec![Channel::Left, Channel::Right, Channel::LowFrequency],
        "3.0" => vec![Channel::Left, Channel::Right, Channel::Center],
        "3.1" => vec![
            Channel::Left,
            Channel::Right,
            Channel::Center,
            Channel::LowFrequency,
        ],
        "quad(side)" => vec![
            Channel::Left,
            Channel::Right,
            Channel::LeftSurround,
            Channel::RightSurround,
        ],
        "5.0(side)" => vec![
            Channel::Left,
            Channel::Right,
            Channel::Center,
            Channel::LeftSurround,
            Channel::RightSurround,
        ],
        "5.1(side)" => vec![
            Channel::Left,
            Channel::Right,
            Channel::Center,
            Channel::LowFrequency,
            Channel::LeftSurround,
            Channel::RightSurround,
        ],
        _ => return Err(Error::Unsupported(Unsupported::AudioLayout)),
    };
    Ok((
        name,
        ChannelLayout::new(channels).ok_or(Error::FrameConstruction)?,
    ))
}

fn mapped_video_metadata(stream: &StreamInfo) -> Option<VideoFrameMetadata> {
    let primaries = map_primaries(stream.color_primaries.as_deref()?)?;
    let transfer = map_transfer(stream.color_transfer.as_deref()?)?;
    Some(VideoFrameMetadata::new(
        ColorMetadata {
            primaries,
            transfer,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    ))
}

fn map_primaries(value: &str) -> Option<ColorPrimaries> {
    match value {
        "bt709" => Some(ColorPrimaries::Bt709),
        "bt470bg" | "smpte170m" | "smpte240m" => Some(ColorPrimaries::Bt601),
        "bt2020" => Some(ColorPrimaries::Bt2020),
        "smpte432" => Some(ColorPrimaries::DisplayP3),
        _ => None,
    }
}

fn map_transfer(value: &str) -> Option<TransferFunction> {
    match value {
        "linear" => Some(TransferFunction::Linear),
        "iec61966-2-1" => Some(TransferFunction::Srgb),
        "bt709" | "smpte170m" | "bt2020-10" | "bt2020-12" => Some(TransferFunction::Bt709),
        _ => None,
    }
}

fn is_hdr_transfer(value: &str) -> bool {
    matches!(value, "smpte2084" | "arib-std-b67")
}

fn supported_non_alpha_pixel_format(value: &str) -> bool {
    matches!(
        value,
        "yuv420p"
            | "yuv422p"
            | "yuv444p"
            | "yuv420p10le"
            | "yuv422p10le"
            | "yuv444p10le"
            | "yuv420p12le"
            | "yuv422p12le"
            | "yuv444p12le"
            | "nv12"
            | "p010le"
            | "rgb24"
            | "bgr24"
            | "gbrp"
            | "gbrp10le"
            | "gray"
            | "gray10le"
            | "gray12le"
    )
}

fn source_has_alpha(value: &str) -> bool {
    value.starts_with("yuva")
        || value.starts_with("gbrap")
        || matches!(value, "rgba" | "bgra" | "argb" | "abgr" | "ya8" | "ya16le")
}

fn check_dimension(actual: u32, maximum: u32, kind: LimitKind) -> Result<(), Error> {
    if actual == 0 {
        return Err(Error::MalformedProbe);
    }
    if actual > maximum {
        return Err(Error::LimitExceeded {
            kind,
            actual: u64::from(actual),
            maximum: u64::from(maximum),
        });
    }
    Ok(())
}

fn decoded_limit(actual: u64, maximum: usize) -> Error {
    Error::LimitExceeded {
        kind: LimitKind::DecodedBytes,
        actual,
        maximum: u64::try_from(maximum).unwrap_or(u64::MAX),
    }
}

fn string_value(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Error> {
    object
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(Error::MalformedProbe)
        })
        .transpose()
}

fn i64_value(object: &Map<String, Value>, key: &str) -> Result<Option<i64>, Error> {
    object
        .get(key)
        .map(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .ok_or(Error::MalformedProbe)
        })
        .transpose()
}

fn u32_value(object: &Map<String, Value>, key: &str) -> Result<Option<u32>, Error> {
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(Error::MalformedProbe)
        })
        .transpose()
}

fn u64_value(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, Error> {
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .ok_or(Error::MalformedProbe)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroU128;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    fn record(pts: i64, duration: Option<i64>, samples: Option<usize>) -> FrameRecord {
        FrameRecord {
            pts,
            duration,
            width: None,
            height: None,
            pixel_format: None,
            interlaced: None,
            sample_count: samples,
        }
    }

    #[test]
    fn timeline_uses_positive_duration_or_next_pts_and_rejects_regression() {
        let records = [record(0, None, None), record(40, Some(40), None)];
        assert_eq!(duration_ticks(&records, 0), Ok(40));
        assert_eq!(validate_timeline(&records, 2), Ok(()));
        let regression = [record(40, Some(1), None), record(39, Some(1), None)];
        assert_eq!(
            validate_timeline(&regression, 2),
            Err(Error::InvalidTimeline)
        );
        assert_eq!(
            normalized_interval(i64::MAX, 1, TimeBase::new(u32::MAX, 1).unwrap()),
            Err(Error::InvalidTimeline)
        );
    }

    #[test]
    fn count_resolution_preserves_exact_requests_and_accepts_nonempty_prefixes() {
        let records = [record(0, Some(40), None), record(40, Some(40), None)];
        assert_eq!(
            resolved_count(&records, 2, CountRequirement::Exact, false),
            Ok(2)
        );
        assert_eq!(
            resolved_count(&records, 1, CountRequirement::Exact, false),
            Ok(1)
        );
        assert_eq!(
            resolved_count(&records, 3, CountRequirement::Exact, true),
            Err(Error::MissingFrames)
        );
        assert_eq!(
            resolved_count(&records, 3, CountRequirement::UpTo, true),
            Ok(2)
        );
        assert_eq!(
            resolved_count(&[], 3, CountRequirement::UpTo, true),
            Err(Error::MissingFrames)
        );
        assert_eq!(
            resolved_count(&records, 3, CountRequirement::UpTo, false),
            Err(Error::IncompleteFrameMetadata)
        );
        assert_eq!(
            resolved_count(&[], 3, CountRequirement::CursorUpTo, true),
            Ok(0)
        );
    }

    #[test]
    fn frame_probe_uses_packet_slack_and_only_proves_eos_below_budget() {
        let path = Path::new("movie.nut");
        let args = frame_probe_args(path, 2, 66);
        let args = args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|argument| argument == "-count_packets"));
        assert!(args.iter().any(|argument| argument == "-show_streams"));
        assert!(args.iter().any(|argument| argument == "%+#66"));
        assert!(
            args.iter()
                .any(|argument| argument.contains("stream=index,nb_read_packets"))
        );

        let exhausted = br#"{
            "frames":[
                {"stream_index":2,"best_effort_timestamp":0},
                {"stream_index":2,"best_effort_timestamp":1}
            ],
            "streams":[{"index":2,"nb_read_packets":"66"}]
        }"#;
        let metadata = parse_frame_records(exhausted, 2, 1, 66).unwrap();
        assert_eq!(metadata.records.len(), 1);
        assert!(!metadata.end_of_stream);

        let proven_eos = exhausted
            .windows(b"66".len())
            .position(|window| window == b"66")
            .map(|position| {
                let mut json = exhausted.to_vec();
                json[position..position + 2].copy_from_slice(b"65");
                json
            })
            .unwrap();
        assert!(
            parse_frame_records(&proven_eos, 2, 1, 66)
                .unwrap()
                .end_of_stream
        );
    }

    #[test]
    fn video_window_args_trim_ordinals_without_seeking_or_rebasing_pts() {
        let args = video_decode_args(Path::new("movie.nut"), 3, 7, 4)
            .unwrap()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.iter()
                .any(|argument| argument == "trim=start_frame=7:end_frame=11")
        );
        assert!(args.windows(2).any(|pair| pair == ["-frames:v", "4"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-fps_mode", "passthrough"])
        );
        assert!(!args.iter().any(|argument| argument == "-ss"));
        assert!(!args.iter().any(|argument| argument.contains("setpts")));
    }

    #[test]
    fn audio_window_args_seek_before_input_and_bound_the_correction_trim() {
        let args = audio_decode_args(
            Path::new("movie.nut"),
            3,
            AudioSeek {
                input_microseconds: Some(12_345_678),
                expected_first_pts: Some(330_750),
                correction_samples: 44_032,
            },
            3_072,
            "stereo",
        )
        .unwrap()
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-ss", "12345678us"]));
        assert!(
            args.iter().position(|argument| argument == "-ss")
                < args.iter().position(|argument| argument == "-i")
        );
        assert!(args.iter().any(|argument| argument == "-copyts"));
        let graph = args
            .windows(2)
            .find(|pair| pair[0] == "-filter_complex")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(graph.contains("[0:3]asplit=2[main][verify]"));
        assert!(graph.contains("[verify]atrim=end_sample=1,ashowinfo,anullsink"));
        assert!(graph.contains("[main]atrim=start_sample=44032:end_sample=47104[out]"));
        assert!(args.windows(2).any(|pair| pair == ["-map", "[out]"]));
        assert!(!args.iter().any(|argument| argument == "-frames:a"));
        assert!(!args.iter().any(|argument| argument.contains("asetpts")));
    }

    #[test]
    fn leading_audio_window_omits_seek_and_diagnostic_filter() {
        let args = audio_decode_args(
            Path::new("movie.nut"),
            3,
            AudioSeek {
                input_microseconds: None,
                expected_first_pts: None,
                correction_samples: 0,
            },
            3_072,
            "stereo",
        )
        .unwrap()
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(!args.iter().any(|argument| argument == "-ss"));
        assert!(!args.iter().any(|argument| argument == "-copyts"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-af", "atrim=start_sample=0:end_sample=3072"])
        );
        assert!(!args.iter().any(|argument| argument.contains("ashowinfo")));
    }

    #[test]
    fn audio_seek_uses_bounded_metadata_anchors() {
        let sample_rate = 44_100;
        let base = i64::from(sample_rate) * 5;
        let records = (0..60)
            .map(|index| record(base + i64::from(index * 1_024), Some(1_024), Some(1_024)))
            .collect::<Vec<_>>();
        let sample_positions = (0..=60).map(|index| index * 1_024).collect::<Vec<_>>();
        let seeks = audio_seek_plan(
            AudioSeekTimeline {
                records: &records,
                sample_positions: &sample_positions,
                start: 50,
                start_sample: 50 * 1_024,
                sample_rate,
                time_base: TimeBase::new(1, sample_rate).unwrap(),
                input_start_microseconds: Some(5_000_000),
            },
            crate::Limits {
                max_audio_samples: 44_100,
                ..crate::Limits::default()
            },
            0,
            2,
        )
        .unwrap();
        assert_eq!(seeks.len(), AUDIO_SEEK_CANDIDATES);
        assert_eq!(
            seeks.first(),
            Some(&AudioSeek {
                input_microseconds: Some(162_539),
                expected_first_pts: Some(base + 7 * 1_024),
                correction_samples: 43 * 1_024,
            })
        );
        assert_eq!(
            seeks.last(),
            Some(&AudioSeek {
                input_microseconds: Some(1_160_997),
                expected_first_pts: Some(base + 50 * 1_024),
                correction_samples: 0,
            })
        );
    }

    #[test]
    fn audio_seek_prefers_bounded_from_start_and_rejects_deep_negative_pts() {
        let sample_rate = 48_000;
        let records = (0..12)
            .map(|index| record(-24_000 + i64::from(index * 4_800), Some(4_800), Some(4_800)))
            .collect::<Vec<_>>();
        let sample_positions = (0..=12).map(|index| index * 4_800).collect::<Vec<_>>();
        assert_eq!(
            audio_seek_plan(
                AudioSeekTimeline {
                    records: &records,
                    sample_positions: &sample_positions,
                    start: 3,
                    start_sample: 14_400,
                    sample_rate,
                    time_base: TimeBase::new(1, sample_rate).unwrap(),
                    input_start_microseconds: Some(-500_000),
                },
                crate::Limits {
                    max_audio_samples: 14_400,
                    ..crate::Limits::default()
                },
                0,
                2,
            ),
            Ok(vec![AudioSeek {
                input_microseconds: None,
                expected_first_pts: None,
                correction_samples: 14_400,
            }])
        );
        assert_eq!(
            audio_seek_plan(
                AudioSeekTimeline {
                    records: &records,
                    sample_positions: &sample_positions,
                    start: 6,
                    start_sample: 28_800,
                    sample_rate,
                    time_base: TimeBase::new(1, sample_rate).unwrap(),
                    input_start_microseconds: Some(-500_000),
                },
                crate::Limits {
                    max_audio_samples: 20_000,
                    ..crate::Limits::default()
                },
                0,
                2,
            ),
            Err(Error::Unsupported(Unsupported::NegativeAudioAnchor))
        );
    }

    #[test]
    fn audio_seek_retries_ordered_anchors_with_one_deadline_and_caller_bounds() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.nut");
        fs::write(&source_path, b"test media").unwrap();
        let executable = std::env::current_exe().unwrap();
        let adapter = Adapter::new(crate::Config {
            ffmpeg: crate::Executable::Explicit(executable.clone()),
            ffprobe: crate::Executable::Explicit(executable),
            allowed_root: Some(directory.path().to_owned()),
            limits: crate::Limits {
                max_audio_samples: 100_000,
                max_total_decoded_bytes: 800_000,
                decode_timeout: Duration::from_millis(300),
                ..crate::Limits::default()
            },
        })
        .unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(70).unwrap());

        for (name, max_page_samples, max_page_decoded_bytes) in [
            ("sample-bound.log", 3_072, 800_000),
            ("byte-bound.log", 100_000, 24_576),
        ] {
            let state_path = directory.path().join(name);
            crate::process::with_test_command_shim("anchor-retry", &state_path, || {
                let mut cursor = adapter
                    .open_local_audio(&source_path, clock, StreamSelector::Best)
                    .unwrap();
                assert_eq!(
                    cursor
                        .skip_complete_blocks_to_sample_bounded(10 * 1_024, 10)
                        .unwrap(),
                    AudioCursorPosition {
                        skipped_blocks: 10,
                        skipped_samples: 10 * 1_024,
                        next_block: 10,
                        next_sample: 10 * 1_024,
                        end_of_stream: false,
                    }
                );

                let page = cursor
                    .decode_up_to_bounded(NonZeroU32::MIN, max_page_samples, max_page_decoded_bytes)
                    .unwrap();
                assert_eq!(page.blocks.len(), 1);
                assert_eq!(page.blocks[0].timing().sequence().get(), 10);
            });

            let log = fs::read_to_string(state_path).unwrap();
            let attempts = log
                .lines()
                .filter(|line| line.contains(" kind=decode "))
                .collect::<Vec<_>>();
            assert_eq!(attempts.len(), 2, "runner log:\n{log}");
            assert!(attempts[0].contains("seek=170666us"));
            assert!(attempts[1].contains("seek=192000us"));
            let first_timeout = runner_timeout_nanos(attempts[0]);
            let second_timeout = runner_timeout_nanos(attempts[1]);
            assert!(first_timeout <= Duration::from_millis(300).as_nanos());
            assert!(
                second_timeout + Duration::from_millis(50).as_nanos() < first_timeout,
                "retry did not consume the shared deadline: {attempts:?}"
            );
        }
    }

    #[test]
    fn from_start_process_failure_is_exact_and_cursor_retry_is_transactional() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.nut");
        let state_path = directory.path().join("runner.log");
        fs::write(&source_path, b"test media").unwrap();
        let executable = std::env::current_exe().unwrap();
        let adapter = Adapter::new(crate::Config {
            ffmpeg: crate::Executable::Explicit(executable.clone()),
            ffprobe: crate::Executable::Explicit(executable),
            allowed_root: Some(directory.path().to_owned()),
            limits: crate::Limits {
                max_audio_samples: 100_000,
                max_total_decoded_bytes: 800_000,
                decode_timeout: Duration::from_millis(300),
                ..crate::Limits::default()
            },
        })
        .unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(71).unwrap());

        crate::process::with_test_command_shim("from-start-failure", &state_path, || {
            let mut cursor = adapter
                .open_local_audio(&source_path, clock, StreamSelector::Best)
                .unwrap();
            cursor
                .skip_complete_blocks_to_sample_bounded(2 * 1_024, 2)
                .unwrap();

            assert_eq!(
                cursor.decode_up_to_bounded(NonZeroU32::MIN, 3_072, 24_576),
                Err(Error::ProcessFailed {
                    tool: Tool::Ffmpeg,
                    status: Some(23),
                    stderr: "from-start sentinel".to_owned(),
                })
            );
            assert_eq!(cursor.ordinal, 2);
            assert_eq!(cursor.absolute_sample_position, 2 * 1_024);
            assert!(!cursor.end_of_stream);

            let recovered = cursor
                .decode_up_to_bounded(NonZeroU32::MIN, 3_072, 24_576)
                .unwrap();
            assert_eq!(recovered.blocks.len(), 1);
            assert_eq!(recovered.blocks[0].timing().sequence().get(), 2);
            assert!(!recovered.end_of_stream);
        });

        let log = fs::read_to_string(state_path).unwrap();
        let attempts = log
            .lines()
            .filter(|line| line.contains(" kind=decode "))
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2, "runner log:\n{log}");
        assert!(attempts.iter().all(|attempt| attempt.contains("seek=none")));
    }

    #[test]
    fn audio_metadata_index_resumes_with_constant_packet_budgets() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.nut");
        let state_path = directory.path().join("runner.log");
        fs::write(&source_path, b"test media").unwrap();
        let executable = std::env::current_exe().unwrap();
        let adapter = Adapter::new(crate::Config {
            ffmpeg: crate::Executable::Explicit(executable.clone()),
            ffprobe: crate::Executable::Explicit(executable),
            allowed_root: Some(directory.path().to_owned()),
            limits: crate::Limits {
                max_audio_blocks: 32,
                max_audio_metadata_records: 64,
                max_audio_metadata_bytes: 16 * 1_024,
                max_audio_metadata_checkpoints: 4,
                audio_metadata_checkpoint_interval: 16,
                max_audio_metadata_resume_attempts: 2,
                ..crate::Limits::default()
            },
        })
        .unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(72).unwrap());

        crate::process::with_test_command_shim("metadata-index", &state_path, || {
            let mut cursor = adapter
                .open_local_audio(&source_path, clock, StreamSelector::Best)
                .unwrap();
            cursor
                .skip_complete_blocks_to_sample_bounded(80 * 1_024, 80)
                .unwrap();
            cursor
                .skip_complete_blocks_to_sample_bounded(160 * 1_024, 80)
                .unwrap();
            let telemetry = cursor.metadata_index_telemetry();
            assert_eq!(telemetry.origin_probe_calls, 1);
            assert_eq!(telemetry.resumed_probe_calls, 3);
            assert!(telemetry.peak_packet_budget <= 113);
            assert!(telemetry.reused_records > 0);
            assert!(telemetry.retained_records <= 64);
            assert!(telemetry.retained_bytes <= 16 * 1_024);
            assert!(telemetry.retained_checkpoints <= 4);
        });

        let frames = fs::read_to_string(state_path)
            .unwrap()
            .lines()
            .filter(|line| line.contains(" kind=frames "))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let intervals = frames
            .iter()
            .map(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("interval="))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            intervals,
            ["%+#113", "1.024000%+#97", "1.706666%+#113", "2.730666%+#97"],
            "unexpected metadata discovery commands: {frames:?}"
        );
    }

    #[test]
    fn audio_metadata_eviction_keeps_deep_position_exact_and_source_change_invalidates() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.nut");
        let state_path = directory.path().join("runner.log");
        fs::write(&source_path, b"test media").unwrap();
        let executable = std::env::current_exe().unwrap();
        let adapter = Adapter::new(crate::Config {
            ffmpeg: crate::Executable::Explicit(executable.clone()),
            ffprobe: crate::Executable::Explicit(executable),
            allowed_root: Some(directory.path().to_owned()),
            limits: crate::Limits {
                max_audio_blocks: 32,
                max_audio_metadata_records: 42,
                max_audio_metadata_bytes: 16 * 1_024,
                max_audio_metadata_checkpoints: 2,
                audio_metadata_checkpoint_interval: 8,
                max_audio_metadata_resume_attempts: 2,
                ..crate::Limits::default()
            },
        })
        .unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(73).unwrap());

        crate::process::with_test_command_shim("metadata-index", &state_path, || {
            let mut cursor = adapter
                .open_local_audio(&source_path, clock, StreamSelector::Best)
                .unwrap();
            let position = cursor
                .skip_complete_blocks_to_sample_bounded(200 * 1_024, 200)
                .unwrap();
            assert_eq!(position.next_block, 200);
            assert_eq!(position.next_sample, 200 * 1_024);
            let telemetry = cursor.metadata_index_telemetry();
            assert!(telemetry.evicted_records > 0);
            assert!(telemetry.evicted_checkpoints > 0);
            assert!(telemetry.retained_records <= 42);
            assert!(telemetry.retained_checkpoints <= 2);

            fs::write(&source_path, b"changed media identity").unwrap();
            assert_eq!(
                cursor.skip_complete_blocks_to_sample_bounded(201 * 1_024, 1),
                Err(Error::SourceChanged)
            );
            assert_eq!(cursor.ordinal, 200);
            assert_eq!(cursor.absolute_sample_position, 200 * 1_024);
            assert_eq!(cursor.metadata_index_telemetry().invalidations, 1);
            assert_eq!(cursor.metadata_index_telemetry().retained_records, 0);
        });
    }

    #[test]
    fn audio_metadata_timeout_leaves_index_and_cursor_retryable() {
        let directory = tempdir().unwrap();
        let source_path = directory.path().join("source.nut");
        let state_path = directory.path().join("runner.log");
        fs::write(&source_path, b"test media").unwrap();
        let executable = std::env::current_exe().unwrap();
        let adapter = Adapter::new(crate::Config {
            ffmpeg: crate::Executable::Explicit(executable.clone()),
            ffprobe: crate::Executable::Explicit(executable),
            allowed_root: Some(directory.path().to_owned()),
            limits: crate::Limits {
                frame_metadata_timeout: Duration::from_millis(50),
                kill_timeout: Duration::from_millis(50),
                ..crate::Limits::default()
            },
        })
        .unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(74).unwrap());

        crate::process::with_test_command_shim("metadata-timeout-once", &state_path, || {
            let mut cursor = adapter
                .open_local_audio(&source_path, clock, StreamSelector::Best)
                .unwrap();
            assert_eq!(
                cursor.skip_complete_blocks_to_sample_bounded(10 * 1_024, 10),
                Err(Error::ProcessTimedOut {
                    tool: Tool::Ffprobe
                })
            );
            assert_eq!(cursor.ordinal, 0);
            assert_eq!(cursor.absolute_sample_position, 0);
            assert_eq!(cursor.metadata_index_telemetry().retained_records, 0);

            let recovered = cursor
                .skip_complete_blocks_to_sample_bounded(10 * 1_024, 10)
                .unwrap();
            assert_eq!(recovered.next_block, 10);
            assert_eq!(recovered.next_sample, 10 * 1_024);
            let telemetry = cursor.metadata_index_telemetry();
            assert_eq!(telemetry.probe_calls, 2);
            assert_eq!(telemetry.origin_probe_calls, 2);
            assert!(telemetry.retained_records >= 11);
        });
    }

    fn runner_timeout_nanos(line: &str) -> u128 {
        line.split_whitespace()
            .find_map(|field| field.strip_prefix("timeout_nanos="))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn layouts_have_exact_semantic_order_and_ambiguous_surround_is_rejected() {
        let (_, stereo) = map_layout(None, 2).unwrap();
        assert_eq!(stereo.channels(), &[Channel::Left, Channel::Right]);
        let (_, surround) = map_layout(Some("5.1(side)"), 6).unwrap();
        assert_eq!(
            surround.channels(),
            &[
                Channel::Left,
                Channel::Right,
                Channel::Center,
                Channel::LowFrequency,
                Channel::LeftSurround,
                Channel::RightSurround
            ]
        );
        assert_eq!(
            map_layout(Some("5.1"), 6),
            Err(Error::Unsupported(Unsupported::AudioLayout))
        );
    }

    #[test]
    fn color_mapping_is_explicit_and_hdr_is_not_mapped_to_sdr() {
        assert_eq!(map_primaries("bt709"), Some(ColorPrimaries::Bt709));
        assert_eq!(map_primaries("future"), None);
        assert_eq!(map_transfer("bt709"), Some(TransferFunction::Bt709));
        assert_eq!(map_transfer("smpte240m"), None);
        assert_eq!(map_transfer("smpte2084"), None);
        assert!(is_hdr_transfer("arib-std-b67"));
    }

    #[test]
    fn fractional_intervals_normalize_endpoints_without_drift() {
        let time_base = TimeBase::new(1, 48_000).unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let timings = [0, 1024, 2048]
            .into_iter()
            .enumerate()
            .map(|(sequence, pts)| media_timing(pts, 1024, time_base, clock, sequence).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            timings
                .iter()
                .map(|timing| timing.presentation_timestamp().as_nanos())
                .collect::<Vec<_>>(),
            [0, 21_333_333, 42_666_666]
        );
        assert_eq!(
            timings
                .iter()
                .map(|timing| timing.duration().as_nanos())
                .collect::<Vec<_>>(),
            [21_333_333, 21_333_333, 21_333_334]
        );
        for pair in timings.windows(2) {
            assert_eq!(
                pair[0].presentation_timestamp().as_nanos()
                    + i64::try_from(pair[0].duration().as_nanos()).unwrap(),
                pair[1].presentation_timestamp().as_nanos()
            );
        }
        assert_eq!(
            timings
                .iter()
                .map(|timing| timing.duration().as_nanos())
                .sum::<u64>(),
            64_000_000
        );
    }
}
