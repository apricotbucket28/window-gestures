#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Point {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: Span,
    pub(crate) y: Span,
}

#[derive(Clone, Copy)]
pub(crate) struct WindowFrame {
    pub(crate) outer: Rect,
    pub(crate) visible: Rect,
}

impl WindowFrame {
    pub(crate) fn centered_in(self, area: Rect) -> Rect {
        let shift = |area: Span, visible: Span| {
            i64::from(area.start) + (i64::from(area.length()) - i64::from(visible.length())) / 2
                - i64::from(visible.start)
        };
        Rect {
            x: self.outer.x.shifted(shift(area.x, self.visible.x)),
            y: self.outer.y.shifted(shift(area.y, self.visible.y)),
        }
    }

    pub(crate) fn outer_for_visible(self, desired: Rect) -> Rect {
        let offset = |desired: i32, outer: i32, visible: i32| {
            desired.saturating_add(outer.saturating_sub(visible))
        };
        Rect::from_edges(
            offset(desired.x.start, self.outer.x.start, self.visible.x.start),
            offset(desired.y.start, self.outer.y.start, self.visible.y.start),
            offset(desired.x.end, self.outer.x.end, self.visible.x.end),
            offset(desired.y.end, self.outer.y.end, self.visible.y.end),
        )
    }

    pub(crate) fn with_visible(self, visible: Rect) -> Self {
        Self {
            outer: self.outer_for_visible(visible),
            visible,
        }
    }

    pub(crate) fn restore_under_pointer(self, point: Point, outer_size: (i32, i32)) -> Self {
        let (width, height) = self.visible_size_for_outer(outer_size);
        let resized = Rect::from_edges(0, 0, width, height);
        self.with_visible(self.visible.anchor_frame(resized, point))
    }

    pub(crate) fn fit_tile(self, tile: Rect, area: Rect, limits: (SizeLimits, SizeLimits)) -> Self {
        let proposed = self.with_visible(tile);
        let size = proposed.visible_size_for_outer((
            limits.0.clamp(proposed.outer.x.length()),
            limits.1.clamp(proposed.outer.y.length()),
        ));
        self.with_visible(area.fit_tile(tile, size))
    }

    fn visible_size_for_outer(self, (width, height): (i32, i32)) -> (i32, i32) {
        let adjust = |size: i32, outer: Span, visible: Span| {
            size.saturating_add(visible.length().saturating_sub(outer.length()))
                .max(1)
        };
        (
            adjust(width, self.outer.x, self.visible.x),
            adjust(height, self.outer.y, self.visible.y),
        )
    }
}

impl Rect {
    pub(crate) fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            x: Span {
                start: left,
                end: right,
            },
            y: Span {
                start: top,
                end: bottom,
            },
        }
    }

    pub(crate) fn has_area(self) -> bool {
        self.x.start < self.x.end && self.y.start < self.y.end
    }

    pub(crate) fn fit_tile(self, tile: Self, size: (i32, i32)) -> Self {
        let fit = |area: Span, tile: Span, length: i32| {
            let start = if tile == area {
                tile.start.saturating_add((tile.length() - length) / 2)
            } else if tile.end == area.end {
                tile.end.saturating_sub(length)
            } else {
                tile.start
            };
            Span {
                start,
                end: start.saturating_add(length),
            }
        };
        Self {
            x: fit(self.x, tile.x, size.0),
            y: fit(self.y, tile.y, size.1),
        }
    }

    pub(crate) fn clamp_point(self, point: Point) -> Point {
        if !self.has_area() {
            return point;
        }
        Point {
            x: point.x.clamp(self.x.start, self.x.end - 1),
            y: point.y.clamp(self.y.start, self.y.end - 1),
        }
    }

    pub(crate) fn anchor_frame(self, frame: Self, point: Point) -> Self {
        let anchor = |before: Span, after: Span, pointer: i32| {
            let offset = pointer
                .saturating_sub(before.start)
                .clamp(0, before.length());
            let offset =
                i64::from(offset) * i64::from(after.length()) / i64::from(before.length().max(1));
            after.shifted(i64::from(pointer) - offset - i64::from(after.start))
        };
        Self {
            x: anchor(self.x, frame.x, point.x),
            y: anchor(self.y, frame.y, point.y),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) start: i32,
    pub(crate) end: i32,
}

