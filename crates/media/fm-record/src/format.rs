use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use fm_frame::{EncodedPacket, MediaTiming, OriginalTimestamp};
use fm_types::{Channel, PixelFormat};

use crate::{
    ActionReceiptId, RecordEvent, RecorderConfig, RecorderError, SegmentPolicy,
    coordinator::DurableWriter,
};

pub(crate) const MANIFEST_NAME: &str = "manifest.fmr";
const MAGIC: [u8; 4] = *b"FMRC";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 24;
pub(crate) const MAX_ENCODED_FRAME_PAYLOAD: usize =
    EncodedPacket::MAX_PAYLOAD_BYTES + u16::MAX as usize + 4096;
pub(crate) const MAX_MANIFEST_PAYLOAD: usize = 256;

pub(crate) const AUDIO: u8 = 1;
pub(crate) const VIDEO: u8 = 2;
pub(crate) const TIMED: u8 = 3;
pub(crate) const DISCONTINUITY: u8 = 4;
const START: u8 = 16;
const STOP: u8 = 17;
const SEGMENT_OPEN: u8 = 18;
const SEGMENT_CLOSE: u8 = 19;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionKind {
    Start,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManifestEntry {
    Start {
        receipt: ActionReceiptId,
        config: RecorderConfig,
        index: u64,
    },
    Stop {
        receipt: ActionReceiptId,
    },
    SegmentOpen {
        index: u64,
    },
    SegmentClose {
        index: u64,
        frames: u64,
        bytes: u64,
    },
}

#[derive(Debug)]
pub(crate) struct ScannedFrame {
    pub kind: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ScanResult {
    pub frames: Vec<ScannedFrame>,
    pub valid_bytes: u64,
    pub truncated_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct SegmentScan {
    pub records: u64,
    pub media_frames: u64,
    pub valid_bytes: u64,
    pub truncated_bytes: u64,
}

pub(crate) fn segment_name(index: u64) -> String {
    format!("segment-{index:020}.fms")
}

pub(crate) fn segment_path(directory: &Path, index: u64) -> PathBuf {
    directory.join(segment_name(index))
}

pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    name.strip_prefix("segment-")?
        .strip_suffix(".fms")?
        .parse()
        .ok()
}

pub(crate) fn encoded_event(event: &RecordEvent) -> Result<(u8, Vec<u8>), RecorderError> {
    validate_event(event)?;
    let mut output = Vec::with_capacity(event.payload_len().saturating_add(256));
    let kind = match event {
        RecordEvent::Audio { metadata, payload } => {
            encode_common(&mut output, &metadata.common)?;
            put_u32(&mut output, metadata.sample_rate.hertz());
            let channels = metadata.channels.channels();
            put_u16(
                &mut output,
                u16::try_from(channels.len())
                    .map_err(|_| RecorderError::FormatLimit("too many audio channels"))?,
            );
            for channel in channels {
                output.push(channel_code(*channel));
            }
            put_blob(&mut output, payload)?;
            AUDIO
        }
        RecordEvent::Video { metadata, payload } => {
            encode_common(&mut output, &metadata.common)?;
            put_u32(&mut output, metadata.dimensions.width());
            put_u32(&mut output, metadata.dimensions.height());
            output.push(pixel_format_code(metadata.pixel_format));
            put_blob(&mut output, payload)?;
            VIDEO
        }
        RecordEvent::Timed { metadata, payload } => {
            encode_common(&mut output, &metadata.common)?;
            put_text(&mut output, &metadata.content_type)?;
            put_blob(&mut output, payload)?;
            TIMED
        }
        RecordEvent::Discontinuity(discontinuity) => {
            match discontinuity.stream_id {
                Some(stream_id) => {
                    output.push(1);
                    put_u32(&mut output, stream_id.get().get());
                }
                None => output.push(0),
            }
            encode_timing(&mut output, discontinuity.timing);
            put_text(&mut output, &discontinuity.reason)?;
            DISCONTINUITY
        }
    };
    Ok((kind, output))
}

pub(crate) fn validate_event(event: &RecordEvent) -> Result<(), RecorderError> {
    let payload_len = match event {
        RecordEvent::Audio { metadata, payload } => checked_sum(&[
            common_len(&metadata.common)?,
            4,
            2,
            u16::try_from(metadata.channels.channels().len())
                .map(usize::from)
                .map_err(|_| RecorderError::FormatLimit("too many audio channels"))?,
            blob_len(payload.len())?,
        ])?,
        RecordEvent::Video { metadata, payload } => checked_sum(&[
            common_len(&metadata.common)?,
            4,
            4,
            1,
            blob_len(payload.len())?,
        ])?,
        RecordEvent::Timed { metadata, payload } => checked_sum(&[
            common_len(&metadata.common)?,
            text_len(&metadata.content_type)?,
            blob_len(payload.len())?,
        ])?,
        RecordEvent::Discontinuity(discontinuity) => checked_sum(&[
            1,
            usize::from(discontinuity.stream_id.is_some()) * 4,
            timing_len(discontinuity.timing),
            text_len(&discontinuity.reason)?,
        ])?,
    };
    if payload_len > MAX_ENCODED_FRAME_PAYLOAD {
        return Err(RecorderError::FormatLimit(
            "encoded event exceeds frame payload limit",
        ));
    }
    Ok(())
}

pub(crate) fn framed_len(payload_len: usize) -> Result<u64, RecorderError> {
    u64::try_from(HEADER_LEN)
        .ok()
        .and_then(|header| {
            u64::try_from(payload_len)
                .ok()
                .and_then(|len| header.checked_add(len))
        })
        .ok_or(RecorderError::FormatLimit("record length overflow"))
}

pub(crate) fn write_frame(
    writer: &mut dyn DurableWriter,
    kind: u8,
    payload: &[u8],
    maximum_payload: usize,
) -> io::Result<u64> {
    if payload.len() > maximum_payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "record payload exceeds format limit",
        ));
    }
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "record is too large"))?;
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = VERSION;
    header[5] = kind;
    header[8..16].copy_from_slice(&payload_len.to_le_bytes());
    header[16..20].copy_from_slice(&crc32(payload).to_le_bytes());
    let header_crc = crc32(&header[..20]);
    header[20..24].copy_from_slice(&header_crc.to_le_bytes());

    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.sync_all()?;
    Ok(u64::try_from(HEADER_LEN).unwrap_or(24) + payload_len)
}

