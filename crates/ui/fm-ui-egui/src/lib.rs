//! Pure `egui` presentation for the native `FreeMix` studio shell.
//!
//! This crate translates immutable client views into operator-facing controls
//! and returns intents. It performs no command dispatch, persistence, or I/O.

use egui::{
    Button, Color32, DragValue, Frame, Grid, Margin, RichText, ScrollArea, Stroke, Ui, Vec2,
};
use fm_protocol::{
    ManualTransitionKind, ManualTransitionPosition, OverlayBorderPreset, OverlayPositionPreset,
    OverlayTransitionKind, StingerReadiness, WireOverlayChannelId, WireStingerSlotId,
};
use fm_types::InputId;
use fm_ui_model::{ClientView, ManualTransitionStatus, SwitcherState};

mod fade_to_black;

pub use fade_to_black::{FadeToBlackAvailability, FadeToBlackGate, fade_to_black_availability};

const GRAPHITE: Color32 = Color32::from_rgb(13, 15, 17);
const GRAPHITE_RAISED: Color32 = Color32::from_rgb(24, 27, 30);
const MONITOR_BLACK: Color32 = Color32::from_rgb(4, 5, 6);
const TEXT: Color32 = Color32::from_rgb(220, 224, 225);
const MUTED: Color32 = Color32::from_rgb(125, 133, 137);
const PROGRAM: Color32 = Color32::from_rgb(224, 48, 53);
const PREVIEW: Color32 = Color32::from_rgb(38, 190, 101);
const AMBER: Color32 = Color32::from_rgb(231, 164, 43);
const ERROR: Color32 = Color32::from_rgb(255, 100, 91);
const MIN_TILE_WIDTH: f32 = 156.0;
const NARROW_MONITOR_WIDTH: f32 = 700.0;

/// Operator actions emitted by [`StudioShell`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StudioIntent {
    /// Selects an input on the Preview bus.
    SelectPreview(InputId),
    /// Performs an immediate Cut transition.
    Cut,
    /// Performs a Fade transition with a duration in frames.
    Fade { duration_frames: u32 },
    /// Performs an `AlphaFade` transition with a duration in frames.
    AlphaFade { duration_frames: u32 },
    /// Performs a horizontal Slide transition with a duration in frames.
    Slide { duration_frames: u32 },
    /// Performs a centered Zoom transition with a duration in frames.
    Zoom { duration_frames: u32 },
    /// Fires one of eight configured Stinger slots.
    Stinger {
        slot: WireStingerSlotId,
        duration_frames: u32,
    },
    /// Takes the current Preview source on one independent overlay channel.
    TakeOverlay {
        channel: WireOverlayChannelId,
        source: InputId,
    },
    /// Removes one overlay channel from Program.
    OverlayOff { channel: WireOverlayChannelId },
    /// Changes the transition used by one overlay channel.
    ConfigureOverlayTransition {
        channel: WireOverlayChannelId,
        transition: OverlayTransitionKind,
        duration_frames: u32,
    },
    /// Changes the placement and border presets for one overlay channel.
    ConfigureOverlayAppearance {
        channel: WireOverlayChannelId,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
    },
    /// Appends the current Preview source to one overlay queue.
    QueueOverlay {
        channel: WireOverlayChannelId,
        source: InputId,
    },
    /// Takes and removes the head of one overlay queue.
    TakeNextOverlay { channel: WireOverlayChannelId },
    /// Performs a Wipe transition with a duration in frames.
    Wipe { duration_frames: u32 },
    /// Fades realized Program video and audio to black or back to live.
    FadeToBlack { active: bool, duration_frames: u32 },
    /// Starts a held manual Fade, Wipe, or `AlphaFade` transition.
    StartManualTransition { kind: ManualTransitionKind },
    /// Sets the exact manual-transition position in basis points.
    SetManualTransitionPosition { position: ManualTransitionPosition },
    /// Commits the active manual transition at its current direction.
    CommitManualTransition,
    /// Cancels the active manual transition and restores its starting routing.
    CancelManualTransition,
}

/// Native studio connection lifecycle as presented to an operator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StudioConnectionStatus {
    /// The native process is starting its control client.
    #[default]
    Launching,
    /// A transport connection is being established.
    Connecting,
    /// State is connected but not yet current.
    Synchronizing,
    /// State is current and operator controls are available.
    Ready,
    /// Reconnection is delayed by retry backoff.
    Backoff,
    /// No control connection is present.
    Disconnected,
    /// Connection or synchronization failed.
    Failed,
    /// The peer protocol is incompatible with this studio.
    Incompatible,
}

