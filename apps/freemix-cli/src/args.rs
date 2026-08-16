use std::{net::SocketAddr, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionKind {
    Fade,
    Wipe,
    AlphaFade,
    Slide,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayTransition {
    Cut,
    Fade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayPosition {
    FullFrame,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayBorder {
    None,
    ThinWhite,
    ThickWhite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TBarAction {
    Start(ManualTransitionKind),
    SetPosition(u16),
    Commit,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackTarget {
    Live,
    Black,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerAudioPolicy {
    Muted,
    StingerOnly,
    MixWithProgram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerFallback {
    Cut,
    Fade,
    KeepProgram,
}

impl FadeToBlackTarget {
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(self, Self::Black)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    New {
        path: PathBuf,
        name: String,
    },
    InputAdd {
        path: PathBuf,
        input: u128,
        name: String,
    },
    InputRemove {
        path: PathBuf,
        input: u128,
    },
    InputDuplicate {
        path: PathBuf,
        source: u128,
        input: u128,
        name: String,
    },
    Status {
        path: PathBuf,
    },
    AudioStrip {
        path: PathBuf,
        input: u128,
        gain_millidb: i32,
        balance_basis_points: i32,
        muted: bool,
        soloed: bool,
        follow_video: bool,
        delay_samples: u32,
    },
    Rename {
        path: PathBuf,
        input: u128,
        name: String,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Reorder {
        path: PathBuf,
        inputs: Vec<u128>,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Preview {
        path: PathBuf,
        input: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Cut {
        path: PathBuf,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Fade {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    AlphaFade {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Slide {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Zoom {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Stinger {
        path: PathBuf,
        slot: u8,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    StingerConfigure {
        path: PathBuf,
        slot: u8,
        media_input: u128,
        preload: bool,
        cut_point_frames: u32,
        audio_policy: StingerAudioPolicy,
        fallback: StingerFallback,
    },
    StingerRemove {
        path: PathBuf,
        slot: u8,
    },
    OverlayTake {
        path: PathBuf,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayUpdate {
        path: PathBuf,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayOff {
        path: PathBuf,
        channel: u8,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayOutput {
        path: PathBuf,
        channel: u8,
        output: u128,
        included: bool,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayTransition {
        path: PathBuf,
        channel: u8,
        transition: OverlayTransition,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayAppearance {
        path: PathBuf,
        channel: u8,
        position: OverlayPosition,
        border: OverlayBorder,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayQueue {
        path: PathBuf,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    OverlayNext {
        path: PathBuf,
        channel: u8,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Wipe {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    TBar {
        path: PathBuf,
        action: TBarAction,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    FadeToBlack {
        path: PathBuf,
        target: FadeToBlackTarget,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteStatus {
        address: SocketAddr,
    },
    RemoteDiagnostics {
        address: SocketAddr,
    },
    RemoteAudioStrip {
        address: SocketAddr,
        input: u128,
        gain_millidb: i32,
        balance_basis_points: i32,
        muted: bool,
        soloed: bool,
        follow_video: bool,
        delay_samples: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteRename {
        address: SocketAddr,
        input: u128,
        name: String,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteReorder {
        address: SocketAddr,
        inputs: Vec<u128>,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemotePreview {
        address: SocketAddr,
        input: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteCut {
        address: SocketAddr,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteFade {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteAlphaFade {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteSlide {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteZoom {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteStinger {
        address: SocketAddr,
        slot: u8,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteStingerConfigure {
        address: SocketAddr,
        slot: u8,
        media_input: u128,
        preload: bool,
        cut_point_frames: u32,
        audio_policy: StingerAudioPolicy,
        fallback: StingerFallback,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteStingerRemove {
        address: SocketAddr,
        slot: u8,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayTake {
        address: SocketAddr,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayUpdate {
        address: SocketAddr,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayOff {
        address: SocketAddr,
        channel: u8,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayOutput {
        address: SocketAddr,
        channel: u8,
        output: u128,
        included: bool,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayTransition {
        address: SocketAddr,
        channel: u8,
        transition: OverlayTransition,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayAppearance {
        address: SocketAddr,
        channel: u8,
        position: OverlayPosition,
        border: OverlayBorder,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayQueue {
        address: SocketAddr,
        channel: u8,
        source: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteOverlayNext {
        address: SocketAddr,
        channel: u8,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteWipe {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteTBar {
        address: SocketAddr,
        action: TBarAction,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteFadeToBlack {
        address: SocketAddr,
        target: FadeToBlackTarget,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Render {
        path: PathBuf,
        output: PathBuf,
        width: u32,
        height: u32,
    },
    Demo {
        path: PathBuf,
        output: Option<PathBuf>,
    },
    Help,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingCommand,
    MissingValue(&'static str),
    BlankValue(&'static str),
    InvalidNumber {
        field: &'static str,
        value: String,
    },
    InvalidChoice {
        field: &'static str,
        value: String,
    },
    OutOfRange {
        field: &'static str,
        minimum: u16,
        maximum: u16,
        value: u16,
    },
    UnexpectedArgument(String),
    UnknownOption(String),
    UnknownCommand(String),
}

impl core::fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingCommand => formatter.write_str("a command is required"),
            Self::MissingValue(field) => write!(formatter, "missing value for {field}"),
            Self::BlankValue(field) => write!(formatter, "{field} must not be blank"),
            Self::InvalidNumber { field, value } | Self::InvalidChoice { field, value } => {
                write!(formatter, "invalid {field} value `{value}`")
            }
            Self::OutOfRange {
                field,
                minimum,
                maximum,
                value,
            } => write!(
                formatter,
                "{field} must be in {minimum}..={maximum}, got {value}"
            ),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
            Self::UnknownOption(option) => write!(formatter, "unknown option `{option}`"),
            Self::UnknownCommand(command) => write!(formatter, "unknown command `{command}`"),
        }
    }
}

impl std::error::Error for ArgsError {}

pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Command, ArgsError> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(ArgsError::MissingCommand);
    };
    match command.as_str() {
        "new" => parse_new(arguments),
        "input-add" => parse_input_add(arguments),
        "input-remove" => parse_input_remove(arguments),
        "input-duplicate" => parse_input_duplicate(arguments),
        "status" => {
            let path = required_path(&mut arguments, "project path")?;
            reject_extra(&mut arguments)?;
            Ok(Command::Status { path })
        }
        "audio-strip" => parse_audio_strip(arguments),
        "rename" => parse_rename(arguments),
        "input-reorder" => parse_reorder(arguments, false),
        "preview" => {
            let path = required_path(&mut arguments, "project path")?;
            let input = number(&required(&mut arguments, "input")?, "input")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Preview {
                path,
                input,
                key,
                expected_revision,
            })
        }
        "cut" => {
            let path = required_path(&mut arguments, "project path")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Cut {
                path,
                key,
                expected_revision,
            })
        }
        "fade" | "alpha-fade" | "slide" | "zoom" | "wipe" => {
            parse_local_timed_transition(&command, arguments)
        }
        "stinger" => parse_local_stinger(arguments),
        "stinger-configure" => parse_stinger_configuration(arguments),
        "stinger-remove" => parse_stinger_removal(arguments),
        "overlay-take" | "overlay-update" | "overlay-off" | "overlay-output"
        | "overlay-transition" | "overlay-appearance" | "overlay-queue" | "overlay-next" => {
            parse_local_overlay(&command, arguments)
        }
        "tbar-start" | "tbar-position" | "tbar-commit" | "tbar-cancel" => {
            parse_local_t_bar(&command, arguments)
        }
        "ftb" => parse_local_fade_to_black(arguments),
        "remote-status" => parse_remote_status(arguments),
        "remote-diagnostics" => parse_remote_diagnostics(arguments),
        "remote-audio-strip" => parse_remote_audio_strip(arguments),
        "remote-rename" => parse_remote_rename(arguments),
        "remote-input-reorder" => parse_reorder(arguments, true),
        "remote-preview" => parse_remote_preview(arguments),
        "remote-cut" => parse_remote_cut(arguments),
        "remote-fade" | "remote-alpha-fade" | "remote-slide" | "remote-zoom" | "remote-wipe" => {
            parse_remote_timed_transition(&command, arguments)
        }
        "remote-stinger" => parse_remote_stinger(arguments),
        "remote-stinger-configure" => parse_remote_stinger_configuration(arguments),
        "remote-stinger-remove" => parse_remote_stinger_removal(arguments),
        "remote-overlay-take"
        | "remote-overlay-update"
        | "remote-overlay-off"
        | "remote-overlay-output"
        | "remote-overlay-transition"
        | "remote-overlay-appearance"
        | "remote-overlay-queue"
        | "remote-overlay-next" => parse_remote_overlay(&command, arguments),
        "remote-tbar-start"
        | "remote-tbar-position"
        | "remote-tbar-commit"
        | "remote-tbar-cancel" => parse_remote_t_bar(&command, arguments),
        "remote-ftb" => parse_remote_fade_to_black(arguments),
        "render" => {
            let path = required_path(&mut arguments, "project path")?;
            let output = required_path(&mut arguments, "output path")?;
            let mut width = 640;
            let mut height = 360;
            while let Some(option) = arguments.next() {
                match option.as_str() {
                    "--width" => {
                        width = number(&required(&mut arguments, "width")?, "width")?;
                    }
                    "--height" => {
                        height = number(&required(&mut arguments, "height")?, "height")?;
                    }
                    _ => return Err(ArgsError::UnknownOption(option)),
                }
            }
            Ok(Command::Render {
                path,
                output,
                width,
                height,
            })
        }
        "demo" => {
            let path = required_path(&mut arguments, "project path")?;
            let output = arguments.next().map(PathBuf::from);
            reject_extra(&mut arguments)?;
            Ok(Command::Demo { path, output })
        }
        "help" | "--help" | "-h" => Ok(Command::Help),
        _ => Err(ArgsError::UnknownCommand(command)),
    }
}

fn parse_audio_strip(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let gain_millidb = number(&required(&mut arguments, "gain millidB")?, "gain millidB")?;
    let balance_basis_points = number(
        &required(&mut arguments, "balance basis points")?,
        "balance basis points",
    )?;
    let muted = boolean_choice(&required(&mut arguments, "muted")?, "muted")?;
    let soloed = boolean_choice(&required(&mut arguments, "soloed")?, "soloed")?;
    let follow_video = boolean_choice(&required(&mut arguments, "follow video")?, "follow video")?;
    let delay_samples = number(&required(&mut arguments, "delay samples")?, "delay samples")?;
    reject_extra(&mut arguments)?;
    Ok(Command::AudioStrip {
        path,
        input,
        gain_millidb,
        balance_basis_points,
        muted,
        soloed,
        follow_video,
        delay_samples,
    })
}

fn parse_rename(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let name = nonblank(&mut arguments, "input name")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::Rename {
        path,
        input,
        name,
        key,
        expected_revision,
    })
}

fn parse_reorder(
    mut arguments: impl Iterator<Item = String>,
    remote: bool,
) -> Result<Command, ArgsError> {
    let path_or_address = required(
        &mut arguments,
        if remote { "address" } else { "project path" },
    )?;
    let mut inputs = Vec::new();
    let mut options = Vec::new();
    let mut positional = true;
    while let Some(argument) = arguments.next() {
        if argument.starts_with("--") {
            positional = false;
            options.push(argument);
            if matches!(
                options.last().map(String::as_str),
                Some("--key" | "--expect")
            ) {
                options.push(required(&mut arguments, "option value")?);
            }
        } else if positional {
            inputs.push(number(&argument, "input")?);
        } else {
            options.push(argument);
        }
    }
    if inputs.is_empty() {
        return Err(ArgsError::MissingValue("input"));
    }
    let (key, expected_revision) = command_options(options.into_iter())?;
    if remote {
        Ok(Command::RemoteReorder {
            address: socket_address(&path_or_address)?,
            inputs,
            key,
            expected_revision,
        })
    } else {
        Ok(Command::Reorder {
            path: PathBuf::from(path_or_address),
            inputs,
            key,
            expected_revision,
        })
    }
}

fn parse_local_overlay(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let channel = overlay_channel(&required(&mut arguments, "overlay channel")?)?;
    let result = match command {
        "overlay-take" | "overlay-update" | "overlay-queue" => {
            let source = number(&required(&mut arguments, "source input")?, "source input")?;
            let (key, expected_revision) = command_options(arguments)?;
            if command == "overlay-take" {
                Command::OverlayTake {
                    path,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            } else if command == "overlay-update" {
                Command::OverlayUpdate {
                    path,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            } else {
                Command::OverlayQueue {
                    path,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            }
        }
        "overlay-off" => {
            let (key, expected_revision) = command_options(arguments)?;
            Command::OverlayOff {
                path,
                channel,
                key,
                expected_revision,
            }
        }
        "overlay-output" => {
            let output = number(&required(&mut arguments, "output")?, "output")?;
            let included = boolean(&required(&mut arguments, "included")?, "included")?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::OverlayOutput {
                path,
                channel,
                output,
                included,
                key,
                expected_revision,
            }
        }
        "overlay-transition" => {
            let transition = overlay_transition(&required(&mut arguments, "overlay transition")?)?;
            let frames = number(&required(&mut arguments, "frames")?, "frames")?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::OverlayTransition {
                path,
                channel,
                transition,
                frames,
                key,
                expected_revision,
            }
        }
        "overlay-appearance" => {
            let position = overlay_position(&required(&mut arguments, "overlay position")?)?;
            let border = overlay_border(&required(&mut arguments, "overlay border")?)?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::OverlayAppearance {
                path,
                channel,
                position,
                border,
                key,
                expected_revision,
            }
        }
        "overlay-next" => {
            let (key, expected_revision) = command_options(arguments)?;
            Command::OverlayNext {
                path,
                channel,
                key,
                expected_revision,
            }
        }
        _ => unreachable!("caller dispatches only overlay commands"),
    };
    Ok(result)
}

fn parse_remote_overlay(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let channel = overlay_channel(&required(&mut arguments, "overlay channel")?)?;
    let result = match command {
        "remote-overlay-take" | "remote-overlay-update" | "remote-overlay-queue" => {
            let source = number(&required(&mut arguments, "source input")?, "source input")?;
            let (key, expected_revision) = command_options(arguments)?;
            if command == "remote-overlay-take" {
                Command::RemoteOverlayTake {
                    address,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            } else if command == "remote-overlay-update" {
                Command::RemoteOverlayUpdate {
                    address,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            } else {
                Command::RemoteOverlayQueue {
                    address,
                    channel,
                    source,
                    key,
                    expected_revision,
                }
            }
        }
        "remote-overlay-off" => {
            let (key, expected_revision) = command_options(arguments)?;
            Command::RemoteOverlayOff {
                address,
                channel,
                key,
                expected_revision,
            }
        }
        "remote-overlay-output" => {
            let output = number(&required(&mut arguments, "output")?, "output")?;
            let included = boolean(&required(&mut arguments, "included")?, "included")?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::RemoteOverlayOutput {
                address,
                channel,
                output,
                included,
                key,
                expected_revision,
            }
        }
        "remote-overlay-transition" => {
            let transition = overlay_transition(&required(&mut arguments, "overlay transition")?)?;
            let frames = number(&required(&mut arguments, "frames")?, "frames")?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::RemoteOverlayTransition {
                address,
                channel,
                transition,
                frames,
                key,
                expected_revision,
            }
        }
        "remote-overlay-appearance" => {
            let position = overlay_position(&required(&mut arguments, "overlay position")?)?;
            let border = overlay_border(&required(&mut arguments, "overlay border")?)?;
            let (key, expected_revision) = command_options(arguments)?;
            Command::RemoteOverlayAppearance {
                address,
                channel,
                position,
                border,
                key,
                expected_revision,
            }
        }
        "remote-overlay-next" => {
            let (key, expected_revision) = command_options(arguments)?;
            Command::RemoteOverlayNext {
                address,
                channel,
                key,
                expected_revision,
            }
        }
        _ => unreachable!("caller dispatches only remote overlay commands"),
    };
    Ok(result)
}

fn parse_new(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let mut name = "FreeMix Show".to_owned();
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--name" => name = required(&mut arguments, "name")?,
            _ => return Err(ArgsError::UnknownOption(option)),
        }
    }
    Ok(Command::New { path, name })
}

fn parse_input_add(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let name = required(&mut arguments, "input name")?;
    reject_extra(&mut arguments)?;
    Ok(Command::InputAdd { path, input, name })
}

fn parse_input_remove(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    reject_extra(&mut arguments)?;
    Ok(Command::InputRemove { path, input })
}

fn parse_input_duplicate(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let source = number(&required(&mut arguments, "source input")?, "source input")?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let name = required(&mut arguments, "input name")?;
    reject_extra(&mut arguments)?;
    Ok(Command::InputDuplicate {
        path,
        source,
        input,
        name,
    })
}

fn parse_local_timed_transition(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(match command {
        "fade" => Command::Fade {
            path,
            frames,
            key,
            expected_revision,
        },
        "alpha-fade" => Command::AlphaFade {
            path,
            frames,
            key,
            expected_revision,
        },
        "slide" => Command::Slide {
            path,
            frames,
            key,
            expected_revision,
        },
        "zoom" => Command::Zoom {
            path,
            frames,
            key,
            expected_revision,
        },
        "wipe" => Command::Wipe {
            path,
            frames,
            key,
            expected_revision,
        },
        _ => unreachable!("caller only dispatches timed local transitions"),
    })
}

fn parse_local_stinger(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let slot = stinger_slot(&required(&mut arguments, "Stinger slot")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::Stinger {
        path,
        slot,
        frames,
        key,
        expected_revision,
    })
}

fn parse_stinger_configuration(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let (slot, media_input, preload, cut_point_frames, audio_policy, fallback) =
        parse_stinger_configuration_fields(&mut arguments)?;
    reject_extra(&mut arguments)?;
    Ok(Command::StingerConfigure {
        path,
        slot,
        media_input,
        preload,
        cut_point_frames,
        audio_policy,
        fallback,
    })
}

fn parse_stinger_configuration_fields(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(u8, u128, bool, u32, StingerAudioPolicy, StingerFallback), ArgsError> {
    let slot = stinger_slot(&required(arguments, "Stinger slot")?)?;
    let media_input = number(&required(arguments, "media input")?, "media input")?;
    let preload = match required(arguments, "preload")?.as_str() {
        "true" => true,
        "false" => false,
        value => {
            return Err(ArgsError::InvalidChoice {
                field: "preload",
                value: value.to_owned(),
            });
        }
    };
    let cut_point_frames = number(
        &required(arguments, "cut point frames")?,
        "cut point frames",
    )?;
    let audio_policy = match required(arguments, "Stinger audio policy")?.as_str() {
        "muted" => StingerAudioPolicy::Muted,
        "stinger-only" => StingerAudioPolicy::StingerOnly,
        "mix-with-program" => StingerAudioPolicy::MixWithProgram,
        value => {
            return Err(ArgsError::InvalidChoice {
                field: "Stinger audio policy",
                value: value.to_owned(),
            });
        }
    };
    let fallback = match required(arguments, "Stinger fallback")?.as_str() {
        "cut" => StingerFallback::Cut,
        "fade" => StingerFallback::Fade,
        "keep-program" => StingerFallback::KeepProgram,
        value => {
            return Err(ArgsError::InvalidChoice {
                field: "Stinger fallback",
                value: value.to_owned(),
            });
        }
    };
    Ok((
        slot,
        media_input,
        preload,
        cut_point_frames,
        audio_policy,
        fallback,
    ))
}

fn parse_stinger_removal(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let slot = stinger_slot(&required(&mut arguments, "Stinger slot")?)?;
    reject_extra(&mut arguments)?;
    Ok(Command::StingerRemove { path, slot })
}

fn parse_local_t_bar(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let action = parse_t_bar_action(command, &mut arguments)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::TBar {
        path,
        action,
        key,
        expected_revision,
    })
}

fn parse_remote_t_bar(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let action = parse_t_bar_action(command, &mut arguments)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteTBar {
        address,
        action,
        key,
        expected_revision,
    })
}

fn parse_local_fade_to_black(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let target = fade_to_black_target(&required(&mut arguments, "FTB target")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::FadeToBlack {
        path,
        target,
        frames,
        key,
        expected_revision,
    })
}

fn parse_remote_fade_to_black(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let target = fade_to_black_target(&required(&mut arguments, "FTB target")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteFadeToBlack {
        address,
        target,
        frames,
        key,
        expected_revision,
    })
}

fn parse_t_bar_action(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<TBarAction, ArgsError> {
    if command.ends_with("-start") {
        let kind = manual_kind(&required(arguments, "transition kind")?)?;
        Ok(TBarAction::Start(kind))
    } else if command.ends_with("-position") {
        let position = basis_points(&required(arguments, "basis points")?)?;
        Ok(TBarAction::SetPosition(position))
    } else if command.ends_with("-commit") {
        Ok(TBarAction::Commit)
    } else {
        Ok(TBarAction::Cancel)
    }
}

fn parse_remote_status(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    reject_extra(&mut arguments)?;
    Ok(Command::RemoteStatus { address })
}

fn parse_remote_diagnostics(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    reject_extra(&mut arguments)?;
    Ok(Command::RemoteDiagnostics { address })
}

fn parse_remote_audio_strip(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let gain_millidb = number(&required(&mut arguments, "gain millidB")?, "gain millidB")?;
    let balance_basis_points = number(
        &required(&mut arguments, "balance basis points")?,
        "balance basis points",
    )?;
    let muted = boolean_choice(&required(&mut arguments, "muted")?, "muted")?;
    let soloed = boolean_choice(&required(&mut arguments, "soloed")?, "soloed")?;
    let follow_video = boolean_choice(&required(&mut arguments, "follow video")?, "follow video")?;
    let delay_samples = number(&required(&mut arguments, "delay samples")?, "delay samples")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteAudioStrip {
        address,
        input,
        gain_millidb,
        balance_basis_points,
        muted,
        soloed,
        follow_video,
        delay_samples,
        key,
        expected_revision,
    })
}

fn parse_remote_rename(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let name = nonblank(&mut arguments, "input name")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteRename {
        address,
        input,
        name,
        key,
        expected_revision,
    })
}

fn parse_remote_preview(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemotePreview {
        address,
        input,
        key,
        expected_revision,
    })
}

fn parse_remote_cut(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteCut {
        address,
        key,
        expected_revision,
    })
}

fn parse_remote_timed_transition(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(match command {
        "remote-fade" => Command::RemoteFade {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-alpha-fade" => Command::RemoteAlphaFade {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-slide" => Command::RemoteSlide {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-zoom" => Command::RemoteZoom {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-wipe" => Command::RemoteWipe {
            address,
            frames,
            key,
            expected_revision,
        },
        _ => unreachable!("caller only dispatches timed remote transitions"),
    })
}

fn parse_remote_stinger(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let slot = stinger_slot(&required(&mut arguments, "Stinger slot")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteStinger {
        address,
        slot,
        frames,
        key,
        expected_revision,
    })
}

fn parse_remote_stinger_configuration(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let (slot, media_input, preload, cut_point_frames, audio_policy, fallback) =
        parse_stinger_configuration_fields(&mut arguments)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteStingerConfigure {
        address,
        slot,
        media_input,
        preload,
        cut_point_frames,
        audio_policy,
        fallback,
        key,
        expected_revision,
    })
}

fn parse_remote_stinger_removal(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let slot = stinger_slot(&required(&mut arguments, "Stinger slot")?)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteStingerRemove {
        address,
        slot,
        key,
        expected_revision,
    })
}

fn command_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(Option<String>, Option<u64>), ArgsError> {
    let mut key = None;
    let mut expected_revision = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--key" => {
                let value = required(&mut arguments, "idempotency key")?;
                if value.trim().is_empty() {
                    return Err(ArgsError::BlankValue("idempotency key"));
                }
                key = Some(value);
            }
            "--expect" => {
                expected_revision = Some(number(
                    &required(&mut arguments, "expected revision")?,
                    "expected revision",
                )?);
            }
            _ => return Err(ArgsError::UnknownOption(option)),
        }
    }
    Ok((key, expected_revision))
}

fn reject_extra(arguments: &mut impl Iterator<Item = String>) -> Result<(), ArgsError> {
    match arguments.next() {
        Some(argument) => Err(ArgsError::UnexpectedArgument(argument)),
        None => Ok(()),
    }
}

fn required(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<String, ArgsError> {
    arguments.next().ok_or(ArgsError::MissingValue(field))
}

fn nonblank(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<String, ArgsError> {
    let value = required(arguments, field)?;
    if value.trim().is_empty() {
        Err(ArgsError::BlankValue(field))
    } else {
        Ok(value)
    }
}

fn required_path(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<PathBuf, ArgsError> {
    required(arguments, field).map(PathBuf::from)
}

fn number<T>(value: &str, field: &'static str) -> Result<T, ArgsError>
where
    T: core::str::FromStr,
{
    value.parse().map_err(|_| ArgsError::InvalidNumber {
        field,
        value: value.to_owned(),
    })
}

fn boolean_choice(value: &str, field: &'static str) -> Result<bool, ArgsError> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(ArgsError::InvalidChoice {
            field,
            value: value.to_owned(),
        }),
    }
}

fn socket_address(value: &str) -> Result<SocketAddr, ArgsError> {
    number(value, "address")
}

fn manual_kind(value: &str) -> Result<ManualTransitionKind, ArgsError> {
    match value {
        "fade" => Ok(ManualTransitionKind::Fade),
        "wipe" => Ok(ManualTransitionKind::Wipe),
        "alpha-fade" => Ok(ManualTransitionKind::AlphaFade),
        "slide" => Ok(ManualTransitionKind::Slide),
        _ => Err(ArgsError::InvalidChoice {
            field: "transition kind",
            value: value.to_owned(),
        }),
    }
}

fn overlay_transition(value: &str) -> Result<OverlayTransition, ArgsError> {
    match value {
        "cut" => Ok(OverlayTransition::Cut),
        "fade" => Ok(OverlayTransition::Fade),
        _ => Err(ArgsError::InvalidChoice {
            field: "overlay transition",
            value: value.to_owned(),
        }),
    }
}

fn overlay_position(value: &str) -> Result<OverlayPosition, ArgsError> {
    match value {
        "full-frame" => Ok(OverlayPosition::FullFrame),
        "top-left" => Ok(OverlayPosition::TopLeft),
        "top-right" => Ok(OverlayPosition::TopRight),
        "bottom-left" => Ok(OverlayPosition::BottomLeft),
        "bottom-right" => Ok(OverlayPosition::BottomRight),
        _ => Err(ArgsError::InvalidChoice {
            field: "overlay position",
            value: value.to_owned(),
        }),
    }
}

fn overlay_border(value: &str) -> Result<OverlayBorder, ArgsError> {
    match value {
        "none" => Ok(OverlayBorder::None),
        "thin-white" => Ok(OverlayBorder::ThinWhite),
        "thick-white" => Ok(OverlayBorder::ThickWhite),
        _ => Err(ArgsError::InvalidChoice {
            field: "overlay border",
            value: value.to_owned(),
        }),
    }
}

fn fade_to_black_target(value: &str) -> Result<FadeToBlackTarget, ArgsError> {
    match value {
        "live" => Ok(FadeToBlackTarget::Live),
        "black" => Ok(FadeToBlackTarget::Black),
        _ => Err(ArgsError::InvalidChoice {
            field: "FTB target",
            value: value.to_owned(),
        }),
    }
}

fn basis_points(value: &str) -> Result<u16, ArgsError> {
    let position = number(value, "basis points")?;
    if position <= 10_000 {
        Ok(position)
    } else {
        Err(ArgsError::OutOfRange {
            field: "basis points",
            minimum: 0,
            maximum: 10_000,
            value: position,
        })
    }
}

fn stinger_slot(value: &str) -> Result<u8, ArgsError> {
    let slot = number::<u16>(value, "Stinger slot")?;
    if !(1..=8).contains(&slot) {
        return Err(ArgsError::OutOfRange {
            field: "Stinger slot",
            minimum: 1,
            maximum: 8,
            value: slot,
        });
    }
    Ok(u8::try_from(slot).expect("validated Stinger slot fits u8"))
}

fn overlay_channel(value: &str) -> Result<u8, ArgsError> {
    let channel = number::<u16>(value, "overlay channel")?;
    if !(1..=8).contains(&channel) {
        return Err(ArgsError::OutOfRange {
            field: "overlay channel",
            minimum: 1,
            maximum: 8,
            value: channel,
        });
    }
    Ok(u8::try_from(channel).expect("validated overlay channel fits u8"))
}

fn boolean(value: &str, field: &'static str) -> Result<bool, ArgsError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ArgsError::InvalidChoice {
            field,
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_revisioned_fade() {
        assert_eq!(
            parse(strings(&[
                "fade",
                "show.freemix",
                "30",
                "--key",
                "take-1",
                "--expect",
                "4"
            ])),
            Ok(Command::Fade {
                path: "show.freemix".into(),
                frames: 30,
                key: Some("take-1".into()),
                expected_revision: Some(4),
            })
        );
    }

    #[test]
    fn parses_local_and_remote_wipe() {
        assert_eq!(
            parse(strings(&[
                "wipe",
                "show.freemix",
                "45",
                "--key",
                "wipe-1",
                "--expect",
                "4",
            ])),
            Ok(Command::Wipe {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("wipe-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-wipe",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-wipe",
            ])),
            Ok(Command::RemoteWipe {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-wipe".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_alpha_fade() {
        assert_eq!(
            parse(strings(&[
                "alpha-fade",
                "show.freemix",
                "45",
                "--key",
                "alpha-fade-1",
                "--expect",
                "4",
            ])),
            Ok(Command::AlphaFade {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("alpha-fade-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-alpha-fade",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-alpha-fade",
            ])),
            Ok(Command::RemoteAlphaFade {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-alpha-fade".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_slide() {
        assert_eq!(
            parse(strings(&[
                "slide",
                "show.freemix",
                "45",
                "--key",
                "slide-1",
                "--expect",
                "4",
            ])),
            Ok(Command::Slide {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("slide-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-slide",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-slide",
            ])),
            Ok(Command::RemoteSlide {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-slide".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_zoom() {
        assert_eq!(
            parse(strings(&[
                "zoom",
                "show.freemix",
                "45",
                "--key",
                "zoom-1",
                "--expect",
                "4",
            ])),
            Ok(Command::Zoom {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("zoom-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-zoom",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-zoom",
            ])),
            Ok(Command::RemoteZoom {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-zoom".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_stinger_and_rejects_invalid_slots() {
        assert_eq!(
            parse(strings(&[
                "stinger",
                "show.freemix",
                "8",
                "45",
                "--key",
                "stinger-8",
                "--expect",
                "4",
            ])),
            Ok(Command::Stinger {
                path: "show.freemix".into(),
                slot: 8,
                frames: 45,
                key: Some("stinger-8".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-stinger",
                "127.0.0.1:9123",
                "1",
                "12",
                "--key",
                "remote-stinger",
            ])),
            Ok(Command::RemoteStinger {
                address: "127.0.0.1:9123".parse().unwrap(),
                slot: 1,
                frames: 12,
                key: Some("remote-stinger".into()),
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&["stinger", "show.freemix", "0", "12"])),
            Err(ArgsError::OutOfRange {
                field: "Stinger slot",
                minimum: 1,
                maximum: 8,
                value: 0,
            })
        );
        assert_eq!(
            parse(strings(&["remote-stinger", "127.0.0.1:9123", "9", "12"])),
            Err(ArgsError::OutOfRange {
                field: "Stinger slot",
                minimum: 1,
                maximum: 8,
                value: 9,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_overlay_appearance() {
        assert_eq!(
            parse(strings(&[
                "overlay-appearance",
                "show.freemix",
                "4",
                "bottom-right",
                "thick-white",
                "--expect",
                "7",
            ])),
            Ok(Command::OverlayAppearance {
                path: "show.freemix".into(),
                channel: 4,
                position: OverlayPosition::BottomRight,
                border: OverlayBorder::ThickWhite,
                key: None,
                expected_revision: Some(7),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-overlay-appearance",
                "127.0.0.1:9123",
                "1",
                "top-left",
                "thin-white",
            ])),
            Ok(Command::RemoteOverlayAppearance {
                address: "127.0.0.1:9123".parse().unwrap(),
                channel: 1,
                position: OverlayPosition::TopLeft,
                border: OverlayBorder::ThinWhite,
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&["overlay-queue", "show.freemix", "4", "2",])),
            Ok(Command::OverlayQueue {
                path: "show.freemix".into(),
                channel: 4,
                source: 2,
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&["remote-overlay-next", "127.0.0.1:9123", "4"])),
            Ok(Command::RemoteOverlayNext {
                address: "127.0.0.1:9123".parse().unwrap(),
                channel: 4,
                key: None,
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_complete_stinger_configuration_and_removal() {
        assert_eq!(
            parse(strings(&[
                "stinger-configure",
                "show.freemix",
                "8",
                "340282366920938463463374607431768211455",
                "false",
                "45",
                "mix-with-program",
                "keep-program",
            ])),
            Ok(Command::StingerConfigure {
                path: "show.freemix".into(),
                slot: 8,
                media_input: u128::MAX,
                preload: false,
                cut_point_frames: 45,
                audio_policy: StingerAudioPolicy::MixWithProgram,
                fallback: StingerFallback::KeepProgram,
            })
        );
        assert_eq!(
            parse(strings(&["stinger-remove", "show.freemix", "1"])),
            Ok(Command::StingerRemove {
                path: "show.freemix".into(),
                slot: 1,
            })
        );
        assert!(matches!(
            parse(strings(&[
                "stinger-configure",
                "show.freemix",
                "1",
                "2",
                "yes",
                "12",
                "muted",
                "cut",
            ])),
            Err(ArgsError::InvalidChoice {
                field: "preload",
                ..
            })
        ));
        assert!(matches!(
            parse(strings(&[
                "stinger-configure",
                "show.freemix",
                "1",
                "2",
                "true",
                "12",
                "program-only",
                "cut",
            ])),
            Err(ArgsError::InvalidChoice {
                field: "Stinger audio policy",
                ..
            })
        ));
    }

    #[test]
    fn parses_local_and_remote_t_bar_commands_with_exact_endpoints() {
        assert_eq!(
            parse(strings(&[
                "tbar-start",
                "show.freemix",
                "wipe",
                "--key",
                "manual-start",
                "--expect",
                "4",
            ])),
            Ok(Command::TBar {
                path: "show.freemix".into(),
                action: TBarAction::Start(ManualTransitionKind::Wipe),
                key: Some("manual-start".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&["tbar-position", "show.freemix", "0"])),
            Ok(Command::TBar {
                path: "show.freemix".into(),
                action: TBarAction::SetPosition(0),
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-tbar-position",
                "127.0.0.1:9123",
                "10000",
                "--expect",
                "9",
            ])),
            Ok(Command::RemoteTBar {
                address: "127.0.0.1:9123".parse().unwrap(),
                action: TBarAction::SetPosition(10_000),
                key: None,
                expected_revision: Some(9),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-tbar-start",
                "127.0.0.1:9123",
                "alpha-fade",
                "--key",
                "manual-alpha-start",
            ])),
            Ok(Command::RemoteTBar {
                address: "127.0.0.1:9123".parse().unwrap(),
                action: TBarAction::Start(ManualTransitionKind::AlphaFade),
                key: Some("manual-alpha-start".into()),
                expected_revision: None,
            })
        );
        assert!(matches!(
            parse(strings(&["tbar-commit", "show.freemix"])),
            Ok(Command::TBar {
                action: TBarAction::Commit,
                ..
            })
        ));
        assert!(matches!(
            parse(strings(&["remote-tbar-cancel", "127.0.0.1:9123"])),
            Ok(Command::RemoteTBar {
                action: TBarAction::Cancel,
                ..
            })
        ));
    }

    #[test]
    fn parses_local_and_remote_fade_to_black_targets() {
        assert_eq!(
            parse(strings(&[
                "ftb",
                "show.freemix",
                "black",
                "45",
                "--key",
                "blackout",
                "--expect",
                "7",
            ])),
            Ok(Command::FadeToBlack {
                path: "show.freemix".into(),
                target: FadeToBlackTarget::Black,
                frames: 45,
                key: Some("blackout".into()),
                expected_revision: Some(7),
            })
        );
        assert_eq!(
            parse(strings(&["remote-ftb", "127.0.0.1:9123", "live", "30",])),
            Ok(Command::RemoteFadeToBlack {
                address: "127.0.0.1:9123".parse().unwrap(),
                target: FadeToBlackTarget::Live,
                frames: 30,
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&["ftb", "show.freemix", "toggle", "10"])),
            Err(ArgsError::InvalidChoice {
                field: "FTB target",
                value: "toggle".into(),
            })
        );
    }

    #[test]
    fn rejects_non_integer_unknown_and_out_of_range_t_bar_values() {
        assert!(matches!(
            parse(strings(&["tbar-position", "show.freemix", "62.50"])),
            Err(ArgsError::InvalidNumber {
                field: "basis points",
                ..
            })
        ));
        assert_eq!(
            parse(strings(&["tbar-position", "show.freemix", "10001"])),
            Err(ArgsError::OutOfRange {
                field: "basis points",
                minimum: 0,
                maximum: 10_000,
                value: 10_001,
            })
        );
        assert!(matches!(
            parse(strings(&["tbar-start", "show.freemix", "unknown"])),
            Err(ArgsError::InvalidChoice {
                field: "transition kind",
                ..
            })
        ));
    }

    #[test]
    fn rejects_unknown_option() {
        assert_eq!(
            parse(strings(&["cut", "show.freemix", "--wat"])),
            Err(ArgsError::UnknownOption("--wat".into()))
        );
    }

    #[test]
    fn status_rejects_extra_positional_argument() {
        assert_eq!(
            parse(strings(&["status", "show.freemix", "extra"])),
            Err(ArgsError::UnexpectedArgument("extra".into()))
        );
    }

    #[test]
    fn audio_strip_parses_exact_project_input_and_controls() {
        assert_eq!(
            parse(strings(&[
                "audio-strip",
                "show.freemix",
                "7",
                "-6000",
                "2500",
                "on",
                "on",
                "off",
                "48000",
            ])),
            Ok(Command::AudioStrip {
                path: PathBuf::from("show.freemix"),
                input: 7,
                gain_millidb: -6_000,
                balance_basis_points: 2_500,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 48_000,
            })
        );
    }

    #[test]
    fn remote_audio_strip_parses_command_options() {
        assert_eq!(
            parse(strings(&[
                "remote-audio-strip",
                "127.0.0.1:9123",
                "7",
                "-6000",
                "-2500",
                "off",
                "off",
                "on",
                "2400",
                "--key",
                "delay",
                "--expect",
                "3",
            ])),
            Ok(Command::RemoteAudioStrip {
                address: "127.0.0.1:9123".parse().unwrap(),
                input: 7,
                gain_millidb: -6_000,
                balance_basis_points: -2_500,
                muted: false,
                soloed: false,
                follow_video: true,
                delay_samples: 2_400,
                key: Some("delay".into()),
                expected_revision: Some(3),
            })
        );
    }

    #[test]
    fn demo_rejects_more_than_one_output_path() {
        assert_eq!(
            parse(strings(&[
                "demo",
                "show.freemix",
                "output.ppm",
                "extra.ppm"
            ])),
            Err(ArgsError::UnexpectedArgument("extra.ppm".into()))
        );
    }

    #[test]
    fn rejects_blank_explicit_idempotency_key() {
        assert_eq!(
            parse(strings(&["cut", "show.freemix", "--key", "  \t"])),
            Err(ArgsError::BlankValue("idempotency key"))
        );
    }

    #[test]
    fn parses_remote_preview_options() {
        assert_eq!(
            parse(strings(&[
                "remote-preview",
                "127.0.0.1:9123",
                "2",
                "--key",
                "remote-preview",
                "--expect",
                "7",
            ])),
            Ok(Command::RemotePreview {
                address: "127.0.0.1:9123".parse().unwrap(),
                input: 2,
                key: Some("remote-preview".into()),
                expected_revision: Some(7),
            })
        );
    }
}
