//! Collaborative stroke model, deterministic reference rendering, and undo.
//!
//! Coordinates and stroke widths are normalized to the board. A point at
//! `(0, 0)` maps to the center of the top-left pixel and `(1, 1)` maps to the
//! center of the bottom-right pixel. Width is relative to the shorter render
//! dimension.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u128);

        impl $name {
            #[must_use]
            pub const fn new(value: u128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u128 {
                self.0
            }
        }
    };
}

id_type!(BoardId);
id_type!(StrokeId);
id_type!(PointId);
id_type!(AuthorId);
id_type!(OperationId);

/// A point in normalized board coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub id: PointId,
    pub x: f32,
    pub y: f32,
}

impl Point {
    /// Creates a point if both coordinates are finite and in `0.0..=1.0`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidPoint`] for a non-finite or out-of-range
    /// coordinate.
    pub fn new(id: PointId, x: f32, y: f32) -> Result<Self, ModelError> {
        let point = Self { id, x, y };
        point.validate()?;
        Ok(point)
    }

    fn validate(self) -> Result<(), ModelError> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !(0.0..=1.0).contains(&self.x)
            || !(0.0..=1.0).contains(&self.y)
        {
            return Err(ModelError::InvalidPoint);
        }
        Ok(())
    }
}

/// Non-premultiplied sRGB color channels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    #[must_use]
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

/// Visual properties shared by all stroke shapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrokeStyle {
    pub color: Color,
    /// Width relative to the shorter board dimension, in `0.0..=1.0`.
    pub width: f32,
    /// Additional alpha multiplier, in `0.0..=1.0`.
    pub opacity: f32,
}

impl StrokeStyle {
    /// Creates a validated stroke style.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidWidth`] or [`ModelError::InvalidOpacity`]
    /// when the corresponding normalized value is invalid.
    pub fn new(color: Color, width: f32, opacity: f32) -> Result<Self, ModelError> {
        let style = Self {
            color,
            width,
            opacity,
        };
        style.validate()?;
        Ok(style)
    }

    fn validate(self) -> Result<(), ModelError> {
        if !self.width.is_finite() || self.width <= 0.0 || self.width > 1.0 {
            return Err(ModelError::InvalidWidth);
        }
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) {
            return Err(ModelError::InvalidOpacity);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrokeKind {
    Freehand,
    Line,
    Arrow,
    Rectangle,
    Ellipse,
}

/// One drawable object. Geometric shapes use their first and last points.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    id: StrokeId,
    author: AuthorId,
    kind: StrokeKind,
    style: StrokeStyle,
    points: Vec<Point>,
    finalized: bool,
}

impl Stroke {
    /// Creates a non-finalized stroke.
    ///
    /// # Errors
    ///
    /// Returns an error if the style or points are invalid. At least one point
    /// is required and point IDs must be unique within the stroke.
    pub fn new(
        id: StrokeId,
        author: AuthorId,
        kind: StrokeKind,
        style: StrokeStyle,
        points: Vec<Point>,
    ) -> Result<Self, ModelError> {
        style.validate()?;
        validate_points(&points)?;
        Ok(Self {
            id,
            author,
            kind,
            style,
            points,
            finalized: false,
        })
    }

    #[must_use]
    pub const fn id(&self) -> StrokeId {
        self.id
    }

    #[must_use]
    pub const fn author(&self) -> AuthorId {
        self.author
    }

    #[must_use]
    pub const fn kind(&self) -> StrokeKind {
        self.kind
    }

    #[must_use]
    pub const fn style(&self) -> StrokeStyle {
        self.style
    }

    #[must_use]
    pub fn points(&self) -> &[Point] {
        &self.points
    }

    #[must_use]
    pub const fn is_finalized(&self) -> bool {
        self.finalized
    }

