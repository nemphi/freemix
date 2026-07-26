//! Bounded, transport-independent translation for the documented vMix API surface.
//!
//! This crate deliberately does not listen on HTTP or TCP sockets. A server may
//! pass an HTTP query string or one decoded TCP line to the parsers, execute the
//! returned native command or compatibility intent, and render state from the
//! same [`CompatState`] used for input resolution.
//!
//! Input references are resolved when parsing. vMix ordinals are one-based,
//! `0` selects Preview, `-1` selects Active, names are case-sensitive, and the
//! stable input ID may be supplied in UUID form.

use std::{error::Error, fmt, fmt::Write as _, num::NonZeroU128};

use fm_protocol::{CommandPayload, WireInputId};

/// Maximum encoded HTTP query length accepted by [`parse_http_query`].
pub const MAX_HTTP_QUERY_BYTES: usize = 8 * 1024;
/// Maximum TCP line length, excluding an optional trailing CRLF.
pub const MAX_TCP_LINE_BYTES: usize = 8 * 1024;
/// Maximum number of query parameters in one function call.
pub const MAX_PARAMETERS: usize = 32;
/// Maximum decoded parameter-name length.
pub const MAX_PARAMETER_NAME_BYTES: usize = 64;
/// Maximum decoded parameter-value length.
pub const MAX_PARAMETER_VALUE_BYTES: usize = 4 * 1024;
/// vMix's documented maximum input count and tally width.
pub const MAX_INPUTS: usize = 1_000;
/// Number of vMix 29 overlay channels represented by the compatibility model.
pub const OVERLAY_CHANNELS: usize = 8;

const MAX_TRANSITION_MILLIS: u32 = 86_400_000;

/// A title text field exposed in a compatibility state snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatTextField {
    /// Field name used by vMix's `SelectedName` parameter.
    pub name: String,
    /// Current field value.
    pub value: String,
}

impl CompatTextField {
    /// Creates a text field.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// One input in the compatibility state model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatInput {
    /// Stable native input identifier. XML renders it in UUID form.
    pub id: WireInputId,
    /// Case-sensitive vMix input title.
    pub title: String,
    /// vMix-style input type label, such as `Camera` or `GT`.
    pub kind: String,
    /// Title text fields, in deterministic `SelectedIndex` order.
    pub text_fields: Vec<CompatTextField>,
}

impl CompatInput {
    /// Creates an input without title fields.
    #[must_use]
    pub fn new(id: WireInputId, title: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            kind: kind.into(),
            text_fields: Vec::new(),
        }
    }

    /// Replaces the title fields while preserving their supplied order.
    #[must_use]
    pub fn with_text_fields(mut self, fields: Vec<CompatTextField>) -> Self {
        self.text_fields = fields;
        self
    }
}

/// Errors constructing or mutating a compatibility state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateError {
    /// More than [`MAX_INPUTS`] were supplied.
    TooManyInputs,
    /// Two inputs have the same stable identifier.
    DuplicateInput,
    /// Active, Preview, or an overlay refers to an absent input.
    InputNotFound,
    /// Overlay channels are one-based and must be in `1..=8`.
    InvalidOverlayChannel,
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyInputs => "compatibility state exceeds the input limit",
            Self::DuplicateInput => "compatibility state contains a duplicate input ID",
            Self::InputNotFound => "compatibility state refers to an unknown input",
            Self::InvalidOverlayChannel => "overlay channel must be between 1 and 8",
        })
    }
}

impl Error for StateError {}

/// Point-in-time state used for command resolution, XML, and tally rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatState {
    /// Version string rendered in the vMix XML element.
    pub version: String,
    /// Inputs in vMix ordinal order.
    pub inputs: Vec<CompatInput>,
    /// Stable ID currently on Program/Active.
    pub active: WireInputId,
    /// Stable ID currently on Preview.
    pub preview: WireInputId,
    /// One entry per one-based overlay channel.
    pub overlays: [Option<WireInputId>; OVERLAY_CHANNELS],
    /// Recording activity exposed by XML.
    pub recording: bool,
    /// Streaming activity exposed by XML.
    pub streaming: bool,
}