pub(crate) fn encode_manifest(entry: ManifestEntry) -> (u8, Vec<u8>) {
    let mut output = Vec::with_capacity(64);
    let kind = match entry {
        ManifestEntry::Start {
            receipt,
            config,
            index,
        } => {
            put_u128(&mut output, receipt.get().get());
            put_u64(
                &mut output,
                u64::try_from(config.queue.max_events).unwrap_or(u64::MAX),
            );
            put_u64(
                &mut output,
                u64::try_from(config.queue.max_bytes).unwrap_or(u64::MAX),
            );
            put_option_u64(&mut output, config.segments.max_frames);
            put_option_u64(&mut output, config.segments.max_bytes);
            put_u64(&mut output, index);
            START
        }
        ManifestEntry::Stop { receipt } => {
            put_u128(&mut output, receipt.get().get());
            STOP
        }
        ManifestEntry::SegmentOpen { index } => {
            put_u64(&mut output, index);
            SEGMENT_OPEN
        }
        ManifestEntry::SegmentClose {
            index,
            frames,
            bytes,
        } => {
            put_u64(&mut output, index);
            put_u64(&mut output, frames);
            put_u64(&mut output, bytes);
            SEGMENT_CLOSE
        }
    };
    (kind, output)
}

pub(crate) fn decode_manifest(
    frame: &ScannedFrame,
    path: &Path,
) -> Result<ManifestEntry, RecorderError> {
    let mut cursor = Cursor::new(&frame.payload);
    let entry = match frame.kind {
        START => {
            let receipt = receipt(cursor.u128()?, path)?;
            let max_events = usize::try_from(cursor.u64()?)
                .map_err(|_| corrupt(path, "queue event limit does not fit this platform"))?;
            let max_bytes = usize::try_from(cursor.u64()?)
                .map_err(|_| corrupt(path, "queue byte limit does not fit this platform"))?;
            let config = RecorderConfig {
                queue: crate::QueueLimits {
                    max_events,
                    max_bytes,
                },
                segments: SegmentPolicy {
                    max_frames: cursor.option_u64()?,
                    max_bytes: cursor.option_u64()?,
                },
            };
            config
                .validate()
                .map_err(|_| corrupt(path, "invalid persisted recorder config"))?;
            ManifestEntry::Start {
                receipt,
                config,
                index: cursor.u64()?,
            }
        }
        STOP => ManifestEntry::Stop {
            receipt: receipt(cursor.u128()?, path)?,
        },
        SEGMENT_OPEN => ManifestEntry::SegmentOpen {
            index: cursor.u64()?,
        },
        SEGMENT_CLOSE => ManifestEntry::SegmentClose {
            index: cursor.u64()?,
            frames: cursor.u64()?,
            bytes: cursor.u64()?,
        },
        _ => return Err(corrupt(path, "unknown manifest record kind")),
    };
    if !cursor.is_empty() {
        return Err(corrupt(path, "manifest record has trailing bytes"));
    }
    Ok(entry)
}