    fn validate(&self, max_points: usize) -> Result<(), ModelError> {
        self.style.validate()?;
        validate_points(&self.points)?;
        if self.points.len() > max_points {
            return Err(ModelError::PointLimitExceeded);
        }
        if self.finalized && self.kind != StrokeKind::Freehand && self.points.len() < 2 {
            return Err(ModelError::NotEnoughPoints);
        }
        Ok(())
    }
}

fn validate_points(points: &[Point]) -> Result<(), ModelError> {
    if points.is_empty() {
        return Err(ModelError::NotEnoughPoints);
    }
    for (index, point) in points.iter().copied().enumerate() {
        point.validate()?;
        if points[..index]
            .iter()
            .any(|previous| previous.id == point.id)
        {
            return Err(ModelError::DuplicatePoint(point.id));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UndoScope {
    /// Undo the latest retained mutation by this operation's author.
    Author,
    /// Undo the latest retained mutation by any author.
    Global,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OperationKind {
    Add(Stroke),
    /// Appends points to an existing, non-finalized stroke.
    Update {
        stroke_id: StrokeId,
        points: Vec<Point>,
    },
    Finalize {
        stroke_id: StrokeId,
    },
    Delete {
        stroke_id: StrokeId,
    },
    Clear,
    Undo(UndoScope),
}

/// A totally ordered operation for one board.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    pub id: OperationId,
    pub board_id: BoardId,
    pub sequence: u64,
    pub author_id: AuthorId,
    pub kind: OperationKind,
}

impl Operation {
    #[must_use]
    pub const fn new(
        id: OperationId,
        board_id: BoardId,
        sequence: u64,
        author_id: AuthorId,
        kind: OperationKind,
    ) -> Self {
        Self {
            id,
            board_id,
            sequence,
            author_id,
            kind,
        }
    }
}

/// Hard bounds for attacker-controlled collaborative state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_points_per_stroke: usize,
    pub max_strokes: usize,
    pub max_history: usize,
}

impl Limits {
    pub const DEFAULT: Self = Self {
        max_points_per_stroke: 4_096,
        max_strokes: 1_024,
        max_history: 4_096,
    };

    /// Creates non-zero state limits.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidLimits`] if any limit is zero.
    pub fn new(
        max_points_per_stroke: usize,
        max_strokes: usize,
        max_history: usize,
    ) -> Result<Self, ModelError> {
        if max_points_per_stroke == 0 || max_strokes == 0 || max_history == 0 {
            return Err(ModelError::InvalidLimits);
        }
        Ok(Self {
            max_points_per_stroke,
            max_strokes,
            max_history,
        })
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    Duplicate,
    NothingToUndo,
}

#[derive(Clone, Debug)]
enum UndoChange {
    Add { stroke_id: StrokeId },
    Update { stroke_id: StrokeId, old_len: usize },
    Finalize { stroke_id: StrokeId },
    Delete { index: usize, stroke: Stroke },
    Clear { strokes: Vec<Stroke> },
}

#[derive(Clone, Debug)]
struct UndoRecord {
    author: AuthorId,
    change: UndoChange,
}

/// The authoritative state of one collaborative board.
#[derive(Clone, Debug)]
pub struct Board {
    id: BoardId,
    limits: Limits,
    last_sequence: u64,
    strokes: Vec<Stroke>,
    operations: VecDeque<Operation>,
    undo: VecDeque<UndoRecord>,
}

impl Board {
    #[must_use]
    pub fn new(id: BoardId) -> Self {
        Self::from_limits(id, Limits::DEFAULT)
    }

    /// Creates a board with custom state bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidLimits`] if any limit is zero.
    pub fn with_limits(id: BoardId, limits: Limits) -> Result<Self, ModelError> {
        Limits::new(
            limits.max_points_per_stroke,
            limits.max_strokes,
            limits.max_history,
        )?;
        Ok(Self::from_limits(id, limits))
    }

    fn from_limits(id: BoardId, limits: Limits) -> Self {
        Self {
            id,
            limits,
            last_sequence: 0,
            strokes: Vec::new(),
            operations: VecDeque::new(),
            undo: VecDeque::new(),
        }
    }

    #[must_use]
    pub const fn id(&self) -> BoardId {
        self.id
    }

    #[must_use]
    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    #[must_use]
    pub fn strokes(&self) -> &[Stroke] {
        &self.strokes
    }

    #[must_use]
    pub fn stroke(&self, id: StrokeId) -> Option<&Stroke> {
        self.strokes.iter().find(|stroke| stroke.id == id)
    }

    #[must_use]
    pub fn operation_history_len(&self) -> usize {
        self.operations.len()
    }

    #[must_use]
    pub fn undo_history_len(&self) -> usize {
        self.undo.len()
    }

    /// Applies exactly the next operation. Replaying an exactly equal retained
    /// operation is idempotent and does not mutate state.
    ///
    /// # Errors
    ///
    /// Returns an error when ordering, identity, ownership, geometry, or a
    /// configured state bound is violated. Failed operations do not consume a
    /// sequence number.
    pub fn apply(&mut self, operation: Operation) -> Result<ApplyOutcome, ModelError> {
        if operation.board_id != self.id {
            return Err(ModelError::WrongBoard {
                expected: self.id,
                actual: operation.board_id,
            });
        }

        if let Some(previous) = self
            .operations
            .iter()
            .find(|previous| previous.id == operation.id || previous.sequence == operation.sequence)
        {
            return if previous == &operation {
                Ok(ApplyOutcome::Duplicate)
            } else if previous.id == operation.id {
                Err(ModelError::OperationIdConflict(operation.id))
            } else {
                Err(ModelError::SequenceConflict(operation.sequence))
            };
        }

        let expected = self
            .last_sequence
            .checked_add(1)
            .ok_or(ModelError::SequenceExhausted)?;
        if operation.sequence > expected {
            return Err(ModelError::SequenceGap {
                expected,
                actual: operation.sequence,
            });
        }
        if operation.sequence < expected {
            return Err(ModelError::OperationTooOld(operation.sequence));
        }

        let outcome = self.apply_kind(&operation)?;
        self.last_sequence = operation.sequence;
        self.operations.push_back(operation);
        if self.operations.len() > self.limits.max_history {
            self.operations.pop_front();
        }
        Ok(outcome)
    }

    fn apply_kind(&mut self, operation: &Operation) -> Result<ApplyOutcome, ModelError> {
        match &operation.kind {
            OperationKind::Add(stroke) => {
                stroke.validate(self.limits.max_points_per_stroke)?;
                if stroke.author != operation.author_id {
                    return Err(ModelError::WrongAuthor);
                }
                if self.strokes.iter().any(|item| item.id == stroke.id) {
                    return Err(ModelError::DuplicateStroke(stroke.id));
                }
                if self.strokes.len() == self.limits.max_strokes {
                    return Err(ModelError::StrokeLimitExceeded);
                }
                self.strokes.push(stroke.clone());
                self.push_undo(UndoRecord {
                    author: operation.author_id,
                    change: UndoChange::Add {
                        stroke_id: stroke.id,
                    },
                });
            }
            OperationKind::Update { stroke_id, points } => {
                validate_points(points)?;
                let max_points = self.limits.max_points_per_stroke;
                let stroke = self.stroke_mut_owned(*stroke_id, operation.author_id)?;
                if stroke.finalized {
                    return Err(ModelError::StrokeFinalized(*stroke_id));
                }
                let new_len = stroke
                    .points
                    .len()
                    .checked_add(points.len())
                    .ok_or(ModelError::PointLimitExceeded)?;
                if new_len > max_points {
                    return Err(ModelError::PointLimitExceeded);
                }
                for point in points {
                    if stroke.points.iter().any(|old| old.id == point.id) {
                        return Err(ModelError::DuplicatePoint(point.id));
                    }
                }
                let old_len = stroke.points.len();
                stroke.points.extend_from_slice(points);
                self.push_undo(UndoRecord {
                    author: operation.author_id,
                    change: UndoChange::Update {
                        stroke_id: *stroke_id,
                        old_len,
                    },
                });
            }
            OperationKind::Finalize { stroke_id } => {
                let stroke = self.stroke_mut_owned(*stroke_id, operation.author_id)?;
                if stroke.finalized {
                    return Err(ModelError::StrokeFinalized(*stroke_id));
                }
                if stroke.kind != StrokeKind::Freehand && stroke.points.len() < 2 {
                    return Err(ModelError::NotEnoughPoints);
                }
                stroke.finalized = true;
                self.push_undo(UndoRecord {
                    author: operation.author_id,
                    change: UndoChange::Finalize {
                        stroke_id: *stroke_id,
                    },
                });
            }
            OperationKind::Delete { stroke_id } => {
                let index = self
                    .strokes
                    .iter()
                    .position(|stroke| stroke.id == *stroke_id)
                    .ok_or(ModelError::UnknownStroke(*stroke_id))?;
                let stroke = self.strokes.remove(index);
                self.push_undo(UndoRecord {
                    author: operation.author_id,
                    change: UndoChange::Delete { index, stroke },
                });
            }
            OperationKind::Clear => {
                if !self.strokes.is_empty() {
                    let strokes = std::mem::take(&mut self.strokes);
                    self.push_undo(UndoRecord {
                        author: operation.author_id,
                        change: UndoChange::Clear { strokes },
                    });
                }
            }
            OperationKind::Undo(scope) => {
                return self.apply_undo(*scope, operation.author_id);
            }
        }
        Ok(ApplyOutcome::Applied)
    }

    fn stroke_mut_owned(
        &mut self,
        stroke_id: StrokeId,
        author: AuthorId,
    ) -> Result<&mut Stroke, ModelError> {
        let stroke = self
            .strokes
            .iter_mut()
            .find(|stroke| stroke.id == stroke_id)
            .ok_or(ModelError::UnknownStroke(stroke_id))?;
        if stroke.author != author {
            return Err(ModelError::WrongAuthor);
        }
        Ok(stroke)
    }

    fn push_undo(&mut self, record: UndoRecord) {
        self.undo.push_back(record);
        if self.undo.len() > self.limits.max_history {
            self.undo.pop_front();
        }
    }

    fn apply_undo(
        &mut self,
        scope: UndoScope,
        author: AuthorId,
    ) -> Result<ApplyOutcome, ModelError> {
        let position = self
            .undo
            .iter()
            .rposition(|record| scope == UndoScope::Global || record.author == author);
        let Some(position) = position else {
            return Ok(ApplyOutcome::NothingToUndo);
        };

        // Validate restoration before removing the undo record.
        match &self.undo[position].change {
            UndoChange::Delete { stroke, .. } => {
                if self.strokes.len() == self.limits.max_strokes {
                    return Err(ModelError::StrokeLimitExceeded);
                }
                if self.strokes.iter().any(|item| item.id == stroke.id) {
                    return Err(ModelError::UndoConflict(stroke.id));
                }
            }
            UndoChange::Clear { strokes } => {
                if self.strokes.len().saturating_add(strokes.len()) > self.limits.max_strokes {
                    return Err(ModelError::StrokeLimitExceeded);
                }
                if strokes
                    .iter()
                    .any(|old| self.strokes.iter().any(|current| current.id == old.id))
                {
                    return Err(ModelError::UndoConflict(
                        strokes
                            .iter()
                            .find(|old| self.strokes.iter().any(|current| current.id == old.id))
                            .expect("a conflict was found")
                            .id,
                    ));
                }
            }
            UndoChange::Add { stroke_id }
            | UndoChange::Update { stroke_id, .. }
            | UndoChange::Finalize { stroke_id } => {
                if self.stroke(*stroke_id).is_none() {
                    return Err(ModelError::UndoConflict(*stroke_id));
                }
            }
        }

        let record = self.undo.remove(position).expect("position is in range");
        match record.change {
            UndoChange::Add { stroke_id } => {
                self.strokes.retain(|stroke| stroke.id != stroke_id);
            }
            UndoChange::Update { stroke_id, old_len } => {
                self.strokes
                    .iter_mut()
                    .find(|stroke| stroke.id == stroke_id)
                    .expect("undo target was validated")
                    .points
                    .truncate(old_len);
            }
            UndoChange::Finalize { stroke_id } => {
                self.strokes
                    .iter_mut()
                    .find(|stroke| stroke.id == stroke_id)
                    .expect("undo target was validated")
                    .finalized = false;
            }
            UndoChange::Delete { index, stroke } => {
                self.strokes.insert(index.min(self.strokes.len()), stroke);
            }
            UndoChange::Clear { mut strokes } => {
                strokes.append(&mut self.strokes);
                self.strokes = strokes;
            }
        }
        Ok(ApplyOutcome::Applied)
    }

    /// Renders all strokes in insertion order into a transparent RGBA layer.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::EmptyDimensions`] for a zero dimension or
    /// [`RenderError::DimensionsTooLarge`] if the RGBA allocation size
    /// overflows `usize`.
    pub fn render(&self, width: u32, height: u32) -> Result<RenderLayer, RenderError> {
        let mut layer = RenderLayer::new(width, height)?;
        for stroke in &self.strokes {
            render_stroke(&mut layer, stroke);
        }
        Ok(layer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    InvalidPoint,
    InvalidWidth,
    InvalidOpacity,
    InvalidLimits,
    NotEnoughPoints,
    DuplicatePoint(PointId),
    WrongBoard { expected: BoardId, actual: BoardId },
    WrongAuthor,
    SequenceGap { expected: u64, actual: u64 },
    SequenceConflict(u64),
    SequenceExhausted,
    OperationTooOld(u64),
    OperationIdConflict(OperationId),
    DuplicateStroke(StrokeId),
    UnknownStroke(StrokeId),
    StrokeFinalized(StrokeId),
    PointLimitExceeded,
    StrokeLimitExceeded,
    UndoConflict(StrokeId),
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ModelError {}

/// A tightly packed, non-premultiplied RGBA8 image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderLayer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RenderLayer {
    fn new(width: u32, height: u32) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::EmptyDimensions);
        }
        let len = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(RenderError::DimensionsTooLarge)?;
        Ok(Self {
            width,
            height,
            pixels: vec![0; len],
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (usize::try_from(y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(x).ok()?)
            * 4;
        self.pixels[offset..offset + 4].try_into().ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    EmptyDimensions,
    DimensionsTooLarge,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for RenderError {}

#[derive(Clone, Copy)]
struct ScreenPoint {
    x: f64,
    y: f64,
}

fn render_stroke(layer: &mut RenderLayer, stroke: &Stroke) {
    let points: Vec<_> = stroke
        .points
        .iter()
        .map(|point| ScreenPoint {
            x: f64::from(point.x) * f64::from(layer.width.saturating_sub(1)),
            y: f64::from(point.y) * f64::from(layer.height.saturating_sub(1)),
        })
        .collect();
    let radius = f64::from(stroke.style.width) * f64::from(layer.width.min(layer.height)) / 2.0;
    let paint = |layer: &mut RenderLayer, from, to| {
        paint_segment(layer, from, to, radius, stroke.style);
    };

    match stroke.kind {
        StrokeKind::Freehand => {
            if points.len() == 1 {
                paint(layer, points[0], points[0]);
            } else {
                for pair in points.windows(2) {
                    paint(layer, pair[0], pair[1]);
                }
            }
        }
        StrokeKind::Line => paint(layer, points[0], *points.last().expect("stroke has points")),
        StrokeKind::Arrow => {
            let from = points[0];
            let to = *points.last().expect("stroke has points");
            paint(layer, from, to);
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let length = dx.hypot(dy);
            if length > f64::EPSILON {
                let head = (length * 0.25).min((radius * 8.0).max(3.0));
                let ux = dx / length;
                let uy = dy / length;
                let base = ScreenPoint {
                    x: to.x - ux * head,
                    y: to.y - uy * head,
                };
                let wing = head * 0.55;
                paint(
                    layer,
                    to,
                    ScreenPoint {
                        x: base.x - uy * wing,
                        y: base.y + ux * wing,
                    },
                );
                paint(
                    layer,
                    to,
                    ScreenPoint {
                        x: base.x + uy * wing,
                        y: base.y - ux * wing,
                    },
                );
            }
        }
        StrokeKind::Rectangle => {
            let first = points[0];
            let last = *points.last().expect("stroke has points");
            let top_right = ScreenPoint {
                x: last.x,
                y: first.y,
            };
            let bottom_left = ScreenPoint {
                x: first.x,
                y: last.y,
            };
            paint(layer, first, top_right);
            paint(layer, top_right, last);
            paint(layer, last, bottom_left);
            paint(layer, bottom_left, first);
        }
        StrokeKind::Ellipse => {
            let first = points[0];
            let last = *points.last().expect("stroke has points");
            paint_ellipse(layer, first, last, radius, stroke.style);
        }
    }
}

fn paint_segment(
    layer: &mut RenderLayer,
    from: ScreenPoint,
    to: ScreenPoint,
    radius: f64,
    style: StrokeStyle,
) {
    let min_x = clipped_floor(from.x.min(to.x) - radius - 0.5, layer.width - 1);
    let max_x = clipped_ceil(from.x.max(to.x) + radius + 0.5, layer.width - 1);
    let min_y = clipped_floor(from.y.min(to.y) - radius - 0.5, layer.height - 1);
    let max_y = clipped_ceil(from.y.max(to.y) + radius + 0.5, layer.height - 1);
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let length_squared = dx.mul_add(dx, dy * dy);

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let pixel_x = f64::from(x);
            let pixel_y = f64::from(y);
            let projection = if length_squared <= f64::EPSILON {
                0.0
            } else {
                (((pixel_x - from.x) * dx + (pixel_y - from.y) * dy) / length_squared)
                    .clamp(0.0, 1.0)
            };
            let nearest_x = dx.mul_add(projection, from.x);
            let nearest_y = dy.mul_add(projection, from.y);
            let distance = (pixel_x - nearest_x).hypot(pixel_y - nearest_y);
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            blend_pixel(layer, x, y, style, coverage);
        }
    }
}

fn paint_ellipse(
    layer: &mut RenderLayer,
    first: ScreenPoint,
    last: ScreenPoint,
    radius: f64,
    style: StrokeStyle,
) {
    let center = ScreenPoint {
        x: first.x.midpoint(last.x),
        y: first.y.midpoint(last.y),
    };
    let rx = (last.x - first.x).abs() / 2.0;
    let ry = (last.y - first.y).abs() / 2.0;
    if rx <= f64::EPSILON || ry <= f64::EPSILON {
        paint_segment(layer, first, last, radius, style);
        return;
    }

    let min_x = clipped_floor(center.x - rx - radius - 0.5, layer.width - 1);
    let max_x = clipped_ceil(center.x + rx + radius + 0.5, layer.width - 1);
    let min_y = clipped_floor(center.y - ry - radius - 0.5, layer.height - 1);
    let max_y = clipped_ceil(center.y + ry + radius + 0.5, layer.height - 1);
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = f64::from(x) - center.x;
            let py = f64::from(y) - center.y;
            let normalized = ((px / rx).powi(2) + (py / ry).powi(2)).sqrt();
            let gradient = ((px / rx.powi(2)).powi(2) + (py / ry.powi(2)).powi(2)).sqrt();
            let distance = if gradient <= f64::EPSILON {
                rx.min(ry)
            } else {
                (normalized - 1.0).abs() / gradient
            };
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            blend_pixel(layer, x, y, style, coverage);
        }
    }
}

fn blend_pixel(layer: &mut RenderLayer, x: u32, y: u32, style: StrokeStyle, coverage: f64) {
    if coverage <= 0.0 {
        return;
    }
    let source_alpha =
        alpha_to_u32(f64::from(style.color.alpha) * f64::from(style.opacity) * coverage);
    if source_alpha == 0 {
        return;
    }
    let offset = (y as usize * layer.width as usize + x as usize) * 4;
    let destination_alpha = u32::from(layer.pixels[offset + 3]);
    let inverse = 255 - source_alpha;
    let output_alpha = source_alpha + divide_255(destination_alpha * inverse);

    for (channel, source) in [style.color.red, style.color.green, style.color.blue]
        .into_iter()
        .enumerate()
    {
        let premultiplied = u32::from(source) * source_alpha
            + divide_255(u32::from(layer.pixels[offset + channel]) * destination_alpha * inverse);
        layer.pixels[offset + channel] =
            ((premultiplied + output_alpha / 2) / output_alpha).min(255) as u8;
    }
    layer.pixels[offset + 3] = output_alpha.min(255) as u8;
}

const fn divide_255(value: u32) -> u32 {
    (value + 127) / 255
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clipped_floor(value: f64, upper: u32) -> u32 {
    value.floor().clamp(0.0, f64::from(upper)) as u32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clipped_ceil(value: f64, upper: u32) -> u32 {
    value.ceil().clamp(0.0, f64::from(upper)) as u32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn alpha_to_u32(value: f64) -> u32 {
    value.round().clamp(0.0, 255.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: BoardId = BoardId(1);
    const ALICE: AuthorId = AuthorId(10);
    const BOB: AuthorId = AuthorId(11);

    fn point(id: u128, x: f32, y: f32) -> Point {
        Point::new(PointId(id), x, y).unwrap()
    }

    fn style(color: Color) -> StrokeStyle {
        StrokeStyle::new(color, 0.2, 1.0).unwrap()
    }

    fn stroke(id: u128, author: AuthorId, kind: StrokeKind, points: Vec<Point>) -> Stroke {
        Stroke::new(
            StrokeId(id),
            author,
            kind,
            style(Color::rgba(255, 0, 0, 255)),
            points,
        )
        .unwrap()
    }

    fn operation(sequence: u64, author: AuthorId, kind: OperationKind) -> Operation {
        Operation::new(OperationId(sequence.into()), BOARD, sequence, author, kind)
    }

    #[test]
    fn validates_normalized_values() {
        assert_eq!(
            Point::new(PointId(1), -0.1, 0.0),
            Err(ModelError::InvalidPoint)
        );
        assert_eq!(
            Point::new(PointId(1), f32::NAN, 0.0),
            Err(ModelError::InvalidPoint)
        );
        assert_eq!(
            StrokeStyle::new(Color::rgba(0, 0, 0, 0), 0.0, 1.0),
            Err(ModelError::InvalidWidth)
        );
    }

    #[test]
    fn add_update_finalize_delete_and_clear() {
        let mut board = Board::new(BOARD);
        board
            .apply(operation(
                1,
                ALICE,
                OperationKind::Add(stroke(
                    1,
                    ALICE,
                    StrokeKind::Freehand,
                    vec![point(1, 0.0, 0.0)],
                )),
            ))
            .unwrap();
        board
            .apply(operation(
                2,
                ALICE,
                OperationKind::Update {
                    stroke_id: StrokeId(1),
                    points: vec![point(2, 1.0, 1.0)],
                },
            ))
            .unwrap();
        board
            .apply(operation(
                3,
                ALICE,
                OperationKind::Finalize {
                    stroke_id: StrokeId(1),
                },
            ))
            .unwrap();
        assert!(board.stroke(StrokeId(1)).unwrap().is_finalized());
        assert_eq!(board.stroke(StrokeId(1)).unwrap().points().len(), 2);

        board
            .apply(operation(
                4,
                BOB,
                OperationKind::Delete {
                    stroke_id: StrokeId(1),
                },
            ))
            .unwrap();
        assert!(board.strokes().is_empty());
        board
            .apply(operation(
                5,
                BOB,
                OperationKind::Add(stroke(
                    2,
                    BOB,
                    StrokeKind::Line,
                    vec![point(3, 0.0, 0.0), point(4, 1.0, 1.0)],
                )),
            ))
            .unwrap();
        board
            .apply(operation(6, ALICE, OperationKind::Clear))
            .unwrap();
        assert!(board.strokes().is_empty());
    }

    #[test]
    fn rejects_gaps_and_conflicts_but_accepts_exact_duplicates() {
        let mut board = Board::new(BOARD);
        let first = operation(1, ALICE, OperationKind::Clear);
        assert_eq!(board.apply(first.clone()), Ok(ApplyOutcome::Applied));
        assert_eq!(board.apply(first), Ok(ApplyOutcome::Duplicate));
        assert_eq!(
            board.apply(Operation::new(
                OperationId(1),
                BOARD,
                2,
                ALICE,
                OperationKind::Clear,
            )),
            Err(ModelError::OperationIdConflict(OperationId(1)))
        );
        assert_eq!(
            board.apply(operation(3, ALICE, OperationKind::Clear)),
            Err(ModelError::SequenceGap {
                expected: 2,
                actual: 3,
            })
        );
        assert_eq!(board.last_sequence(), 1);
    }

    #[test]
    fn author_and_global_undo_are_independent() {
        let mut board = Board::new(BOARD);
        board
            .apply(operation(
                1,
                ALICE,
                OperationKind::Add(stroke(
                    1,
                    ALICE,
                    StrokeKind::Line,
                    vec![point(1, 0.0, 0.0), point(2, 1.0, 1.0)],
                )),
            ))
            .unwrap();
        board
            .apply(operation(
                2,
                BOB,
                OperationKind::Add(stroke(
                    2,
                    BOB,
                    StrokeKind::Line,
                    vec![point(3, 1.0, 0.0), point(4, 0.0, 1.0)],
                )),
            ))
            .unwrap();
        board
            .apply(operation(3, ALICE, OperationKind::Undo(UndoScope::Author)))
            .unwrap();
        assert_eq!(board.strokes()[0].id(), StrokeId(2));
        board
            .apply(operation(4, ALICE, OperationKind::Undo(UndoScope::Global)))
            .unwrap();
        assert!(board.strokes().is_empty());
    }

    #[test]
    fn enforces_state_and_history_bounds() {
        let limits = Limits::new(2, 1, 2).unwrap();
        let mut board = Board::with_limits(BOARD, limits).unwrap();
        board
            .apply(operation(
                1,
                ALICE,
                OperationKind::Add(stroke(
                    1,
                    ALICE,
                    StrokeKind::Freehand,
                    vec![point(1, 0.0, 0.0)],
                )),
            ))
            .unwrap();
        assert_eq!(
            board.apply(operation(
                2,
                BOB,
                OperationKind::Add(stroke(
                    2,
                    BOB,
                    StrokeKind::Freehand,
                    vec![point(2, 0.0, 0.0)],
                )),
            )),
            Err(ModelError::StrokeLimitExceeded)
        );
        assert_eq!(
            board.apply(operation(
                2,
                ALICE,
                OperationKind::Update {
                    stroke_id: StrokeId(1),
                    points: vec![point(2, 0.5, 0.5), point(3, 1.0, 1.0)],
                },
            )),
            Err(ModelError::PointLimitExceeded)
        );
        board
            .apply(operation(2, BOB, OperationKind::Clear))
            .unwrap();
        board
            .apply(operation(3, BOB, OperationKind::Clear))
            .unwrap();
        assert_eq!(board.operation_history_len(), 2);
        assert!(board.undo_history_len() <= 2);
        assert_eq!(
            board.apply(operation(1, ALICE, OperationKind::Clear)),
            Err(ModelError::OperationTooOld(1))
        );
    }

    #[test]
    fn clear_can_be_undone() {
        let mut board = Board::new(BOARD);
        board
            .apply(operation(
                1,
                ALICE,
                OperationKind::Add(stroke(
                    1,
                    ALICE,
                    StrokeKind::Freehand,
                    vec![point(1, 0.5, 0.5)],
                )),
            ))
            .unwrap();
        board
            .apply(operation(2, BOB, OperationKind::Clear))
            .unwrap();
        board
            .apply(operation(3, BOB, OperationKind::Undo(UndoScope::Author)))
            .unwrap();
        assert_eq!(board.strokes().len(), 1);
    }

    #[test]
    fn renders_shape_pixels_and_clips_edges() {
        let kinds = [
            StrokeKind::Freehand,
            StrokeKind::Line,
            StrokeKind::Arrow,
            StrokeKind::Rectangle,
            StrokeKind::Ellipse,
        ];
        for kind in kinds {
            let mut board = Board::new(BOARD);
            board
                .apply(operation(
                    1,
                    ALICE,
                    OperationKind::Add(stroke(
                        1,
                        ALICE,
                        kind,
                        vec![point(1, 0.0, 0.0), point(2, 1.0, 1.0)],
                    )),
                ))
                .unwrap();
            let layer = board.render(9, 9).unwrap();
            assert!(layer.pixels().chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert_eq!(layer.pixels().len(), 9 * 9 * 4);
            assert!(layer.pixel(0, 0).unwrap()[3] > 0);
        }
    }

    #[test]
    fn rendering_is_deterministic_and_uses_source_over_rgba() {
        let mut board = Board::new(BOARD);
        let mut red = stroke(
            1,
            ALICE,
            StrokeKind::Line,
            vec![point(1, 0.0, 0.5), point(2, 1.0, 0.5)],
        );
        red.style = StrokeStyle::new(Color::rgba(255, 0, 0, 255), 0.2, 0.5).unwrap();
        let mut blue = stroke(
            2,
            BOB,
            StrokeKind::Line,
            vec![point(3, 0.5, 0.0), point(4, 0.5, 1.0)],
        );
        blue.style = StrokeStyle::new(Color::rgba(0, 0, 255, 255), 0.2, 0.5).unwrap();
        board
            .apply(operation(1, ALICE, OperationKind::Add(red)))
            .unwrap();
        board
            .apply(operation(2, BOB, OperationKind::Add(blue)))
            .unwrap();
        let first = board.render(5, 5).unwrap();
        let second = board.render(5, 5).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.pixel(2, 2), Some([85, 0, 170, 192]));
        assert_eq!(first.pixel(0, 4), Some([0, 0, 0, 0]));
    }

    #[test]
    fn insertion_order_changes_composited_pixels() {
        let make_board = |reverse: bool| {
            let mut board = Board::new(BOARD);
            let colors = if reverse {
                [Color::rgba(0, 0, 255, 255), Color::rgba(255, 0, 0, 255)]
            } else {
                [Color::rgba(255, 0, 0, 255), Color::rgba(0, 0, 255, 255)]
            };
            for (index, color) in colors.into_iter().enumerate() {
                let mut item = stroke(
                    index as u128 + 1,
                    ALICE,
                    StrokeKind::Line,
                    vec![
                        point(index as u128 * 2 + 1, 0.0, 0.5),
                        point(index as u128 * 2 + 2, 1.0, 0.5),
                    ],
                );
                item.style = StrokeStyle::new(color, 0.2, 0.5).unwrap();
                board
                    .apply(operation(index as u64 + 1, ALICE, OperationKind::Add(item)))
                    .unwrap();
            }
            board
        };
        assert_ne!(
            make_board(false).render(5, 5).unwrap().pixel(2, 2),
            make_board(true).render(5, 5).unwrap().pixel(2, 2)
        );
    }
}