impl CompatState {
    /// Creates validated state with empty overlays and inactive outputs.
    ///
    /// # Errors
    ///
    /// Returns an error for too many inputs, duplicate IDs, or absent Active or
    /// Preview references.
    pub fn new(
        version: impl Into<String>,
        inputs: Vec<CompatInput>,
        active: WireInputId,
        preview: WireInputId,
    ) -> Result<Self, StateError> {
        if inputs.len() > MAX_INPUTS {
            return Err(StateError::TooManyInputs);
        }
        for (index, input) in inputs.iter().enumerate() {
            if inputs[..index].iter().any(|other| other.id == input.id) {
                return Err(StateError::DuplicateInput);
            }
        }
        if !inputs.iter().any(|input| input.id == active)
            || !inputs.iter().any(|input| input.id == preview)
        {
            return Err(StateError::InputNotFound);
        }
        Ok(Self {
            version: version.into(),
            inputs,
            active,
            preview,
            overlays: [None; OVERLAY_CHANNELS],
            recording: false,
            streaming: false,
        })
    }

    /// Assigns or clears a one-based overlay channel.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid channel or absent input ID.
    pub fn set_overlay(
        &mut self,
        channel: u8,
        input: Option<WireInputId>,
    ) -> Result<(), StateError> {
        let index = usize::from(channel)
            .checked_sub(1)
            .filter(|index| *index < OVERLAY_CHANNELS)
            .ok_or(StateError::InvalidOverlayChannel)?;
        if input.is_some_and(|id| !self.inputs.iter().any(|candidate| candidate.id == id)) {
            return Err(StateError::InputNotFound);
        }
        self.overlays[index] = input;
        Ok(())
    }

    /// Renders deterministic vMix-shaped XML without an XML declaration.
    ///
    /// Inputs and text fields retain model order. Element text and attributes
    /// escape all five XML metacharacters.
    #[must_use]
    pub fn xml(&self) -> String {
        let mut output = String::from("<vmix><version>");
        escape_xml_into(&mut output, &self.version);
        output.push_str("</version><inputs>");
        for (index, input) in self.inputs.iter().enumerate() {
            let number = index + 1;
            write!(
                output,
                "<input key=\"{}\" number=\"{number}\" type=\"",
                uuid(input.id)
            )
            .expect("writing to a String cannot fail");
            escape_xml_into(&mut output, &input.kind);
            output.push_str("\" title=\"");
            escape_xml_into(&mut output, &input.title);
            output.push_str("\">");
            escape_xml_into(&mut output, &input.title);
            for (field_index, field) in input.text_fields.iter().enumerate() {
                write!(output, "<text index=\"{field_index}\" name=\"")
                    .expect("writing to a String cannot fail");
                escape_xml_into(&mut output, &field.name);
                output.push_str("\">");
                escape_xml_into(&mut output, &field.value);
                output.push_str("</text>");
            }
            output.push_str("</input>");
        }
        output.push_str("</inputs><overlays>");
        for (index, overlay) in self.overlays.iter().enumerate() {
            let channel = index + 1;
            write!(output, "<overlay number=\"{channel}\">")
                .expect("writing to a String cannot fail");
            if let Some(input) = overlay {
                write!(output, "{}", self.ordinal(*input).unwrap_or_default())
                    .expect("writing to a String cannot fail");
            }
            output.push_str("</overlay>");
        }
        write!(
            output,
            "</overlays><preview>{}</preview><active>{}</active><recording>{}</recording><streaming>{}</streaming></vmix>",
            self.ordinal(self.preview).unwrap_or_default(),
            self.ordinal(self.active).unwrap_or_default(),
            vmix_bool(self.recording),
            vmix_bool(self.streaming)
        )
        .expect("writing to a String cannot fail");
        output
    }

    /// Returns one tally character per input: Program/overlay is `1`, Preview
    /// is `2`, and off is `0`. Program takes precedence over Preview.
    #[must_use]
    pub fn tally(&self) -> String {
        self.inputs
            .iter()
            .map(|input| {
                if input.id == self.active || self.overlays.contains(&Some(input.id)) {
                    '1'
                } else if input.id == self.preview {
                    '2'
                } else {
                    '0'
                }
            })
            .collect()
    }

    /// Renders the complete CRLF-terminated TCP tally response.
    #[must_use]
    pub fn tally_response(&self) -> String {
        format!("TALLY OK {}\r\n", self.tally())
    }

