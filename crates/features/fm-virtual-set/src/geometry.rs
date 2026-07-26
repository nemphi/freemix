/// A point in normalized output coordinates, where `(0, 0)` is the top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedPoint {
    pub x: f32,
    pub y: f32,
}

impl NormalizedPoint {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub(crate) fn is_normalized(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
}

/// Four corners in clockwise winding order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedCorners {
    pub top_left: NormalizedPoint,
    pub top_right: NormalizedPoint,
    pub bottom_right: NormalizedPoint,
    pub bottom_left: NormalizedPoint,
}

impl NormalizedCorners {
    #[must_use]
    pub const fn new(
        top_left: NormalizedPoint,
        top_right: NormalizedPoint,
        bottom_right: NormalizedPoint,
        bottom_left: NormalizedPoint,
    ) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    #[must_use]
    pub const fn rectangle(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self::new(
            NormalizedPoint::new(left, top),
            NormalizedPoint::new(right, top),
            NormalizedPoint::new(right, bottom),
            NormalizedPoint::new(left, bottom),
        )
    }

    pub(crate) const fn points(self) -> [NormalizedPoint; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }
}

/// A quadrilateral onto which a source is mapped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedPlane {
    pub corners: NormalizedCorners,
}

impl NormalizedPlane {
    #[must_use]
    pub const fn new(corners: NormalizedCorners) -> Self {
        Self { corners }
    }

    #[must_use]
    pub const fn rectangle(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self::new(NormalizedCorners::rectangle(left, top, right, bottom))
    }

    pub(crate) fn is_normalized(self) -> bool {
        self.corners
            .points()
            .into_iter()
            .all(NormalizedPoint::is_normalized)
    }

    pub(crate) fn is_degenerate(self) -> bool {
        const MIN_AREA: f32 = 1.0e-6;

        let points = self.corners.points();
        let twice_area = points
            .into_iter()
            .zip(points.into_iter().cycle().skip(1))
            .take(4)
            .map(|(a, b)| a.x * b.y - b.x * a.y)
            .sum::<f32>();

        twice_area.abs() <= MIN_AREA
            || segments_intersect(points[0], points[1], points[2], points[3])
            || segments_intersect(points[1], points[2], points[3], points[0])
    }
}

fn segments_intersect(
    first_start: NormalizedPoint,
    first_end: NormalizedPoint,
    second_start: NormalizedPoint,
    second_end: NormalizedPoint,
) -> bool {
    let side = |a: NormalizedPoint, b: NormalizedPoint, point: NormalizedPoint| {
        (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x)
    };

    let first_a = side(first_start, first_end, second_start);
    let first_b = side(first_start, first_end, second_end);
    let second_a = side(second_start, second_end, first_start);
    let second_b = side(second_start, second_end, first_end);

    first_a * first_b <= 0.0 && second_a * second_b <= 0.0
}
