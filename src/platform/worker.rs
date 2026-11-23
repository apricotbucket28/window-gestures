use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use super::actions::{WindowAction, WindowMenuState};
use super::window::WindowTarget;
use crate::geometry::{Point, ResizeHandle};
use crate::settings::WindowPermissions;

pub(super) struct DragStart {
    pub(super) target: WindowTarget,
    pub(super) origin: Point,
    pub(super) resize: Option<ResizeHandle>,
    pub(super) snapping: bool,
    pub(super) tiling: bool,
    pub(super) permissions: WindowPermissions,
}

pub(super) enum WindowCommand {
    Drag(DragStart),
    Action(WindowTarget, WindowAction),
    ReadMenuState(WindowTarget, Sender<WindowMenuState>),
}

pub(super) fn channel() -> (WindowCommands, WindowRequests) {
    let (sender, receiver) = sync_channel(1);
    let motion = Arc::new(DragMotion::default());
    let commands = WindowCommands(Arc::new(CommandClient {
        sender,
        motion: Arc::clone(&motion),
    }));
    (commands, WindowRequests { receiver, motion })
}

struct CommandClient {
    sender: SyncSender<Option<WindowCommand>>,
    motion: Arc<DragMotion>,
}

impl CommandClient {
    fn send(&self, command: WindowCommand, reserved: DragPhase) -> bool {
        if self.sender.try_send(Some(command)).is_ok() {
            return true;
        }
        self.motion.transition(reserved, DragPhase::Idle);
        false
    }
}

#[derive(Clone)]
pub(super) struct WindowCommands(Arc<CommandClient>);

impl WindowCommands {
    pub(super) fn is_idle(&self) -> bool {
        self.0.motion.is_idle()
    }

    pub(super) fn start_drag(&self, start: DragStart) -> bool {
        self.0.motion.begin(start.origin)
            && self.0.send(WindowCommand::Drag(start), DragPhase::Dragging)
    }

    pub(super) fn update_drag(&self, point: Point) {
        self.0.motion.update(point);
    }

    pub(super) fn release_drag(&self, point: Point) {
        self.0.motion.release(point);
    }

    pub(super) fn cancel_drag(&self) {
        self.0.motion.cancel();
    }

    pub(super) fn run_action(&self, target: WindowTarget, action: WindowAction) -> bool {
        self.0.motion.transition(DragPhase::Idle, DragPhase::Action)
            && self
                .0
                .send(WindowCommand::Action(target, action), DragPhase::Action)
    }

    pub(super) fn reserve_menu(&self) -> Option<MenuReservation> {
        self.0
            .motion
            .transition(DragPhase::Idle, DragPhase::Menu)
            .then(|| MenuReservation(Arc::downgrade(&self.0)))
    }

    pub(super) fn close(&self) {
        self.0.motion.close();
        // A full queue already wakes the receiver, which checks the closed phase.
        let _ = self.0.sender.try_send(None);
    }
}

pub(super) struct MenuReservation(Weak<CommandClient>);

impl MenuReservation {
    pub(super) fn menu_state(&self, target: WindowTarget) -> Result<WindowMenuState, &'static str> {
        const ERROR: &str = "The window worker could not read the window actions state.";
        let client = self.0.upgrade().ok_or(ERROR)?;
        let (reply, state) = mpsc::channel();
        // This reservation keeps the worker free to answer with read-only window queries.
        client
            .sender
            .try_send(Some(WindowCommand::ReadMenuState(target, reply)))
            .map_err(|_| ERROR)?;
        state.recv().map_err(|_| ERROR)
    }

    pub(super) fn select(
        mut self,
        target: WindowTarget,
        action: WindowAction,
    ) -> Result<(), &'static str> {
        let client = std::mem::take(&mut self.0).upgrade();
        if let Some(client) = client
            && client.motion.transition(DragPhase::Menu, DragPhase::Action)
            && !client.send(WindowCommand::Action(target, action), DragPhase::Action)
        {
            return Err("The window worker could not accept the selected action.");
        }
        Ok(())
    }
}

impl Drop for MenuReservation {
    fn drop(&mut self) {
        if let Some(client) = self.0.upgrade() {
            client.motion.transition(DragPhase::Menu, DragPhase::Idle);
        }
    }
}

