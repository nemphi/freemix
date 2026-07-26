use fm_frame::TimeBase;
use serde_json::{Map, Value};

use crate::{Error, LimitKind};

/// Stream type reported by ffprobe. Unknown values remain visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamKind {
    Video,
    Audio,
    Other(String),
}

/// Deterministic selection within one requested media type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StreamSelector {
    /// Prefer a default stream, then the lowest absolute stream index.
    Best,
    /// Select one absolute container stream index.
    Index(u32),
}

/// Raw format fields retained from bounded ffprobe JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatInfo {
    pub name: String,
    pub long_name: Option<String>,
    pub start_time: Option<String>,
    pub duration: Option<String>,
}

/// Decode-relevant stream metadata. Enumerated `FFmpeg` strings are intentionally
/// retained as strings so unknown codecs and color values are not mislabeled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamInfo {
    pub index: u32,
    pub kind: StreamKind,
    pub codec_name: Option<String>,
    pub time_base: Option<TimeBase>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pixel_format: Option<String>,
    pub sample_aspect_ratio: Option<String>,
    pub field_order: Option<String>,
    pub rotation_degrees: Option<i32>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub default: bool,
    pub attached_picture: bool,
}

/// Bounded local-file probe result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Probe {
    pub format: FormatInfo,
    pub streams: Vec<StreamInfo>,
}

impl Probe {
    /// Resolves a video selector while excluding attached pictures.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSelector`] or [`Error::MissingStream`].
    pub fn select_video(&self, selector: StreamSelector) -> Result<&StreamInfo, Error> {
        self.select(selector, true)
    }

    /// Resolves an audio selector.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidSelector`] or [`Error::MissingStream`].
    pub fn select_audio(&self, selector: StreamSelector) -> Result<&StreamInfo, Error> {
        self.select(selector, false)
    }

    fn select(&self, selector: StreamSelector, video: bool) -> Result<&StreamInfo, Error> {
        let matches_kind = |stream: &&StreamInfo| {
            matches!(
                (&stream.kind, video),
                (StreamKind::Video, true) | (StreamKind::Audio, false)
            ) && (!video || !stream.attached_picture)
        };
        match selector {
            StreamSelector::Index(index) => self
                .streams
                .iter()
                .find(|stream| stream.index == index)
                .filter(|stream| matches_kind(stream))
                .ok_or(Error::InvalidSelector),
            StreamSelector::Best => self
                .streams
                .iter()
                .filter(matches_kind)
                .min_by_key(|stream| (!stream.default, stream.index))
                .ok_or(Error::MissingStream),
        }
    }
}

pub(crate) fn parse_probe(bytes: &[u8], max_streams: usize) -> Result<Probe, Error> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| Error::MalformedProbe)?;
    let root = root.as_object().ok_or(Error::MalformedProbe)?;
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .ok_or(Error::MalformedProbe)?;
    if streams.len() > max_streams {
        return Err(Error::LimitExceeded {
            kind: LimitKind::Streams,
            actual: u64::try_from(streams.len()).unwrap_or(u64::MAX),
            maximum: u64::try_from(max_streams).unwrap_or(u64::MAX),
        });
    }
    let format = parse_format(root.get("format").ok_or(Error::MalformedProbe)?)?;
    let streams = streams
        .iter()
        .map(parse_stream)
        .collect::<Result<Vec<_>, _>>()?;
    if streams.iter().enumerate().any(|(position, stream)| {
        streams[position + 1..]
            .iter()
            .any(|other| other.index == stream.index)
    }) {
        return Err(Error::MalformedProbe);
    }
    Ok(Probe { format, streams })
}

fn parse_format(value: &Value) -> Result<FormatInfo, Error> {
    let object = value.as_object().ok_or(Error::MalformedProbe)?;
    Ok(FormatInfo {
        name: string_required(object, "format_name")?,
        long_name: string_optional(object, "format_long_name")?,
        start_time: string_optional(object, "start_time")?,
        duration: string_optional(object, "duration")?,
    })
}

fn parse_stream(value: &Value) -> Result<StreamInfo, Error> {
    let object = value.as_object().ok_or(Error::MalformedProbe)?;
    let index = unsigned_required(object, "index")?;
    let index = u32::try_from(index).map_err(|_| Error::MalformedProbe)?;
    let raw_kind = string_required(object, "codec_type")?;
    let kind = match raw_kind.as_str() {
        "video" => StreamKind::Video,
        "audio" => StreamKind::Audio,
        _ => StreamKind::Other(raw_kind),
    };
    let disposition = object
        .get("disposition")
        .and_then(Value::as_object)
        .ok_or(Error::MalformedProbe)?;

    Ok(StreamInfo {
        index,
        kind,
        codec_name: string_optional(object, "codec_name")?,
        time_base: object.get("time_base").map(parse_time_base).transpose()?,
        width: u32_optional(object, "width")?,
        height: u32_optional(object, "height")?,
        pixel_format: string_optional(object, "pix_fmt")?,
        sample_aspect_ratio: string_optional(object, "sample_aspect_ratio")?,
        field_order: string_optional(object, "field_order")?,
        rotation_degrees: parse_rotation(object)?,
        color_primaries: string_optional(object, "color_primaries")?,
        color_transfer: string_optional(object, "color_transfer")?,
        color_space: string_optional(object, "color_space")?,
        color_range: string_optional(object, "color_range")?,
        sample_rate: u32_optional(object, "sample_rate")?,
        channels: u32_optional(object, "channels")?,
        channel_layout: parse_channel_layout(object)?,
        default: boolean_integer(disposition, "default")?,
        attached_picture: boolean_integer(disposition, "attached_pic")?,
    })
}