impl StudioConnectionStatus {
    /// Returns the compact uppercase label used in the studio header.
    #[must_use]
    pub const fn operator_label(self) -> &'static str {
        match self {
            Self::Launching => "LAUNCHING",
            Self::Connecting => "CONNECTING",
            Self::Synchronizing => "SYNCHRONIZING",
            Self::Ready => "READY",
            Self::Backoff => "RETRY BACKOFF",
            Self::Disconnected => "DISCONNECTED",
            Self::Failed => "FAILED",
            Self::Incompatible => "INCOMPATIBLE",
        }
    }

    /// Returns whether this connection state permits production controls.
    #[must_use]
    pub const fn controls_enabled(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Owned render input for one studio frame.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StudioUiState {
    pub connection_status: StudioConnectionStatus,
    pub view: Option<ClientView>,
    pub can_select_preview: bool,
    pub can_transition: bool,
    pub pending_commands: usize,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl StudioUiState {
    /// Creates a state without a replicated client view.
    #[must_use]
    pub const fn new(connection_status: StudioConnectionStatus) -> Self {
        Self {
            connection_status,
            view: None,
            can_select_preview: false,
            can_transition: false,
            pending_commands: 0,
            notice: None,
            error: None,
        }
    }

    /// Attaches an owned client view.
    #[must_use]
    pub fn with_view(mut self, view: ClientView) -> Self {
        self.view = Some(view);
        self
    }

    /// Applies the switcher permissions negotiated for the active session.
    #[must_use]
    pub const fn with_switcher_permissions(
        mut self,
        can_select_preview: bool,
        can_transition: bool,
    ) -> Self {
        self.can_select_preview = can_select_preview;
        self.can_transition = can_transition;
        self
    }
}

/// Pure transition-control availability derived from one UI state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionAvailability {
    base: TransitionBaseAvailability,
    pub alpha_fade: bool,
    pub wipe: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TransitionBaseAvailability {
    basic: bool,
    slide: bool,
    additive: AdditiveTransitionAvailability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdditiveTransitionAvailability {
    zoom: bool,
    stinger: bool,
}

impl TransitionAvailability {
    /// Returns whether protocol-independent Cut and Fade controls are available.
    #[must_use]
    pub const fn basic(self) -> bool {
        self.base.basic
    }

    /// Returns whether automatic Slide is available.
    #[must_use]
    pub const fn slide(self) -> bool {
        self.base.slide
    }

    /// Returns whether automatic Zoom is available.
    #[must_use]
    pub const fn zoom(self) -> bool {
        self.base.additive.zoom
    }

    /// Returns whether numbered Stinger controls are available.
    #[must_use]
    pub const fn stinger(self) -> bool {
        self.base.additive.stinger
    }
}

/// Session gates for automatic transition controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionGate {
    pub connection_status: StudioConnectionStatus,
    pub has_view: bool,
    pub can_transition: bool,
}

impl TransitionGate {
    #[must_use]
    pub const fn from_state(state: &StudioUiState) -> Self {
        Self {
            connection_status: state.connection_status,
            has_view: state.view.is_some(),
            can_transition: state.can_transition,
        }
    }
}

/// Pure manual T-bar control availability derived from replicated state and session gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionAvailability {
    pub start: bool,
    pub alpha_fade_start_exposed: bool,
    pub active_controls: bool,
}

/// Session and replicated-state gates for manual T-bar controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionGate {
    pub connection_status: StudioConnectionStatus,
    pub has_view: bool,
    pub can_transition: bool,
}

impl ManualTransitionGate {
    #[must_use]
    pub const fn from_state(state: &StudioUiState) -> Self {
        Self {
            connection_status: state.connection_status,
            has_view: state.view.is_some(),
            can_transition: state.can_transition,
        }
    }
}

/// Computes transition availability without drawing or dispatching intents.
#[must_use]
pub const fn transition_availability(gate: TransitionGate) -> TransitionAvailability {
    let base = gate.connection_status.controls_enabled() && gate.has_view && gate.can_transition;
    TransitionAvailability {
        base: TransitionBaseAvailability {
            basic: base,
            slide: base,
            additive: AdditiveTransitionAvailability {
                zoom: base,
                stinger: base,
            },
        },
        alpha_fade: base,
        wipe: base,
    }
}

/// Computes manual T-bar availability without drawing or dispatching intents.
#[must_use]
pub fn manual_transition_availability(
    gate: ManualTransitionGate,
    active: bool,
) -> ManualTransitionAvailability {
    let base = gate.connection_status.controls_enabled() && gate.has_view && gate.can_transition;
    ManualTransitionAvailability {
        start: base && !active,
        alpha_fade_start_exposed: true,
        active_controls: base && active,
    }
}

/// Independent tally facts for one input.
///
/// Desired flags are set only when the corresponding bus has not yet realized
/// that desired input. Separate flags preserve unusual but valid cases where
/// one input occupies more than one role.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TallyState(u8);

impl TallyState {
    const REALIZED_PROGRAM: u8 = 1 << 0;
    const REALIZED_PREVIEW: u8 = 1 << 1;
    const DESIRED_PROGRAM: u8 = 1 << 2;
    const DESIRED_PREVIEW: u8 = 1 << 3;

    /// Returns true when the input is realized on the Program bus.
    #[must_use]
    pub const fn is_realized_program(self) -> bool {
        self.0 & Self::REALIZED_PROGRAM != 0
    }

    /// Returns true when the input is realized on the Preview bus.
    #[must_use]
    pub const fn is_realized_preview(self) -> bool {
        self.0 & Self::REALIZED_PREVIEW != 0
    }

    /// Returns true when the input is desired, but not realized, on Program.
    #[must_use]
    pub const fn is_desired_program(self) -> bool {
        self.0 & Self::DESIRED_PROGRAM != 0
    }

    /// Returns true when the input is desired, but not realized, on Preview.
    #[must_use]
    pub const fn is_desired_preview(self) -> bool {
        self.0 & Self::DESIRED_PREVIEW != 0
    }

    /// Returns true when the input has no realized or pending bus role.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Returns a compact, deterministic operator label for this tally.
    #[must_use]
    pub fn operator_label(self) -> String {
        let mut labels = Vec::with_capacity(4);
        if self.is_realized_program() {
            labels.push("PROGRAM");
        }
        if self.is_realized_preview() {
            labels.push("PREVIEW");
        }
        if self.is_desired_program() {
            labels.push("PROGRAM DESIRED");
        }
        if self.is_desired_preview() {
            labels.push("PREVIEW DESIRED");
        }
        if labels.is_empty() {
            "NO TALLY".to_owned()
        } else {
            labels.join(" / ")
        }
    }
}

/// Formats a stable input label from its one-based ordinal and domain ID.
#[must_use]
pub fn input_label(ordinal: usize, input: InputId) -> String {
    format!("{ordinal:02} | Input {input}")
}

/// Computes realized and not-yet-realized desired tallies for an input.
#[must_use]
pub const fn tally_state(input: InputId, switcher: SwitcherState) -> TallyState {
    let mut flags = 0;
    if switcher.realized.program.get().get() == input.get().get() {
        flags |= TallyState::REALIZED_PROGRAM;
    }
    if switcher.realized.preview.get().get() == input.get().get() {
        flags |= TallyState::REALIZED_PREVIEW;
    }
    if switcher.desired.program.get().get() == input.get().get()
        && switcher.desired.program.get().get() != switcher.realized.program.get().get()
    {
        flags |= TallyState::DESIRED_PROGRAM;
    }
    if switcher.desired.preview.get().get() == input.get().get()
        && switcher.desired.preview.get().get() != switcher.realized.preview.get().get()
    {
        flags |= TallyState::DESIRED_PREVIEW;
    }
    TallyState(flags)
}