pub(crate) fn scan_manifest(path: &Path) -> Result<ScanResult, RecorderError> {
    let scan = scan_file(path, MAX_MANIFEST_PAYLOAD, true, false)?;
    Ok(ScanResult {
        frames: scan.frames,
        valid_bytes: scan.valid_bytes,
        truncated_bytes: scan.truncated_bytes,
    })
}

pub(crate) fn scan_segment(path: &Path) -> Result<SegmentScan, RecorderError> {
    let scan = scan_file(path, MAX_ENCODED_FRAME_PAYLOAD, false, true)?;
    Ok(SegmentScan {
        records: scan.records,
        media_frames: scan.media_frames,
        valid_bytes: scan.valid_bytes,
        truncated_bytes: scan.truncated_bytes,
    })
}

struct RawScan {
    frames: Vec<ScannedFrame>,
    records: u64,
    media_frames: u64,
    valid_bytes: u64,
    truncated_bytes: u64,
}

fn scan_file(
    path: &Path,
    maximum_payload: usize,
    collect_payloads: bool,
    segment: bool,
) -> Result<RawScan, RecorderError> {
    let mut file = File::open(path).map_err(|error| RecorderError::io(path, error))?;
    let original_len = file
        .metadata()
        .map_err(|error| RecorderError::io(path, error))?
        .len();
    let mut frames = Vec::new();
    let mut records = 0u64;
    let mut media_frames = 0u64;
    let mut offset = 0u64;

    loop {
        let mut header = [0u8; HEADER_LEN];
        let read =
            read_up_to(&mut file, &mut header).map_err(|error| RecorderError::io(path, error))?;
        if read == 0 {
            break;
        }
        if read < HEADER_LEN {
            break;
        }
        if header[..4] != MAGIC || header[4] != VERSION || header[6] != 0 || header[7] != 0 {
            return Err(RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "invalid record header",
            });
        }
        let expected_header_crc = u32::from_le_bytes(header[20..24].try_into().unwrap_or([0; 4]));
        if crc32(&header[..20]) != expected_header_crc {
            return Err(RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "header checksum mismatch",
            });
        }
        let payload_len = u64::from_le_bytes(header[8..16].try_into().unwrap_or([0; 8]));
        if payload_len > u64::try_from(maximum_payload).unwrap_or(u64::MAX) {
            return Err(RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "record payload exceeds format limit",
            });
        }
        let payload_len_usize =
            usize::try_from(payload_len).map_err(|_| RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "record payload does not fit this platform",
            })?;
        if segment && !matches!(header[5], AUDIO | VIDEO | TIMED | DISCONTINUITY) {
            return Err(RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "unknown segment record kind",
            });
        }
        let Some((payload, actual_payload_crc)) =
            read_payload(&mut file, payload_len_usize, collect_payloads)
                .map_err(|error| RecorderError::io(path, error))?
        else {
            break;
        };
        let expected_payload_crc = u32::from_le_bytes(header[16..20].try_into().unwrap_or([0; 4]));
        if actual_payload_crc != expected_payload_crc {
            return Err(RecorderError::Corrupt {
                path: path.to_path_buf(),
                offset,
                reason: "payload checksum mismatch",
            });
        }
        if collect_payloads {
            frames.push(ScannedFrame {
                kind: header[5],
                payload,
            });
        }
        records = records.saturating_add(1);
        if segment && header[5] != DISCONTINUITY {
            media_frames = media_frames.saturating_add(1);
        }
        offset = offset
            .checked_add(u64::try_from(HEADER_LEN).unwrap_or(24))
            .and_then(|value| value.checked_add(payload_len))
            .ok_or(RecorderError::FormatLimit("file offset overflow"))?;
    }

    Ok(RawScan {
        frames,
        records,
        media_frames,
        valid_bytes: offset,
        truncated_bytes: original_len.saturating_sub(offset),
    })
}