    fn ordinal(&self, id: WireInputId) -> Option<usize> {
        self.inputs
            .iter()
            .position(|input| input.id == id)
            .map(|index| index + 1)
    }

    fn resolve_input(&self, reference: &str) -> Result<WireInputId, ParseError> {
        if reference == "0" {
            return Ok(self.preview);
        }
        if reference == "-1" {
            return Ok(self.active);
        }
        if reference.bytes().all(|byte| byte.is_ascii_digit()) {
            let ordinal = reference
                .parse::<usize>()
                .map_err(|_| ParseError::InputNotFound(reference.to_owned()))?;
            return self
                .inputs
                .get(ordinal.wrapping_sub(1))
                .map(|input| input.id)
                .ok_or_else(|| ParseError::InputNotFound(reference.to_owned()));
        }
        if let Some(id) = parse_uuid(reference) {
            return self
                .inputs
                .iter()
                .find(|input| input.id == id)
                .map(|input| input.id)
                .ok_or_else(|| ParseError::InputNotFound(reference.to_owned()));
        }
        self.inputs
            .iter()
            .find(|input| input.title == reference)
            .map(|input| input.id)
            .ok_or_else(|| ParseError::InputNotFound(reference.to_owned()))
    }
}

/// Selects a title field without binding the compatibility core to a title engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextFieldSelector {
    /// A case-sensitive field name.
    Name(String),
    /// A zero-based vMix field index.
    Index(usize),
}

/// Semantics that are valid vMix operations but have no equivalent native wire command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatIntent {
    /// Cut directly to a selected input.
    CutTo { input: WireInputId },
    /// Fade using vMix's millisecond duration, optionally to a selected input.
    Fade {
        input: Option<WireInputId>,
        duration_millis: u32,
    },
    /// Toggle a one-based overlay channel for an input on the main mix.
    ToggleOverlay { channel: u8, input: WireInputId },
    /// Set one title field.
    SetText {
        input: WireInputId,
        field: TextFieldSelector,
        value: String,
    },
    /// Start or stop the primary recorder.
    SetRecording { enabled: bool },
    /// Start or stop all streams or one zero-based stream index.
    SetStreaming { enabled: bool, stream: Option<u16> },
}

/// A recognized function that this compatibility core cannot preserve.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedReport {
    /// Decoded vMix function name.
    pub function: String,
    /// Stable, operator-facing explanation.
    pub reason: &'static str,
}

/// Result of translating one vMix function call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Translation {
    /// Semantics exactly represented by the native protocol.
    Protocol(CommandPayload),
    /// Semantics retained as an adapter intent for a higher-level dispatcher.
    Compat(CompatIntent),
    /// Recognized vMix semantics that cannot be represented safely.
    Unsupported(UnsupportedReport),
}

/// A bounded parser failure. Unknown functions remain distinct from unsupported ones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// HTTP query exceeds [`MAX_HTTP_QUERY_BYTES`].
    QueryTooLong,
    /// TCP command exceeds [`MAX_TCP_LINE_BYTES`].
    LineTooLong,
    /// More than [`MAX_PARAMETERS`] were supplied.
    TooManyParameters,
    /// A decoded parameter name exceeds [`MAX_PARAMETER_NAME_BYTES`].
    ParameterNameTooLong,
    /// A decoded parameter value exceeds [`MAX_PARAMETER_VALUE_BYTES`].
    ParameterValueTooLong,
    /// A percent escape was incomplete or non-hexadecimal.
    InvalidPercentEncoding,
    /// Percent-decoded bytes were not UTF-8.
    InvalidUtf8,
    /// A query item lacked `=` or a TCP line had invalid framing.
    Malformed(&'static str),
    /// No `Function` parameter or TCP function name was supplied.
    MissingFunction,
    /// A parameter name appeared more than once, ignoring ASCII case.
    DuplicateParameter(String),
    /// A required function parameter was absent.
    MissingParameter(&'static str),
    /// A parameter was present but invalid.
    InvalidParameter {
        /// Parameter name.
        parameter: &'static str,
        /// Stable reason suitable for an API error.
        reason: &'static str,
    },
    /// An input selector did not resolve in the supplied state.
    InputNotFound(String),
    /// The function is not one this core recognizes. This maps to vMix's error
    /// path, unlike [`Translation::Unsupported`].
    UnknownFunction(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueryTooLong => formatter.write_str("HTTP query exceeds the byte limit"),
            Self::LineTooLong => formatter.write_str("TCP line exceeds the byte limit"),
            Self::TooManyParameters => formatter.write_str("too many query parameters"),
            Self::ParameterNameTooLong => formatter.write_str("parameter name exceeds the limit"),
            Self::ParameterValueTooLong => formatter.write_str("parameter value exceeds the limit"),
            Self::InvalidPercentEncoding => formatter.write_str("invalid percent encoding"),
            Self::InvalidUtf8 => formatter.write_str("percent-decoded value is not UTF-8"),
            Self::Malformed(reason) => write!(formatter, "malformed request: {reason}"),
            Self::MissingFunction => formatter.write_str("missing Function"),
            Self::DuplicateParameter(parameter) => {
                write!(formatter, "duplicate parameter: {parameter}")
            }
            Self::MissingParameter(parameter) => {
                write!(formatter, "missing parameter: {parameter}")
            }
            Self::InvalidParameter { parameter, reason } => {
                write!(formatter, "invalid parameter {parameter}: {reason}")
            }
            Self::InputNotFound(reference) => write!(formatter, "input not found: {reference}"),
            Self::UnknownFunction(function) => write!(formatter, "unknown function: {function}"),
        }
    }
}