/// Stateful controller for the pure studio presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StudioShell {
    transition_duration_frames: u32,
    fade_to_black_duration_frames: u32,
}

impl StudioShell {
    pub const MIN_TRANSITION_DURATION_FRAMES: u32 = 1;
    pub const MAX_TRANSITION_DURATION_FRAMES: u32 = 3_600;
    pub const DEFAULT_TRANSITION_DURATION_FRAMES: u32 = 30;

    /// Returns the current transition duration in frames.
    #[must_use]
    pub const fn transition_duration_frames(&self) -> u32 {
        self.transition_duration_frames
    }

    /// Sets and clamps the transition duration to the supported frame range.
    pub fn set_transition_duration_frames(&mut self, duration_frames: u32) {
        self.transition_duration_frames = duration_frames.clamp(
            Self::MIN_TRANSITION_DURATION_FRAMES,
            Self::MAX_TRANSITION_DURATION_FRAMES,
        );
    }

    /// Returns the current Fade-to-Black duration in frames.
    #[must_use]
    pub const fn fade_to_black_duration_frames(&self) -> u32 {
        self.fade_to_black_duration_frames
    }

    /// Sets and clamps the Fade-to-Black duration to the supported frame range.
    pub fn set_fade_to_black_duration_frames(&mut self, duration_frames: u32) {
        self.fade_to_black_duration_frames = duration_frames.clamp(
            Self::MIN_TRANSITION_DURATION_FRAMES,
            Self::MAX_TRANSITION_DURATION_FRAMES,
        );
    }

    /// Draws one complete shell frame and returns operator intents in UI order.
    ///
    /// The UI must belong to an active `egui` pass, as is customary for
    /// immediate-mode drawing.
    pub fn draw(&mut self, ui: &mut Ui, state: &StudioUiState) -> Vec<StudioIntent> {
        apply_console_visuals(ui.ctx());
        self.set_transition_duration_frames(self.transition_duration_frames);
        self.set_fade_to_black_duration_frames(self.fade_to_black_duration_frames);

        let mut intents = Vec::new();
        Frame::new()
            .fill(GRAPHITE)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                draw_header(ui, state);
                draw_messages(ui, state);
                ui.add_space(8.0);
                draw_monitors(ui, state.view.as_ref());
                ui.add_space(8.0);
                draw_transition_row(ui, self, state, &mut intents);
                ui.add_space(8.0);
                draw_overlays(ui, state, self.transition_duration_frames, &mut intents);
                ui.add_space(8.0);
                fade_to_black::draw_fade_to_black(
                    ui,
                    &mut self.fade_to_black_duration_frames,
                    state,
                    &mut intents,
                );
                self.set_fade_to_black_duration_frames(self.fade_to_black_duration_frames);
                ui.add_space(8.0);
                draw_manual_transition(ui, state, &mut intents);
                ui.add_space(8.0);
                draw_inputs(ui, state, &mut intents);
            });
        intents
    }
}

fn draw_overlays(
    ui: &mut Ui,
    state: &StudioUiState,
    duration_frames: u32,
    intents: &mut Vec<StudioIntent>,
) {
    let enabled =
        state.connection_status.controls_enabled() && state.can_transition && state.view.is_some();
    Frame::new()
        .fill(GRAPHITE_RAISED)
        .stroke(Stroke::new(1.0, Color32::from_rgb(67, 61, 44)))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("OVERLAYS").small().strong().color(AMBER));
                for channel_number in 1..=8 {
                    draw_overlay_channel(
                        ui,
                        state,
                        channel_number,
                        duration_frames,
                        enabled,
                        intents,
                    );
                }
            });
        });
}

fn draw_overlay_channel(
    ui: &mut Ui,
    state: &StudioUiState,
    channel_number: u8,
    duration_frames: u32,
    enabled: bool,
    intents: &mut Vec<StudioIntent>,
) {
    let channel = WireOverlayChannelId::new(channel_number)
        .expect("Studio renders only overlay channels 1 through 8");
    let status = state.view.as_ref().and_then(|view| {
        view.desired_overlays
            .iter()
            .find(|overlay| overlay.channel == channel_number)
    });
    let source = state
        .view
        .as_ref()
        .map(|view| view.switcher.desired.preview);
    if ui
        .add_enabled(
            enabled && source.is_some(),
            Button::new(RichText::new(format!("O{channel_number} TAKE")).strong()),
        )
        .on_hover_text("Take the current Preview source on this overlay channel")
        .clicked()
        && let Some(source) = source
    {
        intents.push(StudioIntent::TakeOverlay { channel, source });
    }
    if ui
        .add_enabled(
            enabled && source.is_some(),
            Button::new(format!(
                "Q+{}",
                status.map_or(0, |value| value.queued_sources.len())
            )),
        )
        .on_hover_text("Append the current Preview source to this overlay queue")
        .clicked()
        && let Some(source) = source
    {
        intents.push(StudioIntent::QueueOverlay { channel, source });
    }
    if ui
        .add_enabled(
            enabled && status.is_some_and(|value| !value.queued_sources.is_empty()),
            Button::new("NEXT"),
        )
        .clicked()
    {
        intents.push(StudioIntent::TakeNextOverlay { channel });
    }
    let transition = status.map_or(OverlayTransitionKind::Cut, |overlay| overlay.transition);
    if ui
        .add_enabled(
            enabled,
            Button::new(match transition {
                OverlayTransitionKind::Cut => "CUT",
                OverlayTransitionKind::Fade => "FADE",
            }),
        )
        .on_hover_text("Toggle this overlay channel between Cut and Fade")
        .clicked()
    {
        intents.push(StudioIntent::ConfigureOverlayTransition {
            channel,
            transition: match transition {
                OverlayTransitionKind::Cut => OverlayTransitionKind::Fade,
                OverlayTransitionKind::Fade => OverlayTransitionKind::Cut,
            },
            duration_frames,
        });
    }
    let position = status.map_or(OverlayPositionPreset::FullFrame, |overlay| overlay.position);
    let border = status.map_or(OverlayBorderPreset::None, |overlay| overlay.border);
    if ui
        .add_enabled(enabled, Button::new(overlay_position_label(position)))
        .on_hover_text("Cycle this overlay channel's position preset")
        .clicked()
    {
        intents.push(StudioIntent::ConfigureOverlayAppearance {
            channel,
            position: next_overlay_position(position),
            border,
        });
    }
    if ui
        .add_enabled(enabled, Button::new(overlay_border_label(border)))
        .on_hover_text("Cycle this overlay channel's white border preset")
        .clicked()
    {
        intents.push(StudioIntent::ConfigureOverlayAppearance {
            channel,
            position,
            border: next_overlay_border(border),
        });
    }
    if ui
        .add_enabled(
            enabled && status.is_some_and(|overlay| overlay.active),
            Button::new(RichText::new(format!("O{channel_number} OFF")).strong()),
        )
        .on_hover_text("Remove this overlay channel from Program")
        .clicked()
    {
        intents.push(StudioIntent::OverlayOff { channel });
    }
}

