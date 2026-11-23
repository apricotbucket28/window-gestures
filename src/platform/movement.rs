use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClipCursor, SW_MAXIMIZE, SW_SHOWNOACTIVATE, ShowWindowAsync,
};

use super::actions::{RESTORE_ORDER_INTERVAL, WindowActions};
use super::tile_preview::TilePreview;
use super::window::{DragState, Monitor, WindowTarget, neighboring_frames, work_area};
use super::worker::{DragMotion, DragPhase, DragStart, WindowCommand, WindowRequests};
use crate::geometry::{
    DragGeometry, DragMode, EdgeDrop, Point, Rect, SizeLimits, WindowFrame, edge_drop,
};

const SNAP_DISTANCE_AT_96_DPI: u32 = 20;
const TILE_DISTANCE_AT_96_DPI: u32 = 32;
const DRAG_FRAME_INTERVAL: Duration = Duration::from_millis(8);

pub(super) fn run(requests: WindowRequests, action_failed: impl Fn(String)) -> bool {
    let motion = requests.motion();
    let mut actions = WindowActions::default();
    let mut tiles = HashMap::new();
    while motion.phase() != DragPhase::Closed {
        let timeout = actions
            .advance_restore_order()
            .then_some(RESTORE_ORDER_INTERVAL);
        let request = match requests.next(timeout) {
            Ok(Some(request)) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
        };
        match request {
            WindowCommand::Drag(start) => {
                tiles.retain(|target: &WindowTarget, _| target.still_matches());
                run_drag(start, motion, &mut tiles);
            }
            WindowCommand::Action(target, action) => {
                if let Err(error) = actions.apply(target, action) {
                    action_failed(error);
                }
                motion.finish_action();
            }
            WindowCommand::ReadMenuState(target, reply) => {
                let _ = reply.send(actions.menu_state(target));
            }
        }
    }
    actions.restore_all()
}

#[derive(Clone, Copy)]
struct WindowSize {
    width: i32,
    height: i32,
    dpi: u32,
}

impl WindowSize {
    fn at_dpi(self, dpi: u32) -> (i32, i32) {
        let scale = |size| {
            let scaled =
                (i64::from(size) * i64::from(dpi) + i64::from(self.dpi / 2)) / i64::from(self.dpi);
            scaled.clamp(1, i64::from(i32::MAX)) as i32
        };
        (scale(self.width), scale(self.height))
    }
}

struct TiledWindow {
    normal_size: WindowSize,
    bounds: Rect,
}

struct DragSession {
    normal_size: WindowSize,
    placement: Option<DragPlacement>,
    neighbors: Vec<Rect>,
    snap_distance: u32,
}

struct DragPlacement {
    bounds: Rect,
    geometry: DragGeometry,
    next_mode: DragMode,
}

impl DragSession {
    fn start(
        start: &DragStart,
        mode: DragMode,
        motion: &DragMotion,
        tiles: &mut HashMap<WindowTarget, TiledWindow>,
    ) -> Option<Self> {
        let target = start.target;
        let frames = target
            .drag_state(mode, start.permissions)
            .and_then(|window| match window {
                DragState::Normal => Some((target.frame()?, None)),
                DragState::Maximized => {
                    let maximized = target.frame()?.visible;
                    let restored = restore_maximized_drag(target, motion, start.origin)?;
                    Some((restored, Some(maximized)))
                }
            });

        if tiles.get(&target).is_some_and(|tile| {
            frames.map(|(frame, _)| frame.outer) != Some(tile.bounds) || !target.is_sizable()
        }) {
            tiles.remove(&target);
        }
        let (frame, maximized) = frames?;
        let previous_size = tiles.get(&target).map(|tile| tile.normal_size);
        let dpi = target.dpi();
        let normal_size = previous_size.unwrap_or(WindowSize {
            width: frame.outer.x.length(),
            height: frame.outer.y.length(),
            dpi,
        });
        let pulling_away = mode == DragMode::Move && previous_size.is_some();
        let initial = if let Some(maximized) = maximized {
            let visible = match mode {
                DragMode::Move => maximized.anchor_frame(frame.visible, start.origin),
                DragMode::Resize => maximized,
            };
            frame.with_visible(visible)
        } else if pulling_away {
            let (width, height) = normal_size.at_dpi(dpi);
            let (width_limit, height_limit) = target.tracking_limits(DragMode::Resize);
            frame.restore_under_pointer(
                start.origin,
                (width_limit.clamp(width), height_limit.clamp(height)),
            )
        } else {
            frame
        };
        Some(Self {
            normal_size,
            placement: Some(DragPlacement {
                bounds: frame.outer,
                geometry: DragGeometry::new(
                    start.resize,
                    start.origin,
                    initial,
                    target.tracking_limits(mode),
                ),
                next_mode: if pulling_away { DragMode::Resize } else { mode },
            }),
            neighbors: if start.snapping {
                neighboring_frames(&target)
            } else {
                Vec::new()
            },
            snap_distance: SNAP_DISTANCE_AT_96_DPI.saturating_mul(dpi) / 96,
        })
    }