impl Error for ParseError {}

/// Parses and translates the query component of a vMix HTTP API request.
///
/// A single leading `?` is accepted. Form encoding is decoded (`+` is a space),
/// malformed escapes and duplicate names are rejected, and all bounds are
/// applied before a command is returned.
///
/// # Errors
///
/// Returns [`ParseError`] for malformed, over-limit, unknown, or unresolved
/// requests. Recognized but unrepresentable functions return
/// [`Translation::Unsupported`] instead.
pub fn parse_http_query(query: &str, state: &CompatState) -> Result<Translation, ParseError> {
    if query.len() > MAX_HTTP_QUERY_BYTES {
        return Err(ParseError::QueryTooLong);
    }
    let query = query.strip_prefix('?').unwrap_or(query);
    let parameters = parse_parameters(query)?;
    let function = optional(&parameters, "Function").ok_or(ParseError::MissingFunction)?;
    translate(function, &parameters, state)
}

/// Parses and translates one vMix TCP `FUNCTION` line.
///
/// The caller may include one trailing CRLF. Embedded line endings and TCP
/// command names other than `FUNCTION` are rejected; state/TALLY request routing
/// belongs to the surrounding transport service.
///
/// # Errors
///
/// Returns [`ParseError`] under the same conditions as [`parse_http_query`].
pub fn parse_tcp_line(line: &str, state: &CompatState) -> Result<Translation, ParseError> {
    let line = line.strip_suffix("\r\n").unwrap_or(line);
    if line.len() > MAX_TCP_LINE_BYTES {
        return Err(ParseError::LineTooLong);
    }
    if line.contains(['\r', '\n']) {
        return Err(ParseError::Malformed("embedded line ending"));
    }
    let body = line
        .strip_prefix("FUNCTION ")
        .ok_or(ParseError::Malformed("expected FUNCTION command"))?;
    let (function, query) = body.split_once(' ').unwrap_or((body, ""));
    if function.is_empty() {
        return Err(ParseError::MissingFunction);
    }
    if function.len() > MAX_PARAMETER_VALUE_BYTES {
        return Err(ParseError::ParameterValueTooLong);
    }
    let parameters = parse_parameters(query)?;
    translate(function, &parameters, state)
}

