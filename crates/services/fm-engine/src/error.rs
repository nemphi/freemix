use core::fmt;

use fm_clock::ClockError;
use fm_command::CounterOverflow;
use fm_scheduler::{ActionError, TickError};
use fm_switcher::SwitcherError;
use fm_types::{InputId, MAX_INPUT_NAME_BYTES, OutputId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowError {
    EmptyName,
    EmptyInputName,
    InputNameTooLong,
    DuplicateInputName,
    NoInputs,
    DuplicateInput,
    EmptyOutputName,
    DuplicateOutputName,
    DuplicateOutput(OutputId),
    UnknownInput(InputId),
    Switcher(SwitcherError),
}

impl fmt::Display for ShowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("show name must not be empty"),
            Self::EmptyInputName => formatter.write_str("show input names must not be empty"),
            Self::InputNameTooLong => write!(
                formatter,
                "show input names must not exceed {MAX_INPUT_NAME_BYTES} bytes"
            ),
            Self::DuplicateInputName => formatter.write_str("show input names must be unique"),
            Self::NoInputs => formatter.write_str("show must contain at least one input"),
            Self::DuplicateInput => formatter.write_str("show input identifiers must be unique"),
            Self::EmptyOutputName => formatter.write_str("show output names must not be empty"),
            Self::DuplicateOutputName => formatter.write_str("show output names must be unique"),
            Self::DuplicateOutput(output) => {
                write!(formatter, "show output identifier {output} is duplicated")
            }
            Self::UnknownInput(input) => write!(formatter, "show does not contain input {input}"),
            Self::Switcher(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ShowError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    WorkInFlight,
    IncompatibleSwitcher,
    MismatchedSwitcherRouting,
    MismatchedManualTransition,
    InvalidFrameCounter,
    ClockTimeMismatch {
        expected_ns: u64,
        actual_ns: u64,
    },
    UnrealizedAcceptedReceipt {
        target_frame: u64,
        frame_cursor: u64,
    },
    CounterMismatch {
        accepted_commands: u64,
        revision: u64,
        event_sequence: u64,
        runtime_generation: u64,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkInFlight => {
                formatter.write_str("cannot snapshot or restore while runtime work is in flight")
            }
            Self::IncompatibleSwitcher => formatter
                .write_str("persisted realized switcher inputs do not match the durable show"),
            Self::MismatchedSwitcherRouting => formatter.write_str(
                "persisted desired and realized switcher routing must match for an idle restore",
            ),
            Self::MismatchedManualTransition => formatter.write_str(
                "persisted desired and realized manual transitions must describe the same active mix",
            ),
            Self::InvalidFrameCounter => {
                formatter.write_str("snapshot frame counter cannot be restored")
            }
            Self::ClockTimeMismatch {
                expected_ns,
                actual_ns,
            } => write!(
                formatter,
                "persisted clock time {actual_ns}ns does not match the frame cursor deadline {expected_ns}ns"
            ),
            Self::UnrealizedAcceptedReceipt {
                target_frame,
                frame_cursor,
            } => write!(
                formatter,
                "accepted receipt targets frame {target_frame}, which has not elapsed before cursor {frame_cursor}"
            ),
            Self::CounterMismatch {
                accepted_commands,
                revision,
                event_sequence,
                runtime_generation,
            } => write!(
                formatter,
                "persisted counters must equal the {accepted_commands} accepted commands (revision {revision}, event sequence {event_sequence}, runtime generation {runtime_generation})"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineError {
    Clock(ClockError),
    Schedule(ActionError),
    Tick(TickError),
    RuntimeSwitcher(SwitcherError),
    CounterExhausted,
    Snapshot(SnapshotError),
    StalePreparation,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clock(error) => error.fmt(formatter),
            Self::Schedule(error) => error.fmt(formatter),
            Self::Tick(error) => error.fmt(formatter),
            Self::RuntimeSwitcher(error) => error.fmt(formatter),
            Self::CounterExhausted => formatter.write_str("engine runtime generation exhausted"),
            Self::Snapshot(error) => error.fmt(formatter),
            Self::StalePreparation => {
                formatter.write_str("prepared engine execution was based on stale authority")
            }
        }
    }
}

impl std::error::Error for EngineError {}

impl From<ClockError> for EngineError {
    fn from(value: ClockError) -> Self {
        Self::Clock(value)
    }
}

impl From<ActionError> for EngineError {
    fn from(value: ActionError) -> Self {
        Self::Schedule(value)
    }
}

impl From<TickError> for EngineError {
    fn from(value: TickError) -> Self {
        Self::Tick(value)
    }
}

impl From<SwitcherError> for EngineError {
    fn from(value: SwitcherError) -> Self {
        Self::RuntimeSwitcher(value)
    }
}

impl From<CounterOverflow> for EngineError {
    fn from(_: CounterOverflow) -> Self {
        Self::CounterExhausted
    }
}

impl From<SnapshotError> for EngineError {
    fn from(value: SnapshotError) -> Self {
        Self::Snapshot(value)
    }
}