    fn place(
        &mut self,
        start: &DragStart,
        mode: DragMode,
        point: Point,
        tiles: &mut HashMap<WindowTarget, TiledWindow>,
    ) {
        let Some(placement) = self.placement.as_mut() else {
            return;
        };
        let area = start.snapping.then(|| work_area(point)).flatten();
        let destination = placement
            .geometry
            .at(point, area, &self.neighbors, self.snap_distance);
        if destination == placement.bounds {
            return;
        }
        if !start.target.place(destination, placement.next_mode) {
            self.placement = None;
            return;
        }
        if placement.next_mode == DragMode::Resize {
            tiles.remove(&start.target);
        }
        placement.bounds = destination;
        placement.next_mode = mode;
    }

    fn apply_tile(
        &self,
        target: WindowTarget,
        plan: TilePlan,
        tiles: &mut HashMap<WindowTarget, TiledWindow>,
    ) -> bool {
        if !plan.apply(target) {
            return false;
        }
        match plan {
            TilePlan::Resize(frame) => {
                tiles.insert(
                    target,
                    TiledWindow {
                        normal_size: self.normal_size,
                        bounds: frame.outer,
                    },
                );
            }
            TilePlan::Maximize { .. } => {
                tiles.remove(&target);
            }
        }
        true
    }
}

fn run_drag(start: DragStart, motion: &DragMotion, tiles: &mut HashMap<WindowTarget, TiledWindow>) {
    let mode = if start.resize.is_some() {
        DragMode::Resize
    } else {
        DragMode::Move
    };
    let target = start.target;
    let mut session = DragSession::start(&start, mode, motion, tiles);
    let tiling = session.is_some() && start.tiling && mode == DragMode::Move;
    let mut preview = tiling.then(TilePreview::create).and_then(Result::ok);
    let mut metrics = None;
    let mut last = start.origin;

    loop {
        let (phase, point) = motion.sample();
        match phase {
            DragPhase::Closed => return,
            DragPhase::Cancelled => break,
            _ => (),
        }

        let released = phase == DragPhase::Released;
        if target.still_matches() {
            if let Some(preview) = preview.as_ref() {
                preview.pump();
            }
            if (point != last || released)
                && let Some(drag) = session.as_mut()
            {
                let plan = (tiling && (released || drag.placement.is_some()))
                    .then(|| tile_at(target, point, &mut metrics))
                    .flatten();
                if motion.phase() != phase {
                    continue;
                }
                if let Some(preview) = preview.as_mut() {
                    match (phase, plan) {
                        (DragPhase::Dragging, Some(plan)) => preview.show(plan.visible()),
                        _ => preview.hide(),
                    }
                }
                if released
                    && let Some(plan) = plan
                    && drag.apply_tile(target, plan, tiles)
                {
                    break;
                }
                drag.place(&start, mode, point, tiles);
                if drag.placement.is_none()
                    && let Some(preview) = preview.as_mut()
                {
                    preview.hide();
                }
                last = point;
            }
        } else if let Some(preview) = preview.as_mut() {
            preview.hide();
        }
        if released {
            break;
        }
        thread::sleep(DRAG_FRAME_INTERVAL);
    }
    drop(preview);
    motion.finish_drag();
}