fn translate(
    function: &str,
    parameters: &[(String, String)],
    state: &CompatState,
) -> Result<Translation, ParseError> {
    match function {
        "PreviewInput" => {
            allowed(parameters, &["Function", "Input"])?;
            let input = state.resolve_input(required(parameters, "Input")?)?;
            Ok(Translation::Protocol(CommandPayload::SelectPreview {
                input,
            }))
        }
        "Cut" => {
            allowed(parameters, &["Function", "Input", "Mix"])?;
            if !is_main_mix(parameters) {
                return Ok(unsupported(
                    function,
                    "only the main mix (0) has a native switcher command",
                ));
            }
            optional(parameters, "Input").map_or(
                Ok(Translation::Protocol(CommandPayload::Cut)),
                |input| {
                    Ok(Translation::Compat(CompatIntent::CutTo {
                        input: state.resolve_input(input)?,
                    }))
                },
            )
        }
        "Fade" => {
            allowed(parameters, &["Function", "Input", "Duration", "Mix"])?;
            if !is_main_mix(parameters) {
                return Ok(unsupported(
                    function,
                    "only the main mix (0) is represented",
                ));
            }
            let Some(duration) = optional(parameters, "Duration") else {
                return Ok(unsupported(
                    function,
                    "the configured vMix transition duration is not present in the request",
                ));
            };
            let duration_millis = parse_u32(duration, "Duration", 1, MAX_TRANSITION_MILLIS)?;
            let input = optional(parameters, "Input")
                .map(|reference| state.resolve_input(reference))
                .transpose()?;
            Ok(Translation::Compat(CompatIntent::Fade {
                input,
                duration_millis,
            }))
        }
        "SetText" => translate_set_text(parameters, state),
        "StartRecording" | "StopRecording" => {
            allowed(parameters, &["Function"])?;
            Ok(Translation::Compat(CompatIntent::SetRecording {
                enabled: function == "StartRecording",
            }))
        }
        "StartStreaming" | "StopStreaming" => {
            allowed(parameters, &["Function", "Value"])?;
            let stream = optional(parameters, "Value")
                .filter(|value| !value.is_empty())
                .map(|value| parse_u16(value, "Value"))
                .transpose()?;
            Ok(Translation::Compat(CompatIntent::SetStreaming {
                enabled: function == "StartStreaming",
                stream,
            }))
        }
        _ => {
            if let Some(channel) = overlay_channel(function) {
                allowed(parameters, &["Function", "Input", "Mix"])?;
                if !is_main_mix(parameters) {
                    return Ok(unsupported(
                        function,
                        "only the main mix (0) is represented",
                    ));
                }
                return Ok(Translation::Compat(CompatIntent::ToggleOverlay {
                    channel,
                    input: state.resolve_input(required(parameters, "Input")?)?,
                }));
            }
            if known_unsupported(function) {
                Ok(unsupported(
                    function,
                    "the function has no semantics in the current compatibility core",
                ))
            } else {
                Err(ParseError::UnknownFunction(function.to_owned()))
            }
        }
    }
}

fn translate_set_text(
    parameters: &[(String, String)],
    state: &CompatState,
) -> Result<Translation, ParseError> {
    allowed(
        parameters,
        &[
            "Function",
            "Input",
            "SelectedName",
            "SelectedIndex",
            "Value",
        ],
    )?;
    let input = state.resolve_input(required(parameters, "Input")?)?;
    let name = optional(parameters, "SelectedName");
    let index = optional(parameters, "SelectedIndex");
    let field = match (name, index) {
        (Some(name), None) if !name.is_empty() => TextFieldSelector::Name(name.to_owned()),
        (None, Some(index)) => TextFieldSelector::Index(parse_usize(index, "SelectedIndex")?),
        (None, None) => {
            return Err(ParseError::MissingParameter(
                "SelectedName or SelectedIndex",
            ));
        }
        _ => {
            return Err(ParseError::InvalidParameter {
                parameter: "SelectedName/SelectedIndex",
                reason: "supply exactly one field selector",
            });
        }
    };
    Ok(Translation::Compat(CompatIntent::SetText {
        input,
        field,
        value: required(parameters, "Value")?.to_owned(),
    }))
}

fn parse_parameters(query: &str) -> Result<Vec<(String, String)>, ParseError> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let mut parameters: Vec<(String, String)> = Vec::new();
    for item in query.split('&') {
        if parameters.len() == MAX_PARAMETERS {
            return Err(ParseError::TooManyParameters);
        }
        let (encoded_name, encoded_value) = item
            .split_once('=')
            .ok_or(ParseError::Malformed("query parameter lacks '='"))?;
        let name = percent_decode(encoded_name)?;
        let value = percent_decode(encoded_value)?;
        if name.len() > MAX_PARAMETER_NAME_BYTES {
            return Err(ParseError::ParameterNameTooLong);
        }
        if value.len() > MAX_PARAMETER_VALUE_BYTES {
            return Err(ParseError::ParameterValueTooLong);
        }
        if name.is_empty() {
            return Err(ParseError::Malformed("empty parameter name"));
        }
        if parameters
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(&name))
        {
            return Err(ParseError::DuplicateParameter(name));
        }
        parameters.push((name, value));
    }
    Ok(parameters)
}