const fn overlay_position_label(position: OverlayPositionPreset) -> &'static str {
    match position {
        OverlayPositionPreset::FullFrame => "FULL",
        OverlayPositionPreset::TopLeft => "TL",
        OverlayPositionPreset::TopRight => "TR",
        OverlayPositionPreset::BottomLeft => "BL",
        OverlayPositionPreset::BottomRight => "BR",
    }
}

const fn next_overlay_position(position: OverlayPositionPreset) -> OverlayPositionPreset {
    match position {
        OverlayPositionPreset::FullFrame => OverlayPositionPreset::TopLeft,
        OverlayPositionPreset::TopLeft => OverlayPositionPreset::TopRight,
        OverlayPositionPreset::TopRight => OverlayPositionPreset::BottomLeft,
        OverlayPositionPreset::BottomLeft => OverlayPositionPreset::BottomRight,
        OverlayPositionPreset::BottomRight => OverlayPositionPreset::FullFrame,
    }
}

const fn overlay_border_label(border: OverlayBorderPreset) -> &'static str {
    match border {
        OverlayBorderPreset::None => "NO BORDER",
        OverlayBorderPreset::ThinWhite => "THIN",
        OverlayBorderPreset::ThickWhite => "THICK",
    }
}

const fn next_overlay_border(border: OverlayBorderPreset) -> OverlayBorderPreset {
    match border {
        OverlayBorderPreset::None => OverlayBorderPreset::ThinWhite,
        OverlayBorderPreset::ThinWhite => OverlayBorderPreset::ThickWhite,
        OverlayBorderPreset::ThickWhite => OverlayBorderPreset::None,
    }
}

fn draw_manual_transition(ui: &mut Ui, state: &StudioUiState, intents: &mut Vec<StudioIntent>) {
    let desired = state
        .view
        .as_ref()
        .map_or(ManualTransitionStatus::Inactive, |view| {
            view.switcher.desired_manual_transition
        });
    let realized = state
        .view
        .as_ref()
        .map_or(ManualTransitionStatus::Inactive, |view| {
            view.switcher.realized_manual_transition
        });
    let active = matches!(desired, ManualTransitionStatus::Active(_));
    let availability =
        manual_transition_availability(ManualTransitionGate::from_state(state), active);

    Frame::new()
        .fill(GRAPHITE_RAISED)
        .stroke(Stroke::new(1.0, Color32::from_rgb(67, 61, 44)))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            draw_manual_transition_status(ui, desired, realized);
            ui.add_space(6.0);
            draw_manual_transition_controls(ui, desired, availability, intents);
        });
}

fn draw_manual_transition_status(
    ui: &mut Ui,
    desired: ManualTransitionStatus,
    realized: ManualTransitionStatus,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("MANUAL T-BAR").small().strong().color(AMBER));
        ui.label(RichText::new(format!("DESIRED {}", manual_transition_label(desired))).small());
        ui.label(
            RichText::new(format!("REALIZED {}", manual_transition_label(realized)))
                .small()
                .color(MUTED),
        );
    });
}

fn draw_manual_transition_controls(
    ui: &mut Ui,
    desired: ManualTransitionStatus,
    availability: ManualTransitionAvailability,
    intents: &mut Vec<StudioIntent>,
) {
    ui.horizontal_wrapped(|ui| {
        draw_manual_start_buttons(
            ui,
            availability.start,
            availability.alpha_fade_start_exposed,
            intents,
        );
        draw_manual_position(ui, desired, availability.active_controls, intents);
        draw_manual_terminal_buttons(ui, availability.active_controls, intents);
    });
}

fn draw_manual_start_buttons(
    ui: &mut Ui,
    enabled: bool,
    alpha_fade_exposed: bool,
    intents: &mut Vec<StudioIntent>,
) {
    for (label, kind, tooltip) in [
        (
            "START FADE T-BAR",
            ManualTransitionKind::Fade,
            "Start a reversible manual Fade transition",
        ),
        (
            "START WIPE T-BAR",
            ManualTransitionKind::Wipe,
            "Start a reversible manual Wipe transition",
        ),
    ] {
        let response = ui
            .add_enabled(enabled, Button::new(RichText::new(label).strong()))
            .on_hover_text(tooltip);
        if response.clicked() {
            intents.push(StudioIntent::StartManualTransition { kind });
        }
    }
    if alpha_fade_exposed {
        let response = ui
            .add_enabled(
                enabled,
                Button::new(RichText::new("START ALPHA FADE T-BAR").strong()),
            )
            .on_hover_text("Start a reversible manual AlphaFade transition");
        if response.clicked() {
            intents.push(StudioIntent::StartManualTransition {
                kind: ManualTransitionKind::AlphaFade,
            });
        }
    }
}

