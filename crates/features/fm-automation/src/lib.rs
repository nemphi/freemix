//! Deterministic shortcuts, triggers, macros, schedules, and controllers.
//!
//! This crate describes automation intent only. It performs no device I/O and
//! never reads a clock; every time-sensitive operation receives a caller-owned
//! timestamp.

mod condition;
mod controller;
mod go;
mod intent;
mod macros;
mod schedule;
mod shortcut;
mod trigger;

pub use condition::{Condition, ConditionContext, Predicate, Value};
pub use controller::{
    ActivatorEngine, ActivatorMapping, ActivatorRule, ControlAddress, ControlMode, ControllerError,
    ControllerFeedback, ControllerInput, ControllerManager, DeviceState, LearnRequest,
    MappedControllerIntent, Mapping, TallySnapshot, ValueRange,
};
pub use go::{
    GoAction, GoActionFire, GoEngine, GoError, GoPreview, GoStart, MAX_GO_START_RECEIPTS,
    PlannedGoAction, ProgrammedGo,
};
pub use intent::{CommandIntent, IntentBuffer};
pub use macros::{
    AttemptOutcome, CancelPolicy, MacroDecision, MacroDefinition, MacroDispatch, MacroError,
    MacroRun, MacroStatus, RetryPolicy,
};
pub use schedule::{ScheduleError, ScheduleFire, ScheduleId, ScheduleKind, ScheduleSet};
pub use shortcut::{
    Chord, ChordError, ConflictKind, KeyStroke, Modifiers, Shortcut, ShortcutConflict,
    ShortcutError, ShortcutRegistry, ShortcutScope,
};
pub use trigger::{
    AutomationEvent, EventFilter, Trigger, TriggerEngine, TriggerError, TriggerFire,
};