fn restore_maximized_drag(
    target: WindowTarget,
    motion: &DragMotion,
    origin: Point,
) -> Option<WindowFrame> {
    // The right button captures immediately. A click alone must leave the
    // window maximized, just like a cancelled gesture that hasn't moved yet.
    loop {
        let (phase, point) = motion.sample();
        if matches!(phase, DragPhase::Closed | DragPhase::Cancelled) || !target.still_matches() {
            return None;
        }
        if point != origin {
            break;
        }
        if phase == DragPhase::Released {
            return None;
        }
        thread::sleep(DRAG_FRAME_INTERVAL);
    }
    if !target.is_responsive() || matches!(motion.phase(), DragPhase::Closed | DragPhase::Cancelled)
    {
        return None;
    }
    // SAFETY: The responsive, captured window was validated above. Post a
    // native restore to its thread before applying any drag geometry.
    if target.is_maximized()
        && !unsafe { ShowWindowAsync(target.window(), SW_SHOWNOACTIVATE) }.as_bool()
    {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(180);
    while !matches!(motion.phase(), DragPhase::Closed | DragPhase::Cancelled)
        && target.still_matches()
    {
        if !target.is_maximized() {
            return target.frame();
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(DRAG_FRAME_INTERVAL);
    }
    None
}

struct TileMetrics {
    dpi: u32,
    frame: WindowFrame,
    limits: (SizeLimits, SizeLimits),
}

#[derive(Clone, Copy)]
enum TilePlan {
    Resize(WindowFrame),
    Maximize { monitor: Monitor, area: Rect },
}

impl TilePlan {
    fn visible(self) -> Rect {
        match self {
            Self::Resize(frame) => frame.visible,
            Self::Maximize { area, .. } => area,
        }
    }

    fn apply(self, target: WindowTarget) -> bool {
        let (monitor, area) = match self {
            Self::Resize(frame) => return target.place(frame.outer, DragMode::Resize),
            Self::Maximize { monitor, area } => (monitor, area),
        };
        if !target.is_responsive() {
            return false;
        }
        // SAFETY: The window's identity was validated. Move it to the destination
        // monitor first because native maximize uses the window's current monitor.
        if Monitor::for_window(target.window()) != monitor {
            let Some(bounds) = target.bounds() else {
                return false;
            };
            let frame = WindowFrame {
                outer: bounds,
                visible: bounds,
            };
            if !target.place(frame.centered_in(area), DragMode::Move) {
                return false;
            }
        }
        // SAFETY: The target identity and responsiveness were checked above.
        unsafe { ShowWindowAsync(target.window(), SW_MAXIMIZE).as_bool() }
    }
}

fn tile_at(
    target: WindowTarget,
    point: Point,
    metrics: &mut Option<TileMetrics>,
) -> Option<TilePlan> {
    let mut clip = RECT::default();
    // SAFETY: GetClipCursor only reads the restriction into our local RECT.
    let point = if unsafe { GetClipCursor(&mut clip) }.is_ok() {
        Rect::from(clip).clamp_point(point)
    } else {
        point
    };
    let monitor = Monitor::at(point);
    let areas = monitor.areas()?;
    let dpi = target.dpi();
    let radius = TILE_DISTANCE_AT_96_DPI.saturating_mul(dpi) / 96;
    let screen = areas.screen;
    let area = areas.work;
    match edge_drop(screen.clamp_point(point), screen, area, radius)? {
        EdgeDrop::Tile(tile) if target.is_sizable() && !target.is_maximized() => {
            if metrics.as_ref().is_none_or(|metrics| metrics.dpi != dpi) {
                *metrics = Some(TileMetrics {
                    dpi,
                    frame: target.frame()?,
                    limits: target.tracking_limits(DragMode::Resize),
                });
            }
            let metrics = metrics.as_ref()?;
            Some(TilePlan::Resize(metrics.frame.fit_tile(
                tile,
                area,
                metrics.limits,
            )))
        }
        EdgeDrop::Maximize if target.can_maximize() || target.is_sizable() => {
            Some(TilePlan::Maximize { monitor, area })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::actions::WindowAction;
    use super::*;
    use crate::geometry::ResizeHandle;
    use crate::settings::WindowPermissions;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Instant;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, EnumThreadWindows, GWL_STYLE,
        GetMessageW, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, PostThreadMessageW,
        SetWindowLongPtrW, WINDOW_EX_STYLE, WM_APP, WM_WINDOWPOSCHANGING, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_MAXIMIZE, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_SYSMENU,
        WS_THICKFRAME,
    };
    use windows::core::BOOL;
    use windows::core::w;

    #[test]
    fn release_publishes_the_final_position_even_after_coalescing() {
        let motion = DragMotion::default();
        assert!(motion.is_idle());
        motion.begin(Point { x: -1200, y: 140 });
        motion.update(Point { x: -1100, y: 100 });
        let end = Point { x: -900, y: -20 };
        motion.release(end);

        assert_eq!(motion.sample(), (DragPhase::Released, end));
        assert!(!motion.is_idle());
    }

    #[test]
    fn aims_the_visible_window_edge_at_the_native_work_area() {
        let work = work_area(Point { x: 0, y: 0 }).unwrap();
        let x = work.x.start;
        let y = work.y.start;
        let outer = Rect::from_edges(x + 80, y + 80, x + 300, y + 260);
        let visible = Rect::from_edges(x + 87, y + 80, x + 293, y + 253);
        let origin = Point {
            x: x + 120,
            y: y + 120,
        };
        let limits = (SizeLimits::new(1, 1_000), SizeLimits::new(1, 1_000));
        let mut drag = DragGeometry::new(None, origin, WindowFrame { outer, visible }, limits);
        let outside = drag.at(
            Point {
                x: origin.x - 66,
                y: origin.y,
            },
            Some(work),
            &[],
            SNAP_DISTANCE_AT_96_DPI,
        );
        assert_eq!(outside.x.start + 7, x + 21);
        let snapped = drag.at(
            Point {
                x: origin.x - 67,
                y: origin.y,
            },
            Some(work),
            &[],
            SNAP_DISTANCE_AT_96_DPI,
        );
        assert_eq!(snapped.x.start + 7, x);
    }

    #[test]
    fn maximized_move_defaults_on_and_fullscreen_gestures_are_blocked() {
        let origin = Point { x: 100, y: 100 };
        // SAFETY: Build two hidden fixtures on this thread using the system STATIC class.
        let (maximized, fullscreen) = unsafe {
            let screen = Monitor::at(origin).areas().unwrap().screen;
            let create = |style, x, y, width, height| {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    None,
                    style,
                    x,
                    y,
                    width,
                    height,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            (
                create(WS_OVERLAPPEDWINDOW | WS_MAXIMIZE, 80, 80, 480, 320),
                create(
                    WS_POPUP | WS_SYSMENU | WS_MAXIMIZEBOX | WS_THICKFRAME,
                    screen.x.start,
                    screen.y.start,
                    screen.x.length(),
                    screen.y.length(),
                ),
            )
        };
        let max = WindowTarget::identify(maximized).unwrap();
        let full = WindowTarget::identify(fullscreen).unwrap();
        assert!(max.accepts_window_actions());
        assert!(!full.accepts_window_actions());
        let defaults = WindowPermissions::default();
        assert_eq!(
            max.drag_state(DragMode::Move, defaults),
            Some(DragState::Maximized)
        );
        assert_eq!(max.drag_state(DragMode::Resize, defaults), None);

        let permissions = WindowPermissions {
            resize_maximized: true,
            move_maximized: false,
        };
        assert_eq!(max.drag_state(DragMode::Move, permissions), None);
        assert_eq!(
            max.drag_state(DragMode::Resize, permissions),
            Some(DragState::Maximized)
        );
        let all_maximized = WindowPermissions {
            move_maximized: true,
            resize_maximized: true,
        };
        assert_eq!(full.drag_state(DragMode::Move, all_maximized), None);
        assert_eq!(full.drag_state(DragMode::Resize, all_maximized), None);
        assert!(!full.can_maximize());

        let motion = DragMotion::default();
        let full_bounds = full.bounds().unwrap();
        let full_grab = Point {
            x: full_bounds.x.start + 30,
            y: full_bounds.y.start + 30,
        };
        let full_end = Point {
            x: full_grab.x + 90,
            y: full_grab.y + 90,
        };
        for resize in [None, Some(ResizeHandle::at(full_bounds, full_grab))] {
            motion.begin(full_grab);
            motion.release(full_end);
            run_drag(
                DragStart {
                    target: full,
                    origin: full_grab,
                    resize,
                    snapping: false,
                    tiling: false,
                    permissions: all_maximized,
                },
                &motion,
                &mut HashMap::new(),
            );
            assert_eq!(full.bounds(), Some(full_bounds));
        }

        // Model a custom-framed window that drops the caption and system menu
        // when maximized. Keep the same monitor-sized bounds to exercise fullscreen detection.
        let custom_maximized = WS_POPUP | WS_MAXIMIZE | WS_MAXIMIZEBOX | WS_THICKFRAME;
        // SAFETY: This thread owns the hidden fixture and only changes its window style.
        unsafe { SetWindowLongPtrW(fullscreen, GWL_STYLE, custom_maximized.0 as isize) };
        assert!(full.is_maximized());
        assert_eq!(full.bounds(), Some(full_bounds));
        assert!(full.accepts_window_actions());
        assert!(full.can_maximize());
        assert_eq!(
            full.drag_state(DragMode::Move, defaults),
            Some(DragState::Maximized)
        );

        let resize = DragStart {
            target: max,
            origin,
            resize: Some(ResizeHandle::at(max.bounds().unwrap(), origin)),
            snapping: false,
            tiling: false,
            permissions,
        };
        motion.begin(origin);
        motion.release(origin);
        run_drag(resize, &motion, &mut HashMap::new());
        assert!(max.is_maximized());
        assert!(motion.is_idle());
        let before = max.bounds().unwrap();
        let grab = Point {
            x: before.x.start + 30,
            y: before.y.start + 30,
        };
        motion.begin(grab);
        motion.release(Point {
            x: grab.x + 90,
            y: grab.y + 90,
        });
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                run_drag(
                    DragStart {
                        target: max,
                        origin: grab,
                        resize: Some(ResizeHandle::at(before, grab)),
                        snapping: false,
                        tiling: false,
                        permissions,
                    },
                    &motion,
                    &mut HashMap::new(),
                )
            });
            let mut message = MSG::default();
            while !worker.is_finished() {
                // SAFETY: Dispatch only messages for the fixture owned by this thread.
                unsafe {
                    while PeekMessageW(&mut message, Some(maximized), 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&message);
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        assert!(!max.is_maximized());
        assert!(max.bounds().unwrap().x.length() < before.x.length());
        // SAFETY: This thread owns both hidden fixtures.
        unsafe {
            DestroyWindow(fullscreen).unwrap();
            DestroyWindow(maximized).unwrap();
        }
    }

    #[test]
    fn native_preview_tile_and_restore_follow_the_same_geometry() {
        let origin = Point { x: 100, y: 100 };
        // SAFETY: Query this test desktop and create a hidden window we own.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                None,
                WS_OVERLAPPEDWINDOW,
                50,
                50,
                300,
                200,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        let areas = Monitor::at(origin).areas().unwrap();
        let target = WindowTarget::identify(window).unwrap();
        let initial = target.bounds().unwrap();
        let area = areas.work;
        let middle_x = area.x.start + area.x.length() / 2;
        let middle_y = area.y.start + area.y.length() / 2;
        let left = Point {
            x: areas.screen.x.start.saturating_sub(1),
            y: middle_y,
        };
        // Verify that tiling activates before the cursor reaches the right edge.
        let right = Point {
            x: area.x.end - 1 - (28 * target.dpi() / 96) as i32,
            y: middle_y,
        };
        let bottom = Point {
            x: middle_x,
            y: areas.screen.y.end - 1,
        };
        let motion = DragMotion::default();
        let mut history = HashMap::new();
        let start = || DragStart {
            target,
            origin,
            resize: None,
            snapping: false,
            tiling: true,
            permissions: WindowPermissions::default(),
        };
        {
            let preview = tile_at(target, left, &mut None).unwrap().visible();
            TilePreview::create().unwrap().verify_window(preview);
        }

        motion.begin(origin);
        motion.update(left);
        motion.cancel();
        run_drag(start(), &motion, &mut history);
        assert_eq!(target.bounds(), Some(initial));

        let frame = target.frame().unwrap().visible;
        let snap_grab = Point {
            x: frame.x.start + 1,
            y: frame.y.start + frame.y.length() / 2,
        };
        motion.begin(snap_grab);
        motion.release(Point {
            x: area.x.start,
            y: snap_grab.y,
        });
        run_drag(
            DragStart {
                origin: snap_grab,
                snapping: true,
                tiling: false,
                ..start()
            },
            &motion,
            &mut history,
        );
        let snapped = target.bounds().unwrap();
        assert_eq!(
            snapped.x.start + (frame.x.start - initial.x.start),
            area.x.start
        );
        assert_eq!(snapped.x.length(), initial.x.length());
        assert!(history.is_empty());

        for (drop, tile) in [
            (
                left,
                Rect::from_edges(area.x.start, area.y.start, middle_x, area.y.end),
            ),
            (
                right,
                Rect::from_edges(middle_x, area.y.start, area.x.end, area.y.end),
            ),
            (
                bottom,
                Rect::from_edges(area.x.start, middle_y, area.x.end, area.y.end),
            ),
        ] {
            assert!(target.place(initial, DragMode::Resize));
            let expected = target.frame().unwrap().outer_for_visible(tile);
            assert_eq!(tile_at(target, drop, &mut None).unwrap().visible(), tile);
            motion.begin(origin);
            motion.release(drop);
            run_drag(start(), &motion, &mut history);
            assert_eq!(target.bounds(), Some(expected));
            assert!(motion.is_idle());
        }

        // Drop the already tiled window on another edge without an intermediate
        // free-move sample, then pull it away using its original dimensions.
        motion.begin(origin);
        motion.release(right);
        run_drag(start(), &motion, &mut history);
        let tiled = target.bounds().unwrap();
        assert_eq!(
            history[&target].normal_size.at_dpi(target.dpi()),
            (initial.x.length(), initial.y.length())
        );
        let grab = Point {
            x: tiled.x.start + 50,
            y: tiled.y.start + 70,
        };
        let free = Point {
            x: middle_x,
            y: middle_y,
        };
        let free_drag = || DragStart {
            origin: grab,
            tiling: false,
            ..start()
        };
        motion.begin(grab);
        motion.release(free);
        run_drag(free_drag(), &motion, &mut history);
        let restored = target.bounds().unwrap();
        assert_eq!(
            (restored.x.length(), restored.y.length()),
            (initial.x.length(), initial.y.length())
        );
        assert!(history.is_empty());

        motion.begin(origin);
        motion.release(left);
        run_drag(start(), &motion, &mut history);
        let changed = Rect::from_edges(50, 50, 470, 310);
        assert!(target.place(changed, DragMode::Resize));
        motion.begin(grab);
        motion.release(free);
        run_drag(free_drag(), &motion, &mut history);
        let moved = target.bounds().unwrap();
        assert_eq!(
            (moved.x.length(), moved.y.length()),
            (changed.x.length(), changed.y.length())
        );
        assert!(history.is_empty());

        motion.begin(origin);
        motion.release(left);
        run_drag(start(), &motion, &mut history);
        let tiled = target.bounds().unwrap();
        assert!(history.contains_key(&target));
        // SAFETY: This thread owns the fixture and changes only its window style.
        let framed =
            unsafe { SetWindowLongPtrW(window, GWL_STYLE, (WS_POPUP | WS_MAXIMIZEBOX).0 as isize) };
        let top = Point {
            x: middle_x,
            y: areas.screen.y.start,
        };
        assert!(!target.can_maximize());
        assert!(tile_at(target, top, &mut None).is_none());
        let grab = Point {
            x: tiled.x.start + tiled.x.length() / 2,
            y: tiled.y.start + tiled.y.length() / 2,
        };
        let (dx, dy) = (50, 30);
        motion.begin(grab);
        motion.release(Point {
            x: grab.x + dx,
            y: grab.y + dy,
        });
        run_drag(
            DragStart {
                origin: grab,
                tiling: false,
                ..start()
            },
            &motion,
            &mut history,
        );
        assert_eq!(
            target.bounds(),
            Some(Rect::from_edges(
                tiled.x.start + dx,
                tiled.y.start + dy,
                tiled.x.end + dx,
                tiled.y.end + dy
            ))
        );
        assert!(history.is_empty());
        // SAFETY: Restore the fixture's original style for the preview checks.
        unsafe { SetWindowLongPtrW(window, GWL_STYLE, framed) };

        for finish in [DragMotion::cancel, DragMotion::close] {
            motion.begin(origin);
            let (showed, cleared, showed_again) = thread::scope(|scope| {
                let probe = scope.spawn(|| {
                    let wait_for = |visible| {
                        let deadline = Instant::now() + Duration::from_secs(2);
                        loop {
                            if visible_window_on_thread(target.thread_id()) == visible {
                                break true;
                            }
                            if Instant::now() >= deadline {
                                break false;
                            }
                            thread::sleep(Duration::from_millis(8));
                        }
                    };
                    motion.update(left);
                    let showed = wait_for(true);
                    motion.update(free);
                    let cleared = wait_for(false);
                    motion.update(right);
                    let showed_again = wait_for(true);
                    finish(&motion);
                    (showed, cleared, showed_again)
                });
                run_drag(start(), &motion, &mut history);
                probe.join().unwrap()
            });
            assert!(showed && cleared && showed_again);
            assert!(!visible_window_on_thread(target.thread_id()));
        }

        // SAFETY: This thread owns the hidden test window.
        unsafe { DestroyWindow(window) }.unwrap();
    }

    fn visible_window_on_thread(owner: u32) -> bool {
        unsafe extern "system" fn find(window: HWND, found: LPARAM) -> BOOL {
            // SAFETY: EnumThreadWindows passes our writable flag synchronously.
            unsafe {
                if IsWindowVisible(window).as_bool() {
                    *(found.0 as *mut bool) = true;
                    return false.into();
                }
            }
            true.into()
        }

        let mut found = false;
        // SAFETY: Enumerate only the fixture thread's windows.
        unsafe {
            let _ = EnumThreadWindows(owner, Some(find), LPARAM(&mut found as *mut bool as isize));
        }
        found
    }

    #[test]
    fn waits_for_native_resize_and_skips_an_unresponsive_window() {
        struct ResizeProbe {
            returned: Receiver<()>,
            completed_early: Sender<bool>,
        }

        unsafe extern "system" fn probe_resize(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
            id: usize,
            data: usize,
        ) -> LRESULT {
            // SAFETY: The owner keeps the probe alive until it destroys the window.
            unsafe {
                if message == WM_WINDOWPOSCHANGING {
                    let _ = RemoveWindowSubclass(window, Some(probe_resize), id);
                    let probe = &*std::ptr::with_exposed_provenance::<ResizeProbe>(data);
                    let returned = probe.returned.recv_timeout(Duration::from_millis(150));
                    let _ = probe.completed_early.send(returned.is_ok());
                }
                DefSubclassProc(window, message, wparam, lparam)
            }
        }

        let (ready, window_created) = channel();
        let (paused, window_paused) = channel();
        let (resume, wait) = channel();
        let (returned, wait_for_resize) = channel();
        let (completed_early, probe_result) = channel();
        let owner = thread::spawn(move || {
            let probe = ResizeProbe {
                returned: wait_for_resize,
                completed_early,
            };
            // SAFETY: This thread owns the hidden window and keeps its subclass data alive.
            unsafe {
                let window = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!(""),
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    200,
                    200,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
                assert!(
                    SetWindowSubclass(
                        window,
                        Some(probe_resize),
                        0,
                        std::ptr::from_ref(&probe).expose_provenance(),
                    )
                    .as_bool()
                );
                let _ = ready.send(WindowTarget::identify(window).unwrap());

                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).0 > 0 && message.message != WM_APP {
                    DispatchMessageW(&message);
                }
                let _ = paused.send(());
                // Wait without pumping messages. The timeout also lets the test
                // recover if a future change bypasses the bounded send.
                let _ = wait.recv_timeout(Duration::from_secs(4));
                DestroyWindow(window).unwrap();
            }
        });

        let window = window_created.recv().unwrap();
        let expected = Rect::from_edges(50, 75, 290, 325);
        let requested = window.place(expected, DragMode::Resize);
        let _ = returned.send(());
        let completed_early = probe_result.recv_timeout(Duration::from_secs(2)).unwrap();
        let reached_position = window.bounds() == Some(expected);

        let mut actions = WindowActions::default();
        let pinned = actions.apply(window, WindowAction::AlwaysOnTop).is_ok()
            && super::super::actions::is_topmost(window);
        let unpinned = actions.apply(window, WindowAction::AlwaysOnTop).is_ok()
            && !super::super::actions::is_topmost(window);
        // SAFETY: The test owner published its thread ID after creating the queue.
        unsafe { PostThreadMessageW(window.thread_id(), WM_APP, WPARAM(0), LPARAM(0)) }.unwrap();
        window_paused.recv_timeout(Duration::from_secs(2)).unwrap();

        let destination = Rect::from_edges(100, 100, 400, 400);
        let started = Instant::now();
        let resized = window.place(destination, DragMode::Resize);
        let moved = window.place(destination, DragMode::Move);
        let elapsed = started.elapsed();
        let _ = resume.send(());
        owner.join().unwrap();

        assert!(
            !completed_early,
            "the resize returned before the window processed it"
        );
        assert!(requested && reached_position);
        assert!(pinned && unpinned);
        assert!(!resized && !moved);
        assert!(
            elapsed < Duration::from_secs(2),
            "placement took {elapsed:?}"
        );
    }
}