fn parse_time_base(value: &Value) -> Result<TimeBase, Error> {
    let raw = value.as_str().ok_or(Error::MalformedProbe)?;
    let (numerator, denominator) = raw.split_once('/').ok_or(Error::MalformedProbe)?;
    let numerator = numerator
        .parse::<u32>()
        .map_err(|_| Error::MalformedProbe)?;
    let denominator = denominator
        .parse::<u32>()
        .map_err(|_| Error::MalformedProbe)?;
    TimeBase::new(numerator, denominator).map_err(|_| Error::MalformedProbe)
}

fn parse_rotation(object: &Map<String, Value>) -> Result<Option<i32>, Error> {
    if let Some(rotation) = object
        .get("side_data_list")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find_map(|item| item.get("rotation")))
    {
        return signed_value(rotation)
            .and_then(|value| i32::try_from(value).map_err(|_| Error::MalformedProbe))
            .map(Some);
    }
    let Some(rotation) = object
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|tags| tags.get("rotate"))
    else {
        return Ok(None);
    };
    signed_value(rotation)
        .and_then(|value| i32::try_from(value).map_err(|_| Error::MalformedProbe))
        .map(Some)
}

fn parse_channel_layout(object: &Map<String, Value>) -> Result<Option<String>, Error> {
    if let Some(layout) = string_optional(object, "channel_layout")? {
        return Ok(Some(layout));
    }
    let Some(layout) = object.get("ch_layout") else {
        return Ok(None);
    };
    match layout {
        Value::String(layout) => Ok(Some(layout.clone())),
        Value::Object(layout) => string_optional(layout, "layout"),
        _ => Err(Error::MalformedProbe),
    }
}

fn string_required(object: &Map<String, Value>, key: &str) -> Result<String, Error> {
    string_optional(object, key)?.ok_or(Error::MalformedProbe)
}

fn string_optional(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Error> {
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

fn unsigned_required(object: &Map<String, Value>, key: &str) -> Result<u64, Error> {
    let value = object.get(key).ok_or(Error::MalformedProbe)?;
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    value
        .as_str()
        .ok_or(Error::MalformedProbe)?
        .parse::<u64>()
        .map_err(|_| Error::MalformedProbe)
}

fn u32_optional(object: &Map<String, Value>, key: &str) -> Result<Option<u32>, Error> {
    object
        .get(key)
        .map(|_| {
            unsigned_required(object, key)
                .and_then(|value| u32::try_from(value).map_err(|_| Error::MalformedProbe))
        })
        .transpose()
}

fn signed_value(value: &Value) -> Result<i64, Error> {
    if let Some(number) = value.as_i64() {
        return Ok(number);
    }
    value
        .as_str()
        .ok_or(Error::MalformedProbe)?
        .parse::<i64>()
        .map_err(|_| Error::MalformedProbe)
}

fn boolean_integer(object: &Map<String, Value>, key: &str) -> Result<bool, Error> {
    match unsigned_required(object, key)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::MalformedProbe),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
      "streams": [
        {"index":4,"codec_name":"mystery","codec_type":"video","width":64,"height":48,
         "pix_fmt":"yuv420p","sample_aspect_ratio":"1:1","time_base":"1/1000",
         "color_primaries":"future-color","disposition":{"default":0,"attached_pic":0}},
        {"index":2,"codec_name":"aac","codec_type":"audio","sample_rate":"48000",
         "channels":2,"time_base":"1/48000","disposition":{"default":1,"attached_pic":0}},
        {"index":1,"codec_name":"aac","codec_type":"audio","sample_rate":"48000",
         "channels":2,"time_base":"1/48000","disposition":{"default":0,"attached_pic":0}}
      ],
      "format":{"format_name":"nut","format_long_name":"NUT"}
    }"#;

    #[test]
    fn parses_raw_unknown_values_and_absolute_indices() {
        let probe = parse_probe(JSON.as_bytes(), 3).unwrap();
        assert_eq!(probe.streams[0].codec_name.as_deref(), Some("mystery"));
        assert_eq!(
            probe.streams[0].color_primaries.as_deref(),
            Some("future-color")
        );
        assert_eq!(probe.select_audio(StreamSelector::Best).unwrap().index, 2);
        assert_eq!(
            probe.select_audio(StreamSelector::Index(1)).unwrap().index,
            1
        );
        assert_eq!(
            probe.select_video(StreamSelector::Index(2)),
            Err(Error::InvalidSelector)
        );
    }

    #[test]
    fn rejects_stream_limit_and_malformed_fields() {
        assert!(matches!(
            parse_probe(JSON.as_bytes(), 2),
            Err(Error::LimitExceeded {
                kind: LimitKind::Streams,
                ..
            })
        ));
        let malformed = JSON.replace("\"1/1000\"", "\"0/0\"");
        assert_eq!(
            parse_probe(malformed.as_bytes(), 3),
            Err(Error::MalformedProbe)
        );
    }
}