fn draw_manual_position(
    ui: &mut Ui,
    desired: ManualTransitionStatus,
    enabled: bool,
    intents: &mut Vec<StudioIntent>,
) {
    let position_label = ui.label(
        RichText::new("POSITION (BASIS POINTS)")
            .small()
            .color(MUTED),
    );
    let mut position = desired_manual_position(desired).basis_points();
    let response = ui
        .add_enabled(
            enabled,
            DragValue::new(&mut position).range(
                ManualTransitionPosition::START.basis_points()
                    ..=ManualTransitionPosition::END.basis_points(),
            ),
        )
        .labelled_by(position_label.id)
        .on_hover_text(
            "Exact T-bar position from 0 through 10000 basis points; lower values reverse",
        );
    if response.changed() {
        intents.push(StudioIntent::SetManualTransitionPosition {
            position: ManualTransitionPosition::new(position)
                .expect("DragValue range bounds manual positions"),
        });
    }
}

fn draw_manual_terminal_buttons(ui: &mut Ui, enabled: bool, intents: &mut Vec<StudioIntent>) {
    for (label, tooltip, intent) in [
        (
            "COMMIT T-BAR",
            "Commit the active manual transition",
            StudioIntent::CommitManualTransition,
        ),
        (
            "CANCEL T-BAR",
            "Cancel the active manual transition",
            StudioIntent::CancelManualTransition,
        ),
    ] {
        let response = ui
            .add_enabled(enabled, Button::new(RichText::new(label).strong()))
            .on_hover_text(tooltip);
        if response.clicked() {
            intents.push(intent);
        }
    }
}

const fn desired_manual_position(status: ManualTransitionStatus) -> ManualTransitionPosition {
    match status {
        ManualTransitionStatus::Inactive => ManualTransitionPosition::START,
        ManualTransitionStatus::Active(state) => state.position,
    }
}

fn manual_transition_label(status: ManualTransitionStatus) -> String {
    match status {
        ManualTransitionStatus::Inactive => "INACTIVE".to_owned(),
        ManualTransitionStatus::Active(state) => format!(
            "{} {}->{} @ {} BP",
            match state.kind {
                ManualTransitionKind::Fade => "FADE",
                ManualTransitionKind::Wipe => "WIPE",
                ManualTransitionKind::AlphaFade => "ALPHA FADE",
            },
            state.from,
            state.to,
            state.position.basis_points(),
        ),
    }
}

impl Default for StudioShell {
    fn default() -> Self {
        Self {
            transition_duration_frames: Self::DEFAULT_TRANSITION_DURATION_FRAMES,
            fade_to_black_duration_frames: Self::DEFAULT_TRANSITION_DURATION_FRAMES,
        }
    }
}

fn apply_console_visuals(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = GRAPHITE;
    visuals.window_fill = GRAPHITE_RAISED;
    visuals.override_text_color = Some(TEXT);
    visuals.widgets.noninteractive.bg_fill = GRAPHITE_RAISED;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(35, 38, 41);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 52, 55);
    visuals.selection.bg_fill = AMBER;
    context.set_visuals(visuals);
}

fn draw_header(ui: &mut Ui, state: &StudioUiState) {
    Frame::new()
        .fill(GRAPHITE_RAISED)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let show_name = state
                    .view
                    .as_ref()
                    .map(|view| view.show_name.as_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("FreeMix Studio");
                ui.label(RichText::new(show_name).strong().size(18.0));
                ui.separator();
                ui.label(
                    RichText::new(state.connection_status.operator_label())
                        .strong()
                        .color(connection_color(state.connection_status)),
                );
                if let Some(view) = &state.view {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("REV {}", view.cursor.revision.get()))
                            .small()
                            .color(MUTED),
                    );
                }
                if state.pending_commands > 0 {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} PENDING", state.pending_commands))
                            .small()
                            .strong()
                            .color(AMBER),
                    );
                }
            });
        });
}

fn draw_messages(ui: &mut Ui, state: &StudioUiState) {
    if let Some(error) = &state.error {
        ui.label(
            RichText::new(format!("ERROR | {error}"))
                .color(ERROR)
                .strong(),
        );
    }
    if let Some(notice) = &state.notice {
        ui.label(RichText::new(format!("NOTICE | {notice}")).color(AMBER));
    }
}

fn draw_monitors(ui: &mut Ui, view: Option<&ClientView>) {
    let (preview, program) = view.map_or((None, None), |view| {
        (
            Some((view.switcher.desired.preview, view)),
            Some((view.switcher.realized.program, view)),
        )
    });
    if ui.available_width() >= NARROW_MONITOR_WIDTH {
        ui.columns(2, |columns| {
            draw_monitor(&mut columns[0], "PREVIEW", PREVIEW, preview);
            draw_monitor(&mut columns[1], "PROGRAM", PROGRAM, program);
        });
    } else {
        draw_monitor(ui, "PREVIEW", PREVIEW, preview);
        ui.add_space(6.0);
        draw_monitor(ui, "PROGRAM", PROGRAM, program);
    }
}

fn draw_monitor(
    ui: &mut Ui,
    bus_label: &str,
    color: Color32,
    selected: Option<(InputId, &ClientView)>,
) {
    Frame::new()
        .fill(MONITOR_BLACK)
        .stroke(Stroke::new(2.0, color))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_min_height(154.0);
            ui.label(RichText::new(bus_label).strong().color(color));
            ui.add_space(28.0);
            let selected_label = selected.map_or_else(
                || "NO INPUT STATE".to_owned(),
                |(input, view)| label_for_input(view, input),
            );
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(selected_label).strong().size(17.0));
                ui.add_space(10.0);
                ui.label(
                    RichText::new("VIDEO PREVIEW DELIVERY PENDING")
                        .small()
                        .color(MUTED),
                );
            });
        });
}