fn percent_decode(encoded: &str) -> Result<String, ParseError> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' => {
                let high = bytes
                    .get(index + 1)
                    .and_then(|byte| hex(*byte))
                    .ok_or(ParseError::InvalidPercentEncoding)?;
                let low = bytes
                    .get(index + 2)
                    .and_then(|byte| hex(*byte))
                    .ok_or(ParseError::InvalidPercentEncoding)?;
                decoded.push((high << 4) | low);
                index += 2;
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| ParseError::InvalidUtf8)
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn required<'a>(
    parameters: &'a [(String, String)],
    name: &'static str,
) -> Result<&'a str, ParseError> {
    optional(parameters, name).ok_or(ParseError::MissingParameter(name))
}

fn optional<'a>(parameters: &'a [(String, String)], name: &str) -> Option<&'a str> {
    parameters
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn allowed(parameters: &[(String, String)], names: &[&str]) -> Result<(), ParseError> {
    if parameters.iter().all(|(parameter, _)| {
        names
            .iter()
            .any(|allowed| parameter.eq_ignore_ascii_case(allowed))
    }) {
        Ok(())
    } else {
        Err(ParseError::InvalidParameter {
            parameter: "query",
            reason: "contains a parameter not accepted by this function",
        })
    }
}

fn is_main_mix(parameters: &[(String, String)]) -> bool {
    optional(parameters, "Mix").is_none_or(|mix| mix == "0")
}

fn parse_u32(
    value: &str,
    parameter: &'static str,
    minimum: u32,
    maximum: u32,
) -> Result<u32, ParseError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|parsed| (*parsed >= minimum) && (*parsed <= maximum))
        .ok_or(ParseError::InvalidParameter {
            parameter,
            reason: "integer is outside the accepted range",
        })
}

fn parse_u16(value: &str, parameter: &'static str) -> Result<u16, ParseError> {
    value.parse().map_err(|_| ParseError::InvalidParameter {
        parameter,
        reason: "expected a zero-based unsigned integer",
    })
}

fn parse_usize(value: &str, parameter: &'static str) -> Result<usize, ParseError> {
    value.parse().map_err(|_| ParseError::InvalidParameter {
        parameter,
        reason: "expected a zero-based unsigned integer",
    })
}

fn overlay_channel(function: &str) -> Option<u8> {
    let suffix = function.strip_prefix("OverlayInput")?;
    if suffix.len() != 1 {
        return None;
    }
    suffix
        .parse::<u8>()
        .ok()
        .filter(|channel| (1..=8).contains(channel))
}

fn known_unsupported(function: &str) -> bool {
    matches!(
        function,
        "StartStopRecording"
            | "StartStopStreaming"
            | "AudioOn"
            | "AudioOff"
            | "AddInput"
            | "FadeToBlack"
            | "OverlayInputAllOff"
    ) || (function.starts_with("OverlayInput")
        && ["In", "Last", "Off", "Out", "Zoom"]
            .iter()
            .any(|suffix| function.ends_with(suffix)))
}

fn unsupported(function: &str, reason: &'static str) -> Translation {
    Translation::Unsupported(UnsupportedReport {
        function: function.to_owned(),
        reason,
    })
}

fn parse_uuid(value: &str) -> Option<WireInputId> {
    let value = value
        .strip_prefix('{')
        .and_then(|value| value.strip_suffix('}'))
        .unwrap_or(value);
    if value.len() != 36
        || ![8, 13, 18, 23]
            .iter()
            .all(|index| value.as_bytes()[*index] == b'-')
    {
        return None;
    }
    let compact: String = value
        .chars()
        .filter(|character| *character != '-')
        .collect();
    let number = u128::from_str_radix(&compact, 16).ok()?;
    NonZeroU128::new(number).map(WireInputId::new)
}

fn uuid(id: WireInputId) -> String {
    let compact = format!("{:032x}", id.get().get());
    format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    )
}

fn escape_xml_into(output: &mut String, value: &str) {
    for character in value.chars() {
        output.push_str(match character {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '\'' => "&apos;",
            '"' => "&quot;",
            _ => {
                output.push(character);
                continue;
            }
        });
    }
}

const fn vmix_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

#[cfg(test)]
mod tests;