impl Span {
    pub(crate) fn length(self) -> i32 {
        self.end.saturating_sub(self.start)
    }

    pub(crate) fn shifted(self, delta: i64) -> Self {
        let delta = delta.clamp(
            i64::from(i32::MIN) - i64::from(self.start),
            i64::from(i32::MAX) - i64::from(self.end),
        );
        Self {
            start: (i64::from(self.start) + delta) as i32,
            end: (i64::from(self.end) + delta) as i32,
        }
    }

    fn at(self, edge: Edge) -> i32 {
        match edge {
            Edge::Start => self.start,
            Edge::End => self.end,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragMode {
    Move,
    Resize,
}

#[derive(Clone, Copy)]
pub(crate) struct SizeLimits {
    min: i32,
    max: i32,
}

impl SizeLimits {
    pub(crate) fn new(min: i32, max: i32) -> Self {
        let min = min.max(1);
        let max = if max < min { i32::MAX } else { max };
        Self { min, max }
    }

    pub(crate) fn clamp(self, size: i32) -> i32 {
        size.clamp(self.min, self.max)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeDrop {
    Tile(Rect),
    Maximize,
}

pub(crate) fn edge_drop(point: Point, screen: Rect, work: Rect, distance: u32) -> Option<EdgeDrop> {
    if !(screen.x.start..screen.x.end).contains(&point.x)
        || !(screen.y.start..screen.y.end).contains(&point.y)
    {
        return None;
    }
    let near = |coordinate: i32, span: Span| {
        let first = coordinate.saturating_sub(span.start).max(0) as u32;
        let last = span.end.saturating_sub(1).saturating_sub(coordinate).max(0) as u32;
        (first.min(last) <= distance).then_some(if first <= last {
            Edge::Start
        } else {
            Edge::End
        })
    };
    let half = |span: Span, edge| {
        let middle = span.start + span.length() / 2;
        match edge {
            Edge::Start => Span {
                end: middle,
                ..span
            },
            Edge::End => Span {
                start: middle,
                ..span
            },
        }
    };
    let vertical = near(point.y, work.y);
    let tile = match (near(point.x, work.x), vertical) {
        (Some(side), vertical) => Rect {
            x: half(work.x, side),
            y: vertical.map_or(work.y, |edge| half(work.y, edge)),
        },
        (None, Some(Edge::Start)) => return Some(EdgeDrop::Maximize),
        (None, Some(Edge::End)) => Rect {
            y: half(work.y, Edge::End),
            ..work
        },
        (None, None) => return None,
    };
    tile.has_area().then_some(EdgeDrop::Tile(tile))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Start,
    End,
}

impl Edge {
    fn opposite(self) -> Self {
        match self {
            Self::Start => Self::End,
            Self::End => Self::Start,
        }
    }
}

#[derive(Clone, Copy)]
enum AxisAction {
    Translate,
    Resize(Edge),
    Fixed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResizeDirection {
    Horizontal,
    Vertical,
    NorthwestSoutheast,
    NortheastSouthwest,
}

#[derive(Clone, Copy)]
pub(crate) struct ResizeHandle(AxisAction, AxisAction);

impl ResizeHandle {
    pub(crate) fn at(frame: Rect, point: Point) -> Self {
        let (x, y) = resize_actions(frame, point);
        Self(x, y)
    }

    pub(crate) fn direction(self) -> ResizeDirection {
        use AxisAction::Resize;
        match (self.0, self.1) {
            (Resize(x), Resize(y)) if x == y => ResizeDirection::NorthwestSoutheast,
            (Resize(_), Resize(_)) => ResizeDirection::NortheastSouthwest,
            (Resize(_), _) => ResizeDirection::Horizontal,
            _ => ResizeDirection::Vertical,
        }
    }
}

struct DragAxis {
    outer: Span,
    frame_offset: Span,
    action: AxisAction,
    limits: SizeLimits,
    snapped_edge: Option<SnapTarget>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SnapTarget {
    edge: Edge,
    position: i32,
    is_peer: bool,
}

impl DragAxis {
    fn new(outer: Span, visible: Span, action: AxisAction, limits: SizeLimits) -> Self {
        Self {
            outer,
            frame_offset: Span {
                start: visible.start.saturating_sub(outer.start),
                end: visible.end.saturating_sub(outer.end),
            },
            action,
            limits,
            snapped_edge: None,
        }
    }

    fn apply(&self, span: Span, delta: i32) -> Span {
        match self.action {
            AxisAction::Translate => span.shifted(i64::from(delta)),
            AxisAction::Resize(Edge::Start) => {
                let width = self.limits.clamp(span.length().saturating_sub(delta));
                Span {
                    start: span.end.saturating_sub(width),
                    end: span.end,
                }
            }
            AxisAction::Resize(Edge::End) => {
                let width = self.limits.clamp(span.length().saturating_add(delta));
                Span {
                    start: span.start,
                    end: span.start.saturating_add(width),
                }
            }
            AxisAction::Fixed => span,
        }
    }

    fn visible(&self, outer: Span) -> Span {
        Span {
            start: outer.start.saturating_add(self.frame_offset.start),
            end: outer.end.saturating_add(self.frame_offset.end),
        }
    }

    fn snap(
        &mut self,
        proposed: Span,
        work: Option<Span>,
        peers: impl Iterator<Item = Span>,
        distance: u32,
    ) -> Span {
        let edges: &[_] = match self.action {
            AxisAction::Translate => &[Edge::Start, Edge::End],
            AxisAction::Resize(Edge::Start) => &[Edge::Start],
            AxisAction::Resize(Edge::End) => &[Edge::End],
            AxisAction::Fixed => return proposed,
        };

        let mut best = None;
        let mut target = None;
        let visible = self.visible(proposed);
        let mut consider = |edge: Edge, wanted: i32, is_peer: bool| {
            let candidate = SnapTarget {
                edge,
                position: wanted,
                is_peer,
            };
            let delta = wanted.saturating_sub(visible.at(edge));
            // Keep an acquired edge through small overshoots. Other candidates
            // still compete by distance once the held edge is out of range.
            let held = self.snapped_edge == Some(candidate);
            let radius = if held {
                distance.saturating_mul(2)
            } else {
                distance
            };
            let score = (!held, delta.unsigned_abs(), is_peer, delta);
            if score.1 <= radius && best.is_none_or(|current| score < current) {
                let fitted = self.apply(proposed, delta);
                if self.visible(fitted).at(edge) == wanted {
                    best = Some(score);
                    target = Some(candidate);
                }
            }
        };
        if let Some(work) = work {
            for &edge in edges {
                consider(edge, work.at(edge), false);
            }
        }
        for peer in peers {
            for &edge in edges {
                consider(edge, peer.at(edge.opposite()), true);
            }
        }
        self.snapped_edge = target;
        best.map_or(proposed, |(_, _, _, delta)| self.apply(proposed, delta))
    }
}

pub(crate) struct DragGeometry {
    origin: Point,
    x: DragAxis,
    y: DragAxis,
}

impl DragGeometry {
    pub(crate) fn new(
        resize: Option<ResizeHandle>,
        origin: Point,
        WindowFrame { outer, visible }: WindowFrame,
        limits: (SizeLimits, SizeLimits),
    ) -> Self {
        let ResizeHandle(x, y) =
            resize.unwrap_or(ResizeHandle(AxisAction::Translate, AxisAction::Translate));
        Self {
            origin,
            x: DragAxis::new(outer.x, visible.x, x, limits.0),
            y: DragAxis::new(outer.y, visible.y, y, limits.1),
        }
    }

    pub(crate) fn at(
        &mut self,
        point: Point,
        work: Option<Rect>,
        peers: &[Rect],
        distance: u32,
    ) -> Rect {
        let x = self
            .x
            .apply(self.x.outer, point.x.saturating_sub(self.origin.x));
        let y = self
            .y
            .apply(self.y.outer, point.y.saturating_sub(self.origin.y));
        let (visible_x, visible_y) = (self.x.visible(x), self.y.visible(y));
        Rect {
            x: self.x.snap(
                x,
                work.map(|r| r.x),
                peers
                    .iter()
                    .filter(|r| r.y.overlaps(visible_y))
                    .map(|r| r.x),
                distance,
            ),
            y: self.y.snap(
                y,
                work.map(|r| r.y),
                peers
                    .iter()
                    .filter(|r| r.x.overlaps(visible_x))
                    .map(|r| r.y),
                distance,
            ),
        }
    }
}

fn resize_actions(frame: Rect, point: Point) -> (AxisAction, AxisAction) {
    let region = |span: Span, at: i32| {
        let third = span.length() / 3;
        if at <= span.start.saturating_add(third) {
            Some(Edge::Start)
        } else if at >= span.end.saturating_sub(third) {
            Some(Edge::End)
        } else {
            None
        }
    };
    let (x, y) = (region(frame.x, point.x), region(frame.y, point.y));
    if x.is_some() || y.is_some() {
        return (
            x.map_or(AxisAction::Fixed, AxisAction::Resize),
            y.map_or(AxisAction::Fixed, AxisAction::Resize),
        );
    }

    [
        (
            point.x.abs_diff(frame.x.start),
            (AxisAction::Resize(Edge::Start), AxisAction::Fixed),
        ),
        (
            point.x.abs_diff(frame.x.end),
            (AxisAction::Resize(Edge::End), AxisAction::Fixed),
        ),
        (
            point.y.abs_diff(frame.y.start),
            (AxisAction::Fixed, AxisAction::Resize(Edge::Start)),
        ),
        (
            point.y.abs_diff(frame.y.end),
            (AxisAction::Fixed, AxisAction::Resize(Edge::End)),
        ),
    ]
    .into_iter()
    .min_by_key(|&(distance, _)| distance)
    .expect("a window has four edges")
    .1
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: (SizeLimits, SizeLimits) = (
        SizeLimits { min: 60, max: 180 },
        SizeLimits { min: 40, max: 160 },
    );

    #[test]
    fn restoring_keeps_the_relative_grab_point_and_invisible_borders() {
        let outer = Rect::from_edges(-805, 0, 5, 805);
        let visible = Rect::from_edges(-800, 0, 0, 800);
        let point = Point { x: -600, y: 200 };
        let restored = WindowFrame { outer, visible }.restore_under_pointer(point, (410, 405));
        assert_eq!(restored.visible, Rect::from_edges(-700, 100, -300, 500));
        assert_eq!(restored.outer, Rect::from_edges(-705, 100, -295, 505));
    }

    #[test]
    fn edge_drop_splits_the_work_area_on_a_negative_origin_monitor() {
        let screen = Rect::from_edges(-1601, 40, 0, 1041);
        let work = Rect::from_edges(-1601, 40, 0, 1001);
        let drop = |x, y| edge_drop(Point { x, y }, screen, work, 10);

        assert_eq!(
            drop(-1595, 500),
            Some(EdgeDrop::Tile(Rect::from_edges(-1601, 40, -801, 1001)))
        );
        assert_eq!(
            drop(-4, 500),
            Some(EdgeDrop::Tile(Rect::from_edges(-801, 40, 0, 1001)))
        );
        assert_eq!(
            drop(-1595, 42),
            Some(EdgeDrop::Tile(Rect::from_edges(-1601, 40, -801, 520)))
        );
        assert_eq!(
            drop(-4, 1038),
            Some(EdgeDrop::Tile(Rect::from_edges(-801, 520, 0, 1001)))
        );
        assert_eq!(drop(-810, 44), Some(EdgeDrop::Maximize));
        assert_eq!(
            drop(-810, 998),
            Some(EdgeDrop::Tile(Rect::from_edges(-1601, 520, 0, 1001)))
        );
        assert_eq!(drop(-810, 1038), drop(-810, 998));
        assert_eq!(drop(1, 700), None);
        assert_eq!(drop(-810, 700), None);
        assert_eq!(
            edge_drop(
                screen.clamp_point(Point { x: -810, y: 39 }),
                screen,
                work,
                10
            ),
            Some(EdgeDrop::Maximize),
        );
    }

    #[test]
    fn tile_bounds_account_for_invisible_frame_borders() {
        let outer = Rect::from_edges(45, 100, 405, 405);
        let visible = Rect::from_edges(50, 100, 400, 400);
        let tile = Rect::from_edges(0, 0, 960, 1040);
        assert_eq!(
            WindowFrame { outer, visible }.outer_for_visible(tile),
            Rect::from_edges(-5, 0, 965, 1045)
        );
    }

    #[test]
    fn size_limited_tiles_stay_anchored_to_the_selected_edge() {
        let work = Rect::from_edges(0, 0, 1200, 960);
        let left = Rect::from_edges(0, 0, 600, 960);
        let right = Rect::from_edges(600, 0, 1200, 960);
        let bottom = Rect::from_edges(0, 480, 1200, 960);
        assert_eq!(
            work.fit_tile(left, (700, 800)),
            Rect::from_edges(0, 80, 700, 880)
        );
        assert_eq!(
            work.fit_tile(right, (700, 800)),
            Rect::from_edges(500, 80, 1200, 880)
        );
        assert_eq!(
            work.fit_tile(bottom, (900, 600)),
            Rect::from_edges(150, 360, 1050, 960)
        );
    }

    #[test]
    fn nearest_peer_wins_even_when_an_earlier_edge_is_on_the_other_side() {
        let outer = Rect::from_edges(100, 100, 200, 180);
        let frame = WindowFrame {
            outer,
            visible: outer,
        };
        let mut drag = DragGeometry::new(None, Point { x: 120, y: 120 }, frame, LIMITS);
        let farther = Rect::from_edges(240, 100, 300, 180);
        let closer = Rect::from_edges(280, 100, 310, 180);
        let distant_row = Rect::from_edges(262, 500, 308, 580);
        let point = Point { x: 328, y: 120 };
        assert_eq!(
            drag.at(point, None, &[farther, distant_row, closer], 12)
                .x
                .start,
            310
        );
    }

    #[test]
    fn uses_visible_edges_for_snapping_on_a_negative_origin_monitor() {
        let outer = Rect::from_edges(-1460, 100, -1250, 300);
        let visible = Rect::from_edges(-1450, 100, -1260, 290);
        let work = Rect::from_edges(-1440, 0, 0, 900);
        let frame = WindowFrame { outer, visible };
        let mut drag = DragGeometry::new(None, Point { x: -1400, y: 150 }, frame, LIMITS);
        let result = drag.at(Point { x: -1395, y: 150 }, Some(work), &[], 12);
        assert_eq!(result, Rect::from_edges(-1450, 100, -1240, 300));
    }

    #[test]
    fn work_area_wins_ties_and_resize_limits_keep_the_opposite_corner_fixed() {
        let outer = Rect::from_edges(100, 100, 200, 180);
        let origin = Point { x: 110, y: 105 };
        let handle = ResizeHandle::at(outer, origin);
        assert!(handle.direction() == ResizeDirection::NorthwestSoutheast);
        let frame = WindowFrame {
            outer,
            visible: outer,
        };
        let mut drag = DragGeometry::new(Some(handle), origin, frame, LIMITS);
        let peer = Rect::from_edges(0, 100, 45, 160);
        let work = Rect::from_edges(55, 0, 900, 900);
        let result = drag.at(Point { x: 60, y: 500 }, Some(work), &[peer], 12);
        assert_eq!(result, Rect::from_edges(55, 140, 200, 180));
        let min_width = drag.at(Point { x: 500, y: -500 }, None, &[], 12);
        assert_eq!(min_width, Rect::from_edges(140, 20, 200, 180));
    }

    #[test]
    fn work_area_snap_survives_small_overshoot_and_releases_on_a_further_pull() {
        let outer = Rect::from_edges(100, 100, 300, 260);
        let work = Rect::from_edges(0, 0, 1920, 1040);
        let origin = Point { x: 200, y: 140 };
        let frame = WindowFrame {
            outer,
            visible: outer,
        };
        let mut drag = DragGeometry::new(None, origin, frame, LIMITS);
        let mut left = |x| {
            drag.at(Point { x, y: origin.y }, Some(work), &[], 24)
                .x
                .start
        };

        assert_eq!(left(132), 32);
        assert_eq!(left(122), 0);
        assert_eq!(left(65), 0);
        assert_eq!(left(50), -50);
    }
}