fn draw_transition_row(
    ui: &mut Ui,
    shell: &mut StudioShell,
    state: &StudioUiState,
    intents: &mut Vec<StudioIntent>,
) {
    let availability = transition_availability(TransitionGate::from_state(state));
    Frame::new()
        .fill(GRAPHITE_RAISED)
        .stroke(Stroke::new(1.0, Color32::from_rgb(67, 61, 44)))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("TRANSITION").small().strong().color(AMBER));
                if transition_button(
                    ui,
                    availability.alpha_fade,
                    "ALPHA",
                    "AlphaFade Preview to Program",
                ) {
                    intents.push(StudioIntent::AlphaFade {
                        duration_frames: shell.transition_duration_frames,
                    });
                }
                if transition_button(
                    ui,
                    availability.slide(),
                    "SLIDE",
                    "Slide Preview to Program",
                ) {
                    intents.push(StudioIntent::Slide {
                        duration_frames: shell.transition_duration_frames,
                    });
                }
                if transition_button(ui, availability.zoom(), "ZOOM", "Zoom Preview to Program") {
                    intents.push(StudioIntent::Zoom {
                        duration_frames: shell.transition_duration_frames,
                    });
                }
                for slot in 1..=8 {
                    let ready = state.view.as_ref().is_some_and(|view| {
                        view.stingers.iter().any(|status| {
                            status.slot == slot && status.readiness == StingerReadiness::Ready
                        })
                    });
                    if transition_button(
                        ui,
                        availability.stinger() && ready,
                        &format!("S{slot}"),
                        &format!("Fire Stinger slot {slot}"),
                    ) {
                        intents.push(StudioIntent::Stinger {
                            slot: WireStingerSlotId::new(slot)
                                .expect("Studio renders only Stinger slots 1 through 8"),
                            duration_frames: shell.transition_duration_frames,
                        });
                    }
                }
                if transition_button(ui, availability.basic(), "CUT", "Cut Preview to Program") {
                    intents.push(StudioIntent::Cut);
                }
                if transition_button(ui, availability.basic(), "FADE", "Fade Preview to Program") {
                    intents.push(StudioIntent::Fade {
                        duration_frames: shell.transition_duration_frames,
                    });
                }
                if transition_button(ui, availability.wipe, "WIPE", "Wipe Preview to Program") {
                    intents.push(StudioIntent::Wipe {
                        duration_frames: shell.transition_duration_frames,
                    });
                }
                ui.label(RichText::new("DURATION").small().color(MUTED));
                ui.add_enabled(
                    availability.basic(),
                    DragValue::new(&mut shell.transition_duration_frames)
                        .range(
                            StudioShell::MIN_TRANSITION_DURATION_FRAMES
                                ..=StudioShell::MAX_TRANSITION_DURATION_FRAMES,
                        )
                        .suffix(" FRAMES")
                        .speed(1.0),
                );
                shell.set_transition_duration_frames(shell.transition_duration_frames);
            });
        });
}

fn transition_button(ui: &mut Ui, enabled: bool, label: &str, hover_text: &str) -> bool {
    ui.add_enabled(
        enabled,
        Button::new(RichText::new(label).strong())
            .fill(Color32::from_rgb(98, 66, 17))
            .min_size(Vec2::new(92.0, 32.0)),
    )
    .on_hover_text(hover_text)
    .clicked()
}