pub(crate) fn truncate_file(path: &Path, length: u64) -> Result<(), RecorderError> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(length).and_then(|()| file.sync_all()))
        .map_err(|error| RecorderError::io(path, error))
}

fn read_payload(
    file: &mut File,
    payload_len: usize,
    collect: bool,
) -> io::Result<Option<(Vec<u8>, u32)>> {
    let mut payload = if collect {
        Vec::with_capacity(payload_len)
    } else {
        Vec::new()
    };
    let mut remaining = payload_len;
    let mut crc = u32::MAX;
    let mut buffer = [0u8; 8 * 1024];
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len());
        let read = file.read(&mut buffer[..chunk_len])?;
        if read == 0 {
            return Ok(None);
        }
        crc = crc32_update(crc, &buffer[..read]);
        if collect {
            payload.extend_from_slice(&buffer[..read]);
        }
        remaining -= read;
    }
    Ok(Some((payload, !crc)))
}

fn read_up_to(reader: &mut File, mut buffer: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => break,
            Ok(read) => {
                total += read;
                buffer = &mut buffer[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

fn encode_common(output: &mut Vec<u8>, common: &crate::PacketCommon) -> Result<(), RecorderError> {
    put_text(output, common.codec().as_str())?;
    put_u64(output, common.config_generation().get().get());
    put_u32(output, common.stream_id().get().get());
    match common.channel_index() {
        Some(index) => {
            output.push(1);
            put_u16(output, index);
        }
        None => output.push(0),
    }
    encode_timing(output, common.timing());
    encode_timestamp(output, common.decode_timestamp());
    output.push(common.flags().bits());
    Ok(())
}

fn common_len(common: &crate::PacketCommon) -> Result<usize, RecorderError> {
    checked_sum(&[
        text_len(common.codec().as_str())?,
        8,
        4,
        1,
        usize::from(common.channel_index().is_some()) * 2,
        timing_len(common.timing()),
        16,
        1,
    ])
}

fn timing_len(timing: MediaTiming) -> usize {
    16 + 8
        + 8
        + 16
        + 8
        + 1
        + 1
        + usize::from(timing.capture_timestamp().is_some()) * 16
        + 1
        + usize::from(timing.timecode().is_some()) * 4
}

fn text_len(value: &str) -> Result<usize, RecorderError> {
    u16::try_from(value.len()).map_err(|_| RecorderError::FormatLimit("text too long"))?;
    value
        .len()
        .checked_add(2)
        .ok_or(RecorderError::FormatLimit("record length overflow"))
}

fn blob_len(length: usize) -> Result<usize, RecorderError> {
    u64::try_from(length).map_err(|_| RecorderError::FormatLimit("payload too large"))?;
    length
        .checked_add(8)
        .ok_or(RecorderError::FormatLimit("record length overflow"))
}

fn checked_sum(parts: &[usize]) -> Result<usize, RecorderError> {
    parts.iter().try_fold(0usize, |total, part| {
        total
            .checked_add(*part)
            .ok_or(RecorderError::FormatLimit("record length overflow"))
    })
}

fn encode_timing(output: &mut Vec<u8>, timing: MediaTiming) {
    encode_timestamp(output, timing.original_timestamp());
    put_i64(output, timing.presentation_timestamp().as_nanos());
    put_u64(output, timing.duration().as_nanos());
    put_u128(output, timing.clock_domain().get().get());
    put_u64(output, timing.sequence().get());
    output.push(timing.flags().bits());
    match timing.capture_timestamp() {
        Some(timestamp) => {
            output.push(1);
            encode_timestamp(output, timestamp);
        }
        None => output.push(0),
    }
    match timing.timecode() {
        Some(timecode) => {
            output.push(1);
            let (hours, minutes, seconds, frames) = timecode.components();
            output.extend_from_slice(&[hours, minutes, seconds, frames]);
        }
        None => output.push(0),
    }
}

fn encode_timestamp(output: &mut Vec<u8>, timestamp: OriginalTimestamp) {
    put_i64(output, timestamp.timestamp().ticks());
    put_u32(output, timestamp.time_base().numerator());
    put_u32(output, timestamp.time_base().denominator());
}

fn channel_code(channel: Channel) -> u8 {
    match channel {
        Channel::Mono => 0,
        Channel::Left => 1,
        Channel::Right => 2,
        Channel::Center => 3,
        Channel::LowFrequency => 4,
        Channel::LeftSurround => 5,
        Channel::RightSurround => 6,
    }
}

fn pixel_format_code(format: PixelFormat) -> u8 {
    match format {
        PixelFormat::Rgba8 => 0,
        PixelFormat::Bgra8 => 1,
        PixelFormat::Rgba16Float => 2,
        PixelFormat::Nv12 => 3,
        PixelFormat::P010 => 4,
        PixelFormat::Yuv422 => 5,
    }
}

fn put_blob(output: &mut Vec<u8>, value: &[u8]) -> Result<(), RecorderError> {
    put_u64(
        output,
        u64::try_from(value.len()).map_err(|_| RecorderError::FormatLimit("payload too large"))?,
    );
    output.extend_from_slice(value);
    Ok(())
}

fn put_text(output: &mut Vec<u8>, value: &str) -> Result<(), RecorderError> {
    put_u16(
        output,
        u16::try_from(value.len()).map_err(|_| RecorderError::FormatLimit("text too long"))?,
    );
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_option_u64(output: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            output.push(1);
            put_u64(output, value);
        }
        None => output.push(0),
    }
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u128(output: &mut Vec<u8>, value: u128) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(output: &mut Vec<u8>, value: i64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn receipt(value: u128, path: &Path) -> Result<ActionReceiptId, RecorderError> {
    NonZeroU128::new(value)
        .map(ActionReceiptId::new)
        .ok_or_else(|| corrupt(path, "zero action receipt"))
}

fn corrupt(path: &Path, reason: &'static str) -> RecorderError {
    RecorderError::Corrupt {
        path: path.to_path_buf(),
        offset: 0,
        reason,
    }
}

struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], RecorderError> {
        if self.remaining.len() < N {
            return Err(RecorderError::FormatLimit("truncated manifest payload"));
        }
        let (value, remaining) = self.remaining.split_at(N);
        self.remaining = remaining;
        value
            .try_into()
            .map_err(|_| RecorderError::FormatLimit("invalid manifest field"))
    }

    fn u64(&mut self) -> Result<u64, RecorderError> {
        self.take().map(u64::from_le_bytes)
    }

    fn u128(&mut self) -> Result<u128, RecorderError> {
        self.take().map(u128::from_le_bytes)
    }

    fn option_u64(&mut self) -> Result<Option<u64>, RecorderError> {
        match self.take::<1>()?[0] {
            0 => Ok(None),
            1 => self.u64().map(Some),
            _ => Err(RecorderError::FormatLimit(
                "invalid optional manifest field",
            )),
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(u32::MAX, bytes)
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc
}

use core::num::NonZeroU128;

pub(crate) fn create_dir_all_durable(path: &Path) -> Result<(), RecorderError> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor
        .try_exists()
        .map_err(|error| RecorderError::io(cursor, error))?
    {
        missing.push(cursor.to_path_buf());
        cursor = cursor.parent().ok_or_else(|| {
            RecorderError::io(path, io::Error::other("directory has no existing ancestor"))
        })?;
    }
    fs::create_dir_all(path).map_err(|error| RecorderError::io(path, error))?;
    for directory in missing.iter().rev() {
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
    }
    sync_directory(path)
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), RecorderError> {
    sync_directory_io(path).map_err(|error| RecorderError::io(path, error))
}

#[cfg(unix)]
fn sync_directory_io(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory_io(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_directory_io(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