pub(super) struct WindowRequests {
    receiver: Receiver<Option<WindowCommand>>,
    motion: Arc<DragMotion>,
}

impl WindowRequests {
    pub(super) fn motion(&self) -> &DragMotion {
        &self.motion
    }

    pub(super) fn next(
        &self,
        timeout: Option<Duration>,
    ) -> Result<Option<WindowCommand>, RecvTimeoutError> {
        let command = if let Some(timeout) = timeout {
            self.receiver.recv_timeout(timeout)?
        } else {
            self.receiver
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected)?
        };
        Ok(command.filter(|_| self.motion.phase() != DragPhase::Closed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum DragPhase {
    Idle,
    Dragging,
    Released,
    Cancelled,
    Closed,
    Menu,
    Action,
}

// Only one operation can own the worker at a time. The hook publishes points
// separately so sending input never waits for a foreign window to respond.
#[derive(Default)]
pub(super) struct DragMotion {
    position: AtomicU64,
    phase: AtomicU8,
}

impl DragMotion {
    pub(super) fn phase(&self) -> DragPhase {
        use DragPhase::*;
        match self.phase.load(Ordering::Acquire) {
            phase if phase == Idle as u8 => Idle,
            phase if phase == Dragging as u8 => Dragging,
            phase if phase == Released as u8 => Released,
            phase if phase == Cancelled as u8 => Cancelled,
            phase if phase == Closed as u8 => Closed,
            phase if phase == Menu as u8 => Menu,
            phase if phase == Action as u8 => Action,
            _ => unreachable!("only DragPhase is stored in this atomic"),
        }
    }

    pub(super) fn is_idle(&self) -> bool {
        self.phase.load(Ordering::Acquire) == DragPhase::Idle as u8
    }

    pub(super) fn begin(&self, origin: Point) -> bool {
        if !self.transition(DragPhase::Idle, DragPhase::Dragging) {
            return false;
        }
        self.update(origin);
        true
    }

    pub(super) fn update(&self, point: Point) {
        let packed = (u64::from(point.y as u32) << 32) | u64::from(point.x as u32);
        self.position.store(packed, Ordering::Relaxed);
    }

    pub(super) fn release(&self, point: Point) {
        self.update(point);
        self.transition(DragPhase::Dragging, DragPhase::Released);
    }

    pub(super) fn cancel(&self) {
        self.transition(DragPhase::Dragging, DragPhase::Cancelled);
    }

    pub(super) fn finish_action(&self) {
        self.transition(DragPhase::Action, DragPhase::Idle);
    }

    pub(super) fn finish_drag(&self) {
        let phase = self.phase();
        if matches!(phase, DragPhase::Released | DragPhase::Cancelled) {
            self.transition(phase, DragPhase::Idle);
        }
    }

    fn transition(&self, expected: DragPhase, next: DragPhase) -> bool {
        self.phase
            .compare_exchange(
                expected as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    pub(super) fn close(&self) {
        self.phase.store(DragPhase::Closed as u8, Ordering::Release);
    }

    pub(super) fn sample(&self) -> (DragPhase, Point) {
        // Reading phase first ensures a released sample includes the final point.
        let phase = self.phase();
        let packed = self.position.load(Ordering::Relaxed);
        (
            phase,
            Point {
                x: packed as i32,
                y: (packed >> 32) as i32,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;

    #[test]
    fn dropping_an_unselected_menu_frees_the_worker() {
        let (commands, _requests) = channel();
        let reservation = commands.reserve_menu().unwrap();
        assert!(commands.reserve_menu().is_none());

        drop(reservation);
        assert!(commands.reserve_menu().is_some());
    }

    #[test]
    fn a_pending_menu_does_not_keep_the_worker_alive_at_shutdown() {
        let (commands, requests) = channel();
        let reservation = commands.reserve_menu().unwrap();

        commands.close();
        assert!(matches!(requests.receiver.try_recv(), Ok(None)));
        drop(commands);
        assert!(matches!(
            requests.receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));

        drop(reservation);
        assert_eq!(requests.motion().phase(), DragPhase::Closed);
    }
}