fn draw_inputs(ui: &mut Ui, state: &StudioUiState, intents: &mut Vec<StudioIntent>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("INPUT BANK").small().strong());
        ui.separator();
        let count = state.view.as_ref().map_or(0, |view| view.inputs.len());
        ui.label(
            RichText::new(format!("{count} SOURCES"))
                .small()
                .color(MUTED),
        );
    });
    ui.add_space(4.0);

    let enabled = state.connection_status.controls_enabled()
        && state.view.is_some()
        && state.can_select_preview;
    ScrollArea::vertical()
        .id_salt("studio-input-bank")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let Some(view) = &state.view else {
                ui.label(RichText::new("INPUT STATE UNAVAILABLE").color(MUTED));
                return;
            };
            let columns = dynamic_columns(ui.available_width(), ui.spacing().item_spacing.x);
            Grid::new("studio-input-grid")
                .num_columns(columns)
                .spacing(Vec2::new(6.0, 6.0))
                .show(ui, |ui| {
                    for (index, input) in view.inputs.iter().copied().enumerate() {
                        let tally = tally_state(input, view.switcher);
                        let tile_text = format!(
                            "{}\n{}",
                            input_label(index + 1, input),
                            tally.operator_label()
                        );
                        let response = ui.add_enabled(
                            enabled,
                            Button::new(RichText::new(tile_text).color(tally_color(tally)))
                                .fill(tally_fill(tally))
                                .stroke(Stroke::new(1.0, tally_color(tally)))
                                .min_size(Vec2::new(MIN_TILE_WIDTH, 52.0)),
                        );
                        if response.clicked() {
                            intents.push(StudioIntent::SelectPreview(input));
                        }
                        if (index + 1) % columns == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
}

fn dynamic_columns(available_width: f32, spacing: f32) -> usize {
    let mut columns = 1;
    let mut occupied = MIN_TILE_WIDTH;
    while occupied + spacing + MIN_TILE_WIDTH <= available_width {
        columns += 1;
        occupied += spacing + MIN_TILE_WIDTH;
    }
    columns
}

fn label_for_input(view: &ClientView, input: InputId) -> String {
    view.inputs
        .iter()
        .position(|candidate| *candidate == input)
        .map_or_else(
            || format!("Input {input}"),
            |index| input_label(index + 1, input),
        )
}

const fn connection_color(status: StudioConnectionStatus) -> Color32 {
    match status {
        StudioConnectionStatus::Ready => PREVIEW,
        StudioConnectionStatus::Failed | StudioConnectionStatus::Incompatible => ERROR,
        StudioConnectionStatus::Disconnected => MUTED,
        StudioConnectionStatus::Launching
        | StudioConnectionStatus::Connecting
        | StudioConnectionStatus::Synchronizing
        | StudioConnectionStatus::Backoff => AMBER,
    }
}

const fn tally_color(tally: TallyState) -> Color32 {
    if tally.is_realized_program() {
        PROGRAM
    } else if tally.is_realized_preview() {
        PREVIEW
    } else if tally.is_desired_program() {
        Color32::from_rgb(191, 91, 64)
    } else if tally.is_desired_preview() {
        Color32::from_rgb(86, 163, 112)
    } else {
        MUTED
    }
}

const fn tally_fill(tally: TallyState) -> Color32 {
    if tally.is_realized_program() {
        Color32::from_rgb(66, 20, 23)
    } else if tally.is_realized_preview() {
        Color32::from_rgb(15, 55, 35)
    } else if tally.is_desired_program() {
        Color32::from_rgb(48, 28, 22)
    } else if tally.is_desired_preview() {
        Color32::from_rgb(23, 43, 30)
    } else {
        GRAPHITE_RAISED
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use fm_ui_model::{ActiveManualTransition, BusSelection};

    use super::*;

    fn input(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    fn switcher(desired: (u128, u128), realized: (u128, u128)) -> SwitcherState {
        SwitcherState {
            desired: BusSelection::new(input(desired.0), input(desired.1)),
            realized: BusSelection::new(input(realized.0), input(realized.1)),
            desired_manual_transition: ManualTransitionStatus::Inactive,
            realized_manual_transition: ManualTransitionStatus::Inactive,
            desired_fade_to_black: fm_protocol::FadeToBlackState {
                target_active: false,
                position: fm_protocol::FadeToBlackPosition::LIVE,
            },
            realized_fade_to_black: fm_protocol::FadeToBlackState {
                target_active: false,
                position: fm_protocol::FadeToBlackPosition::LIVE,
            },
            runtime_generation: None,
        }
    }

    #[test]
    fn only_ready_enables_connection_controls() {
        let statuses = [
            StudioConnectionStatus::Launching,
            StudioConnectionStatus::Connecting,
            StudioConnectionStatus::Synchronizing,
            StudioConnectionStatus::Backoff,
            StudioConnectionStatus::Disconnected,
            StudioConnectionStatus::Failed,
            StudioConnectionStatus::Incompatible,
        ];
        assert!(StudioConnectionStatus::Ready.controls_enabled());
        for status in statuses {
            assert!(!status.controls_enabled(), "{status:?}");
            assert!(!status.operator_label().is_empty());
        }
    }

    #[test]
    fn switcher_permissions_are_explicit_and_separate() {
        let state = StudioUiState::new(StudioConnectionStatus::Ready)
            .with_switcher_permissions(true, false);
        assert!(state.can_select_preview);
        assert!(!state.can_transition);
    }

    #[test]
    fn input_labels_preserve_ordinal_and_full_width_id() {
        let full_width = input(u128::MAX);
        assert_eq!(
            input_label(1, full_width),
            format!("01 | Input {}", u128::MAX)
        );
        assert_eq!(input_label(128, input(7)), "128 | Input 7");
    }

    #[test]
    fn tally_reports_realized_program_and_preview() {
        let state = switcher((1, 2), (1, 2));
        let program = tally_state(input(1), state);
        assert!(program.is_realized_program());
        assert!(!program.is_realized_preview());
        assert!(!program.is_desired_program());
        assert!(!program.is_desired_preview());

        let preview = tally_state(input(2), state);
        assert!(preview.is_realized_preview());
        assert!(!preview.is_realized_program());
        assert!(!preview.is_desired_program());
        assert!(!preview.is_desired_preview());
        assert!(tally_state(input(3), state).is_none());
    }

    #[test]
    fn tally_reports_desired_buses_until_each_is_realized() {
        let state = switcher((3, 4), (1, 2));
        assert!(tally_state(input(3), state).is_desired_program());
        assert!(tally_state(input(4), state).is_desired_preview());
        assert!(tally_state(input(1), state).is_realized_program());
        assert!(tally_state(input(2), state).is_realized_preview());
    }

    #[test]
    fn tally_preserves_combined_roles() {
        let state = switcher((2, 3), (1, 2));
        let tally = tally_state(input(2), state);
        assert!(tally.is_realized_preview());
        assert!(tally.is_desired_program());
        assert!(!tally.is_realized_program());
        assert!(!tally.is_desired_preview());
        assert_eq!(tally.operator_label(), "PREVIEW / PROGRAM DESIRED");
    }

    #[test]
    fn transition_duration_is_shared_and_bounded() {
        let mut shell = StudioShell::default();
        assert_eq!(
            shell.transition_duration_frames(),
            StudioShell::DEFAULT_TRANSITION_DURATION_FRAMES
        );
        shell.set_transition_duration_frames(0);
        assert_eq!(
            shell.transition_duration_frames(),
            StudioShell::MIN_TRANSITION_DURATION_FRAMES
        );
        shell.set_transition_duration_frames(u32::MAX);
        assert_eq!(
            shell.transition_duration_frames(),
            StudioShell::MAX_TRANSITION_DURATION_FRAMES
        );
        assert_eq!(
            shell.fade_to_black_duration_frames(),
            StudioShell::DEFAULT_TRANSITION_DURATION_FRAMES
        );
        shell.set_fade_to_black_duration_frames(0);
        assert_eq!(
            shell.fade_to_black_duration_frames(),
            StudioShell::MIN_TRANSITION_DURATION_FRAMES
        );
        shell.set_fade_to_black_duration_frames(u32::MAX);
        assert_eq!(
            shell.fade_to_black_duration_frames(),
            StudioShell::MAX_TRANSITION_DURATION_FRAMES
        );
    }

    #[test]
    fn transition_availability_requires_ready_view_and_permission() {
        let base = TransitionGate {
            connection_status: StudioConnectionStatus::Ready,
            has_view: true,
            can_transition: true,
        };
        assert_eq!(
            transition_availability(base),
            TransitionAvailability {
                base: TransitionBaseAvailability {
                    basic: true,
                    slide: true,
                    additive: AdditiveTransitionAvailability {
                        zoom: true,
                        stinger: true,
                    },
                },
                alpha_fade: true,
                wipe: true,
            }
        );
        let supported = transition_availability(base);
        assert!(supported.wipe);
        assert!(supported.alpha_fade);
        assert!(supported.slide());
        assert!(supported.zoom());
        assert!(supported.stinger());
        assert!(supported.basic());
        for gate in [
            TransitionGate {
                connection_status: StudioConnectionStatus::Connecting,
                ..base
            },
            TransitionGate {
                has_view: false,
                ..base
            },
            TransitionGate {
                can_transition: false,
                ..base
            },
        ] {
            assert_eq!(
                transition_availability(gate),
                TransitionAvailability {
                    base: TransitionBaseAvailability {
                        basic: false,
                        slide: false,
                        additive: AdditiveTransitionAvailability {
                            zoom: false,
                            stinger: false,
                        },
                    },
                    alpha_fade: false,
                    wipe: false,
                }
            );
        }
    }

    #[test]
    fn manual_t_bar_availability_requires_ready_view_permission_and_active_state() {
        let available = ManualTransitionGate {
            connection_status: StudioConnectionStatus::Ready,
            has_view: true,
            can_transition: true,
        };
        assert_eq!(
            manual_transition_availability(available, false),
            ManualTransitionAvailability {
                start: true,
                alpha_fade_start_exposed: true,
                active_controls: false,
            }
        );
        assert_eq!(
            manual_transition_availability(available, true),
            ManualTransitionAvailability {
                start: false,
                alpha_fade_start_exposed: true,
                active_controls: true,
            }
        );
        for gate in [
            ManualTransitionGate {
                connection_status: StudioConnectionStatus::Connecting,
                ..available
            },
            ManualTransitionGate {
                has_view: false,
                ..available
            },
            ManualTransitionGate {
                can_transition: false,
                ..available
            },
        ] {
            assert_eq!(
                manual_transition_availability(gate, true),
                ManualTransitionAvailability {
                    start: false,
                    alpha_fade_start_exposed: true,
                    active_controls: false,
                }
            );
        }
    }

    #[test]
    fn manual_t_bar_labels_and_positions_come_from_replicated_model() {
        let active = ManualTransitionStatus::Active(ActiveManualTransition {
            kind: ManualTransitionKind::Wipe,
            from: input(1),
            to: input(2),
            interval_start: ManualTransitionPosition::new(8_000).unwrap(),
            position: ManualTransitionPosition::new(2_500).unwrap(),
        });
        assert_eq!(desired_manual_position(active).basis_points(), 2_500);
        assert_eq!(manual_transition_label(active), "WIPE 1->2 @ 2500 BP");
        let alpha_fade = ManualTransitionStatus::Active(ActiveManualTransition {
            kind: ManualTransitionKind::AlphaFade,
            from: input(1),
            to: input(2),
            interval_start: ManualTransitionPosition::new(8_000).unwrap(),
            position: ManualTransitionPosition::new(2_500).unwrap(),
        });
        assert_eq!(
            manual_transition_label(alpha_fade),
            "ALPHA FADE 1->2 @ 2500 BP"
        );
        assert_eq!(
            desired_manual_position(ManualTransitionStatus::Inactive),
            ManualTransitionPosition::START
        );
        assert_eq!(
            manual_transition_label(ManualTransitionStatus::Inactive),
            "INACTIVE"
        );
    }

    #[test]
    fn intents_are_typed_and_comparable() {
        assert_eq!(
            StudioIntent::SelectPreview(input(9)),
            StudioIntent::SelectPreview(input(9))
        );
        assert_ne!(StudioIntent::Cut, StudioIntent::Fade { duration_frames: 1 });
        assert_ne!(
            StudioIntent::Fade {
                duration_frames: 30
            },
            StudioIntent::AlphaFade {
                duration_frames: 30
            }
        );
        assert_ne!(
            StudioIntent::AlphaFade {
                duration_frames: 30
            },
            StudioIntent::Slide {
                duration_frames: 30
            }
        );
        assert_ne!(
            StudioIntent::Slide {
                duration_frames: 30
            },
            StudioIntent::Zoom {
                duration_frames: 30
            }
        );
        assert_ne!(
            StudioIntent::Zoom {
                duration_frames: 30
            },
            StudioIntent::Stinger {
                slot: WireStingerSlotId::new(1).unwrap(),
                duration_frames: 30
            }
        );
        assert_ne!(
            StudioIntent::Stinger {
                slot: WireStingerSlotId::new(1).unwrap(),
                duration_frames: 30
            },
            StudioIntent::Wipe {
                duration_frames: 30
            }
        );
        assert_ne!(
            StudioIntent::FadeToBlack {
                active: true,
                duration_frames: 30,
            },
            StudioIntent::FadeToBlack {
                active: false,
                duration_frames: 30,
            }
        );
        assert_eq!(
            StudioIntent::SetManualTransitionPosition {
                position: ManualTransitionPosition::START,
            },
            StudioIntent::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(0).unwrap(),
            }
        );
        assert_ne!(
            StudioIntent::SetManualTransitionPosition {
                position: ManualTransitionPosition::END,
            },
            StudioIntent::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(2_500).unwrap(),
            }
        );
        assert_eq!(
            StudioIntent::Fade {
                duration_frames: 30
            },
            StudioIntent::Fade {
                duration_frames: 30
            }
        );
    }

    #[test]
    fn draw_smoke_test_without_client_state() {
        let context = egui::Context::default();
        let state = StudioUiState::new(StudioConnectionStatus::Launching);
        let mut shell = StudioShell::default();
        context.begin_pass(egui::RawInput::default());
        let mut ui = egui::Ui::new(
            context.clone(),
            egui::Id::new("fm-ui-egui-smoke-test"),
            egui::UiBuilder::new()
                .layer_id(egui::LayerId::background())
                .max_rect(context.viewport_rect()),
        );
        let intents = shell.draw(&mut ui, &state);
        let _output = context.end_pass();
        assert!(intents.is_empty());
    }
}
