use crate::framebuffer::Framebuffer;
use crate::keyboard::{
    KeyInput, KeyboardSnapshot, KeyboardState, ModifierState, RoutedKeyboardEvent,
};
use crate::help::HelpAction;
use crate::launcher::{LaunchRequest, LauncherAction};
use crate::layout::{Command, ViewLayout};
use crate::pointer::{
    PointerButtonInput, PointerButtonState, PointerSnapshot, PointerState, PointerTarget,
    RoutedPointerFrame,
};
use crate::scene::{Scene, SharedInputRegion, Surface, SurfaceKey};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

pub(crate) const MAX_PENDING_KEYBOARD_DELIVERIES: usize = 64;

#[derive(Clone)]
pub struct SubscriptionStop {
    wake: SyncSender<()>,
    stopped: Arc<AtomicBool>,
}

impl SubscriptionStop {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

pub struct LayoutSubscription {
    receiver: Receiver<()>,
    stop: SubscriptionStop,
}

impl LayoutSubscription {
    pub fn split(self) -> (Receiver<()>, SubscriptionStop) {
        (self.receiver, self.stop)
    }
}

#[derive(Clone)]
pub struct KeyboardSubscriptionStop {
    stopped: Arc<AtomicBool>,
    sender: KeyboardSender,
}

impl KeyboardSubscriptionStop {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.sender.close();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct KeyboardSender {
    channel: Arc<Mutex<KeyboardChannel>>,
    keyboard_active: Arc<AtomicBool>,
    pointer_active: Arc<AtomicBool>,
}

pub enum KeyboardDelivery {
    Event(RoutedKeyboardEvent),
    Pointer(RoutedPointerFrame),
    DeleteId(u32),
}

enum KeyboardChannel {
    Open(SyncSender<KeyboardDelivery>),
    Overflowed,
    Closed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum KeyboardQueueResult {
    Sent,
    Overflowed,
    Closed,
}

impl KeyboardSender {
    fn try_send_event(&self, event: RoutedKeyboardEvent) -> KeyboardQueueResult {
        if !self.keyboard_active.load(Ordering::Acquire) {
            return KeyboardQueueResult::Sent;
        }
        self.try_send(KeyboardDelivery::Event(event))
    }

    fn try_send_pointer(&self, frame: RoutedPointerFrame) -> KeyboardQueueResult {
        if !self.pointer_active.load(Ordering::Acquire) {
            return KeyboardQueueResult::Sent;
        }
        self.try_send(KeyboardDelivery::Pointer(frame))
    }

    fn try_send(&self, delivery: KeyboardDelivery) -> KeyboardQueueResult {
        let mut channel = match self.channel.lock() {
            Ok(channel) => channel,
            Err(poisoned) => poisoned.into_inner(),
        };
        match &*channel {
            KeyboardChannel::Open(sender) => match sender.try_send(delivery) {
                Ok(()) => KeyboardQueueResult::Sent,
                Err(TrySendError::Full(_)) => {
                    *channel = KeyboardChannel::Overflowed;
                    KeyboardQueueResult::Overflowed
                }
                Err(TrySendError::Disconnected(_)) => {
                    *channel = KeyboardChannel::Closed;
                    KeyboardQueueResult::Closed
                }
            },
            KeyboardChannel::Overflowed => KeyboardQueueResult::Overflowed,
            KeyboardChannel::Closed => KeyboardQueueResult::Closed,
        }
    }

    fn close(&self) {
        let mut channel = match self.channel.lock() {
            Ok(channel) => channel,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !matches!(*channel, KeyboardChannel::Overflowed) {
            *channel = KeyboardChannel::Closed;
        }
    }
}

pub struct KeyboardSubscription {
    receiver: Receiver<KeyboardDelivery>,
    stop: KeyboardSubscriptionStop,
}

impl KeyboardSubscription {
    pub fn split(self) -> (Receiver<KeyboardDelivery>, KeyboardSubscriptionStop) {
        (self.receiver, self.stop)
    }
}

pub struct Runtime {
    scene: Scene,
    framebuffer: Framebuffer,
    layout: Arc<BTreeMap<SurfaceKey, ViewLayout>>,
    subscribers: BTreeMap<u64, SyncSender<()>>,
    keyboard: KeyboardState,
    pointer: PointerState,
    keyboard_subscribers: BTreeMap<u64, KeyboardSender>,
    pending_paint: bool,
}

impl Runtime {
    pub fn new(framebuffer: Framebuffer) -> Runtime {
        Runtime {
            scene: Scene::new(),
            framebuffer,
            layout: Arc::new(BTreeMap::new()),
            subscribers: BTreeMap::new(),
            keyboard: KeyboardState::default(),
            pointer: PointerState::default(),
            keyboard_subscribers: BTreeMap::new(),
            pending_paint: false,
        }
    }

    pub fn width(&self) -> usize {
        self.framebuffer.width
    }

    pub fn height(&self) -> usize {
        self.framebuffer.height
    }

    /// Pessimistic across the paint, as the framebuffer's shadow copy is across
    /// its write: a paint that failed leaves the screen owed, not settled.
    pub fn repaint(&mut self) -> Result<(), String> {
        self.pending_paint = true;
        self.framebuffer.paint(&self.scene)?;
        self.pending_paint = false;
        Ok(())
    }

    /// Owe a paint instead of taking one. The scene is already current, so any
    /// repaint before the flush settles the debt.
    pub fn defer_repaint(&mut self) {
        self.pending_paint = true;
    }

    pub fn flush_paint(&mut self) -> Result<(), String> {
        if self.pending_paint {
            return self.repaint();
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn paint_pending(&self) -> bool {
        self.pending_paint
    }

    #[cfg(test)]
    pub fn take_writes(&mut self) -> Vec<(u64, usize)> {
        self.framebuffer.take_writes()
    }

    #[cfg(test)]
    pub fn commit(&mut self, key: SurfaceKey, surface: Surface) -> Result<(), String> {
        let layout_changed = self.scene.commit(key, surface)?;
        self.finish_commit(layout_changed)
    }

    pub fn commit_with_input_region(
        &mut self,
        key: SurfaceKey,
        surface: Surface,
        input_region: Option<SharedInputRegion>,
    ) -> Result<(), String> {
        let layout_changed = self.scene.commit(key, surface)?;
        self.scene.set_input_region(key, input_region);
        self.finish_commit(layout_changed)
    }

    fn finish_commit(&mut self, layout_changed: bool) -> Result<(), String> {
        self.repaint()?;
        self.refresh_focus()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn set_input_region(
        &mut self,
        key: SurfaceKey,
        input_region: Option<SharedInputRegion>,
    ) -> Result<(), String> {
        if self.scene.set_input_region(key, input_region) {
            self.refresh_focus()?;
        }
        Ok(())
    }

    pub fn remove(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.remove(key);
        self.repaint()?;
        self.refresh_focus()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.unmap(key);
        self.repaint()?;
        self.refresh_focus()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        let layout_changed = self.scene.remove_client(client);
        self.repaint()?;
        self.refresh_focus()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn command(&mut self, command: Command) -> Result<(), String> {
        self.scene.command(command);
        // A tiling command is what a user reaches for when the screen looks
        // wrong, so make it the immediate repair for pixels the compositor did
        // not write and its shadow copy therefore cannot see.
        self.framebuffer.resend();
        self.repaint()?;
        self.refresh_focus()?;
        if self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn launcher(&mut self, action: LauncherAction) -> Result<Option<LaunchRequest>, String> {
        let checkpoint = self.scene.launcher_checkpoint();
        let request = self.scene.launcher(action);
        if let Err(error) = self.repaint() {
            self.scene.restore_launcher(checkpoint);
            return Err(error);
        }
        if let Err(error) = self.refresh_focus() {
            self.scene.restore_launcher(checkpoint);
            if let Err(restore_error) = self.repaint() {
                return Err(format!(
                    "{error}; restore launcher after focus failure: {restore_error}"
                ));
            }
            return Err(error);
        }
        Ok(request)
    }

    pub fn launcher_visible(&self) -> bool {
        self.scene.launcher_visible()
    }

    /// The cheat sheet has no model to restore, unlike the launcher: it is one
    /// bit. Both failures put that bit back and repaint what was there —
    /// `refresh_focus` as well as the paint, since a scene showing a modal
    /// sheet the input layer does not believe is up withdraws pointer hover
    /// with nothing left able to dismiss it.
    pub fn help(&mut self, action: HelpAction) -> Result<bool, String> {
        let before = self.scene.help_visible();
        if self.scene.set_help(action.target(before)) == before {
            return Ok(before);
        }
        if let Err(error) = self.repaint() {
            return Err(self.restore_help(before, error));
        }
        if let Err(error) = self.refresh_focus() {
            return Err(self.restore_help(before, error));
        }
        Ok(self.scene.help_visible())
    }

    fn restore_help(&mut self, before: bool, error: String) -> String {
        self.scene.set_help(before);
        match self.repaint() {
            Ok(()) => error,
            Err(restore_error) => format!("{error}; restore help overlay: {restore_error}"),
        }
    }

    #[cfg(test)]
    pub fn help_visible(&self) -> bool {
        self.scene.help_visible()
    }

    #[cfg(test)]
    pub fn exhaust_pointer_revision(&mut self) {
        self.pointer.exhaust_revision();
    }

    #[cfg(test)]
    pub fn fail_next_repaint(&mut self) {
        self.framebuffer.fail_next_paint();
    }

    pub fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String> {
        self.scene
            .move_pointer(dx, dy, self.framebuffer.width, self.framebuffer.height);
        let modal = self.scene.modal();
        let (hover, grab) = self.routed_pointer_targets();
        let mut modal_buttons = Vec::new();
        let buttons = if modal {
            modal_buttons.extend(
                buttons
                    .iter()
                    .copied()
                    .filter(|button| button.state == PointerButtonState::Released),
            );
            &modal_buttons
        } else {
            buttons
        };
        let result = self.pointer.frame(time, hover, grab, buttons)?;
        self.publish_pointer(result.frames);
        if dx != 0 || dy != 0 {
            // Coalesced by the caller: a reader batch that carries many reports
            // owes one paint, not one per report. Before the focus below, so a
            // focusing repaint settles this debt instead of being followed by
            // a second paint of the identical scene.
            self.defer_repaint();
        }
        // Click to focus. The pointer model reports the surface a press
        // ESTABLISHED a grab on, which is by construction the one the button
        // event was routed to, so focus cannot disagree with delivery; and a
        // press made DURING a grab establishes nothing, so dragging off a tile
        // does not take focus with it. A modal launcher filters presses out
        // above, so nothing is established and the overlay keeps focus.
        if let Some(clicked) = result.pressed_on {
            self.focus_surface(clicked)?;
        }
        Ok(())
    }

    fn focus_surface(&mut self, key: SurfaceKey) -> Result<(), String> {
        if !self.scene.focus_key(key) {
            return Ok(());
        }
        // No `framebuffer.resend()` here, unlike a tiling command: that one is
        // the repair gesture for pixels the compositor did not write, and a
        // click is not reached for when the screen looks wrong.
        self.repaint()?;
        self.refresh_focus()?;
        if self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn key(&mut self, input: KeyInput) -> Result<(), String> {
        if let Some(event) = self.keyboard.key(input)? {
            self.publish_keyboard_event(event);
        }
        Ok(())
    }

    pub fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
        if let Some(event) = self.keyboard.modifiers(modifiers)? {
            self.publish_keyboard_event(event);
        }
        Ok(())
    }

    pub fn keyboard_snapshot(&self) -> KeyboardSnapshot {
        self.keyboard.snapshot()
    }

    pub fn pointer_snapshot(&self) -> PointerSnapshot {
        self.pointer.snapshot()
    }

    pub fn layout_snapshot(&self) -> Arc<BTreeMap<SurfaceKey, ViewLayout>> {
        Arc::clone(&self.layout)
    }

    #[cfg(test)]
    pub fn surface_size(&self, key: SurfaceKey) -> Option<(usize, usize)> {
        self.scene.surface_size(key)
    }

    pub fn subscribe(&mut self, id: u64) -> Result<LayoutSubscription, String> {
        if self.subscribers.contains_key(&id) {
            return Err(format!("layout subscriber {id} already exists"));
        }
        let (wake, receiver) = mpsc::sync_channel(1);
        wake.try_send(())
            .map_err(|_| "new layout subscription could not be primed".to_string())?;
        self.subscribers.insert(id, wake.clone());
        Ok(LayoutSubscription {
            receiver,
            stop: SubscriptionStop {
                wake,
                stopped: Arc::new(AtomicBool::new(false)),
            },
        })
    }

    pub fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    #[cfg(test)]
    pub fn subscribe_keyboard(&mut self, id: u64) -> Result<KeyboardSubscription, String> {
        self.subscribe_keyboard_with_activity(id, Arc::new(AtomicBool::new(true)))
    }

    #[cfg(test)]
    pub fn subscribe_keyboard_with_activity(
        &mut self,
        id: u64,
        keyboard_active: Arc<AtomicBool>,
    ) -> Result<KeyboardSubscription, String> {
        self.subscribe_input_with_activity(id, keyboard_active, Arc::new(AtomicBool::new(false)))
    }

    pub fn subscribe_input_with_activity(
        &mut self,
        id: u64,
        keyboard_active: Arc<AtomicBool>,
        pointer_active: Arc<AtomicBool>,
    ) -> Result<KeyboardSubscription, String> {
        if self.keyboard_subscribers.contains_key(&id) {
            return Err(format!("keyboard subscriber {id} already exists"));
        }
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_KEYBOARD_DELIVERIES);
        let sender = KeyboardSender {
            channel: Arc::new(Mutex::new(KeyboardChannel::Open(sender))),
            keyboard_active,
            pointer_active,
        };
        self.keyboard_subscribers.insert(id, sender.clone());
        Ok(KeyboardSubscription {
            receiver,
            stop: KeyboardSubscriptionStop {
                stopped: Arc::new(AtomicBool::new(false)),
                sender,
            },
        })
    }

    pub fn unsubscribe_keyboard(&mut self, id: u64) {
        if let Some(sender) = self.keyboard_subscribers.remove(&id) {
            sender.close();
        }
    }

    pub fn queue_keyboard_delete(&mut self, id: u64, object: u32) -> Result<bool, String> {
        let Some(sender) = self.keyboard_subscribers.get(&id).cloned() else {
            return Ok(false);
        };
        if !sender.keyboard_active.load(Ordering::Acquire)
            && !sender.pointer_active.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        match sender.try_send(KeyboardDelivery::DeleteId(object)) {
            KeyboardQueueResult::Sent => Ok(true),
            KeyboardQueueResult::Closed => Ok(false),
            KeyboardQueueResult::Overflowed => Err(format!(
                "seat event queue overflowed before deleting object {object}"
            )),
        }
    }

    pub fn wake_layout(&mut self, id: u64) {
        let disconnected = self
            .subscribers
            .get(&id)
            .is_some_and(|wake| matches!(wake.try_send(()), Err(TrySendError::Disconnected(()))));
        if disconnected {
            self.subscribers.remove(&id);
        }
    }

    fn refresh_layout(&mut self) -> Result<bool, String> {
        let next: BTreeMap<SurfaceKey, ViewLayout> = self
            .scene
            .views(self.framebuffer.width, self.framebuffer.height)
            .into_iter()
            .map(|view| (view.key, view))
            .collect();
        if self.layout.as_ref() == &next {
            return Ok(false);
        }
        self.layout = Arc::new(next);
        Ok(true)
    }

    fn publish_layout(&mut self) {
        self.subscribers.retain(|_, wake| match wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => true,
            Err(TrySendError::Disconnected(())) => false,
        });
    }

    fn publish_keyboard(&mut self, events: Vec<RoutedKeyboardEvent>) {
        for event in events {
            self.publish_keyboard_event(event);
        }
    }

    fn publish_keyboard_event(&mut self, event: RoutedKeyboardEvent) {
        let client = event.event.surface().client;
        let result = self
            .keyboard_subscribers
            .get(&client)
            .map(|sender| sender.try_send_event(event));
        // Overflow remains registered so deletion cannot bypass queued events.
        if matches!(result, Some(KeyboardQueueResult::Closed)) {
            self.keyboard_subscribers.remove(&client);
        }
    }

    fn refresh_focus(&mut self) -> Result<(), String> {
        let keyboard = self.keyboard.set_focus(self.scene.focused())?;
        self.publish_keyboard(keyboard);
        let (hover, grab) = self.routed_pointer_targets();
        let pointer = self.pointer.refresh(hover, grab)?;
        self.publish_pointer(pointer);
        Ok(())
    }

    fn routed_pointer_targets(&self) -> (Option<PointerTarget>, Option<PointerTarget>) {
        if !self.scene.modal() {
            return self.pointer_targets();
        }
        let (_, grab) = self.pointer_targets();
        (None, grab)
    }

    fn pointer_targets(&self) -> (Option<PointerTarget>, Option<PointerTarget>) {
        let (hover, grab) = self.scene.pointer_targets(
            self.pointer.grab_surface(),
            self.framebuffer.width,
            self.framebuffer.height,
        );
        (
            hover.map(|point| PointerTarget {
                surface: point.key,
                x: point.x,
                y: point.y,
            }),
            grab.map(|point| PointerTarget {
                surface: point.key,
                x: point.x,
                y: point.y,
            }),
        )
    }

    fn publish_pointer(&mut self, frames: Vec<RoutedPointerFrame>) {
        for frame in frames {
            let client = frame.client;
            let result = self
                .keyboard_subscribers
                .get(&client)
                .map(|sender| sender.try_send_pointer(frame));
            if matches!(result, Some(KeyboardQueueResult::Closed)) {
                self.keyboard_subscribers.remove(&client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::{KeyState, KeyboardEvent, MOD_SHIFT};
    use crate::layout::{Axis, Direction};
    use crate::pointer::{PointerButtonState, PointerEvent};
    use crate::scene::SHM_XRGB8888;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn surface(color: [u8; 4]) -> Surface {
        Surface {
            width: 100,
            height: 100,
            pixels: color.repeat(10_000),
            format: SHM_XRGB8888,
        }
    }

    fn recv_event(receiver: &Receiver<KeyboardDelivery>) -> RoutedKeyboardEvent {
        match receiver.recv().unwrap() {
            KeyboardDelivery::Event(event) => event,
            KeyboardDelivery::Pointer(frame) => {
                panic!("unexpected pointer frame for client {}", frame.client);
            }
            KeyboardDelivery::DeleteId(id) => {
                panic!("unexpected queued delete_id for {id}");
            }
        }
    }

    /// Bounded, not a bare `recv`: a compositor that stops emitting an event
    /// a test expects should FAIL it, not wedge the whole binary — which is
    /// how a mutation of the modal pointer path presented before this.
    fn recv_pointer(receiver: &Receiver<KeyboardDelivery>) -> RoutedPointerFrame {
        match receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("expected a pointer delivery")
        {
            KeyboardDelivery::Pointer(frame) => frame,
            KeyboardDelivery::Event(event) => {
                panic!("unexpected keyboard event revision {}", event.revision);
            }
            KeyboardDelivery::DeleteId(id) => {
                panic!("unexpected queued delete_id for {id}");
            }
        }
    }

    #[test]
    fn commands_repaint_the_file_backed_output() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        runtime
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 2,
                },
                surface([4, 5, 6, 0]),
            )
            .unwrap();
        let second_focused = fs::read(&cleanup.0).unwrap();
        runtime.command(Command::Focus(Direction::Left)).unwrap();
        let first_focused = fs::read(&cleanup.0).unwrap();
        assert_ne!(first_focused, second_focused);
    }

    #[test]
    fn many_pointer_reports_owe_one_paint_and_the_flush_takes_it() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-deferred-paint-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        runtime.take_writes();
        assert!(!runtime.paint_pending());

        for step in 0..16 {
            runtime.pointer_frame(step, 1, 0, &[]).unwrap();
        }
        assert!(runtime.paint_pending());
        assert_eq!(runtime.take_writes(), vec![]);

        runtime.flush_paint().unwrap();
        assert!(!runtime.paint_pending());
        assert_eq!(runtime.take_writes().len(), 1);

        // A second flush owes nothing and must not touch the device.
        runtime.flush_paint().unwrap();
        assert_eq!(runtime.take_writes(), vec![]);
    }

    #[test]
    fn a_tiling_command_rewrites_the_whole_image() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-command-resend-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        runtime.take_writes();

        runtime.pointer_frame(1, 30, 30, &[]).unwrap();
        runtime.flush_paint().unwrap();
        let banded = runtime.take_writes();
        assert_eq!(banded.len(), 1);
        assert!(banded.iter().all(|(_, length)| *length < 120 * 4 * 80));

        // The repair a user can reach for when foreign pixels are on screen.
        runtime.command(Command::Focus(Direction::Left)).unwrap();
        assert_eq!(runtime.take_writes(), vec![(0, 120 * 4 * 80)]);
    }

    #[test]
    fn a_failed_flush_leaves_the_paint_owed_so_teardown_retries_it() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-failed-flush-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        runtime.take_writes();

        runtime.pointer_frame(1, 5, 5, &[]).unwrap();
        runtime.fail_next_repaint();
        assert!(runtime.flush_paint().is_err());
        // Settling on a failed paint would close the device showing the pointer
        // where it no longer is, with nothing left to say so.
        assert!(runtime.paint_pending());
        assert_eq!(runtime.take_writes(), vec![]);

        runtime.flush_paint().unwrap();
        assert!(!runtime.paint_pending());
        assert_eq!(runtime.take_writes().len(), 1);
    }

    #[test]
    fn a_failed_repaint_owes_the_screen_even_when_no_pointer_moved() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-failed-repaint-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.fail_next_repaint();
        assert!(runtime.repaint().is_err());
        assert!(runtime.paint_pending());
        runtime.flush_paint().unwrap();
        assert!(!runtime.paint_pending());
    }

    #[test]
    fn a_repaint_from_another_source_settles_the_deferred_pointer_paint() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-settled-paint-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.pointer_frame(1, 4, 4, &[]).unwrap();
        assert!(runtime.paint_pending());
        runtime
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        // The commit painted the scene the motion had already updated.
        assert!(!runtime.paint_pending());
        runtime.take_writes();
        runtime.flush_paint().unwrap();
        assert_eq!(runtime.take_writes(), vec![]);
    }

    #[test]
    fn launcher_actions_repaint_without_mutating_layout() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-launcher-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 640, 240, 640 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        let layout = runtime.layout_snapshot();
        let hidden = fs::read(&cleanup.0).unwrap();

        assert_eq!(runtime.launcher(LauncherAction::Open).unwrap(), None);
        let first = fs::read(&cleanup.0).unwrap();
        assert_ne!(first, hidden);
        assert!(Arc::ptr_eq(&layout, &runtime.layout_snapshot()));

        assert_eq!(runtime.launcher(LauncherAction::Previous).unwrap(), None);
        let second = fs::read(&cleanup.0).unwrap();
        assert_ne!(second, first);
        assert_eq!(runtime.launcher(LauncherAction::Activate).unwrap(), None);
        assert_eq!(fs::read(&cleanup.0).unwrap(), hidden);

        runtime.launcher(LauncherAction::Open).unwrap();
        assert_eq!(
            runtime.launcher(LauncherAction::Activate).unwrap(),
            Some(LaunchRequest::Terminal)
        );
        assert_eq!(fs::read(&cleanup.0).unwrap(), hidden);
    }

    #[test]
    fn launcher_repaint_failure_restores_visibility_and_activation() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-launcher-failure-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 640, 240, 640 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        runtime.launcher(LauncherAction::Open).unwrap();
        assert!(runtime.launcher_visible());

        runtime.framebuffer.fail_next_paint();
        assert!(runtime.launcher(LauncherAction::Activate).is_err());
        assert!(runtime.launcher_visible());
        assert_eq!(
            runtime.launcher(LauncherAction::Activate).unwrap(),
            Some(LaunchRequest::Terminal)
        );
        assert!(!runtime.launcher_visible());
    }

    #[test]
    fn layout_notifications_are_bounded_deduplicated_and_stoppable() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-subscription-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe(7).unwrap();
        let (receiver, stop) = subscription.split();
        receiver.try_recv().unwrap();

        runtime
            .commit(
                SurfaceKey {
                    client: 7,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        runtime
            .commit(
                SurfaceKey {
                    client: 7,
                    object: 2,
                },
                surface([4, 5, 6, 0]),
            )
            .unwrap();
        receiver.try_recv().unwrap();
        assert!(receiver.try_recv().is_err());

        let tiled = runtime.layout_snapshot();
        assert!(Arc::ptr_eq(&tiled, &runtime.layout_snapshot()));
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        assert!(Arc::ptr_eq(&tiled, &runtime.layout_snapshot()));
        assert!(receiver.try_recv().is_err());
        runtime.command(Command::ToggleFullscreen).unwrap();
        let fullscreen = runtime.layout_snapshot();
        assert!(!Arc::ptr_eq(&tiled, &fullscreen));
        assert!(Arc::ptr_eq(&fullscreen, &runtime.layout_snapshot()));
        receiver.try_recv().unwrap();

        runtime.wake_layout(7);
        runtime.wake_layout(7);
        receiver.try_recv().unwrap();
        assert!(receiver.try_recv().is_err());

        stop.stop();
        receiver.recv().unwrap();
        assert!(stop.is_stopped());
        runtime.unsubscribe(7);
    }

    #[test]
    fn pixel_refreshes_and_pointer_motion_do_not_publish_layout() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-refresh-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe(9).unwrap();
        let (receiver, stop) = subscription.split();
        receiver.try_recv().unwrap();
        let key = SurfaceKey {
            client: 9,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        receiver.try_recv().unwrap();
        let layout = runtime.layout_snapshot();
        runtime.commit(key, surface([4, 5, 6, 0])).unwrap();
        runtime.pointer_frame(1, 1, 1, &[]).unwrap();
        assert!(Arc::ptr_eq(&layout, &runtime.layout_snapshot()));
        assert!(receiver.try_recv().is_err());
        stop.stop();
        runtime.unsubscribe(9);
    }

    #[test]
    fn keyboard_events_follow_focus_across_clients_and_commands() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-keyboard-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = runtime.subscribe_keyboard(1).unwrap();
        let second = runtime.subscribe_keyboard(2).unwrap();
        let (first_events, first_stop) = first.split();
        let (second_events, second_stop) = second.split();
        let first_key = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second_key = SurfaceKey {
            client: 2,
            object: 20,
        };

        runtime.commit(first_key, surface([1, 2, 3, 0])).unwrap();
        assert!(matches!(
            recv_event(&first_events).event,
            KeyboardEvent::Enter {
                surface,
                ref keys
            } if surface == first_key && keys.is_empty()
        ));
        assert!(matches!(
            recv_event(&first_events).event,
            KeyboardEvent::Modifiers {
                surface,
                state: ModifierState {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }
            } if surface == first_key
        ));

        runtime.commit(second_key, surface([4, 5, 6, 0])).unwrap();
        assert_eq!(
            recv_event(&first_events).event,
            KeyboardEvent::Leave { surface: first_key }
        );
        assert!(matches!(
            recv_event(&second_events).event,
            KeyboardEvent::Enter { surface, .. } if surface == second_key
        ));
        recv_event(&second_events);

        runtime
            .key(KeyInput {
                time: 33,
                key: 30,
                state: KeyState::Pressed,
            })
            .unwrap();
        let modifiers = ModifierState {
            depressed: MOD_SHIFT,
            ..ModifierState::default()
        };
        runtime.modifiers(modifiers).unwrap();
        assert!(matches!(
            recv_event(&second_events).event,
            KeyboardEvent::Key {
                surface,
                input: KeyInput {
                    time: 33,
                    key: 30,
                    state: KeyState::Pressed
                }
            } if surface == second_key
        ));
        assert!(matches!(
            recv_event(&second_events).event,
            KeyboardEvent::Modifiers {
                surface,
                state
            } if surface == second_key && state == modifiers
        ));

        runtime.command(Command::Focus(Direction::Left)).unwrap();
        assert_eq!(
            recv_event(&second_events).event,
            KeyboardEvent::Leave {
                surface: second_key
            }
        );
        assert!(matches!(
            recv_event(&first_events).event,
            KeyboardEvent::Enter {
                surface,
                ref keys
            } if surface == first_key && keys == &[30]
        ));
        assert!(matches!(
            recv_event(&first_events).event,
            KeyboardEvent::Modifiers {
                surface,
                state
            } if surface == first_key && state == modifiers
        ));

        first_stop.stop();
        second_stop.stop();
        runtime.unsubscribe_keyboard(1);
        runtime.unsubscribe_keyboard(2);
    }

    #[test]
    fn pointer_frames_follow_hit_testing_and_implicit_grab_across_clients() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-pointer-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, 120, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first_active = Arc::new(AtomicBool::new(true));
        let second_active = Arc::new(AtomicBool::new(true));
        let first = runtime
            .subscribe_input_with_activity(
                1,
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&first_active),
            )
            .unwrap();
        let second = runtime
            .subscribe_input_with_activity(
                2,
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&second_active),
            )
            .unwrap();
        let (first_events, first_stop) = first.split();
        let (second_events, second_stop) = second.split();
        let first_key = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second_key = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first_key, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second_key, surface([4, 5, 6, 0])).unwrap();
        let layout = runtime.layout_snapshot();
        let first_rect = layout.get(&first_key).unwrap().rect;
        let second_rect = layout.get(&second_key).unwrap().rect;

        runtime
            .pointer_frame(
                10,
                i32::try_from(first_rect.x.saturating_add(2)).unwrap(),
                i32::try_from(first_rect.y.saturating_add(3)).unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(
            recv_pointer(&first_events).events,
            vec![PointerEvent::Enter {
                target: PointerTarget {
                    surface: first_key,
                    x: 2,
                    y: 3,
                },
            }]
        );

        let press = PointerButtonInput {
            time: 11,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        runtime.pointer_frame(11, 0, 0, &[press]).unwrap();
        assert_eq!(
            recv_pointer(&first_events).events,
            vec![PointerEvent::Button {
                surface: first_key,
                input: press,
            }]
        );

        let dx = i32::try_from(second_rect.x)
            .unwrap()
            .saturating_sub(i32::try_from(first_rect.x.saturating_add(2)).unwrap())
            .saturating_add(1);
        let dy = i32::try_from(second_rect.y)
            .unwrap()
            .saturating_sub(i32::try_from(first_rect.y.saturating_add(3)).unwrap())
            .saturating_add(1);
        runtime.pointer_frame(12, dx, dy, &[]).unwrap();
        assert!(matches!(
            recv_pointer(&first_events).events.as_slice(),
            [PointerEvent::Motion { time: 12, target }]
                if target.surface == first_key
        ));
        assert!(second_events.try_recv().is_err());

        let release = PointerButtonInput {
            time: 13,
            button: 272,
            state: PointerButtonState::Released,
        };
        runtime.pointer_frame(13, 0, 0, &[release]).unwrap();
        assert_eq!(
            recv_pointer(&first_events).events,
            vec![
                PointerEvent::Button {
                    surface: first_key,
                    input: release,
                },
                PointerEvent::Leave { surface: first_key },
            ]
        );
        assert!(matches!(
            recv_pointer(&second_events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == second_key
        ));

        first_active.store(false, Ordering::Release);
        runtime.unmap(first_key).unwrap();
        assert!(first_events.try_recv().is_err());
        first_stop.stop();
        second_stop.stop();
        runtime.unsubscribe_keyboard(1);
        runtime.unsubscribe_keyboard(2);
    }

    #[test]
    fn a_press_focuses_the_surface_under_the_pointer() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-click-focus-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, 120, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second, surface([4, 5, 6, 0])).unwrap();
        // Mapping focuses the newest, so the click has somewhere to move to.
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));
        let layout = runtime.layout_snapshot();
        let rect = layout.get(&first).unwrap().rect;

        // Fresh per frame: an input's `time` rides through to the client, so
        // reusing one in a later frame would send a stale timestamp.
        let press = |time| PointerButtonInput {
            time,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        let release = |time| PointerButtonInput {
            time,
            button: 272,
            state: PointerButtonState::Released,
        };
        // Hovering alone is not focusing: this compositor is click-to-focus.
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));

        runtime.pointer_frame(2, 0, 0, &[press(2)]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));

        // A second press on the same tile changes nothing, and a press made
        // while a grab is HELD does not follow the pointer off the tile.
        runtime.pointer_frame(3, 0, 0, &[release(3)]).unwrap();
        runtime.pointer_frame(4, 0, 0, &[press(4)]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
        let dx = i32::try_from(layout.get(&second).unwrap().rect.x)
            .unwrap()
            .saturating_sub(i32::try_from(rect.x.saturating_add(2)).unwrap())
            .saturating_add(2);
        runtime.pointer_frame(5, dx, 0, &[]).unwrap();
        let second_press = PointerButtonInput {
            time: 6,
            button: 273,
            state: PointerButtonState::Pressed,
        };
        runtime.pointer_frame(6, 0, 0, &[second_press]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));

        // Grab over: now the pointer is on the other tile and a press takes.
        runtime.pointer_frame(7, 0, 0, &[release(7)]).unwrap();
        runtime
            .pointer_frame(
                8,
                0,
                0,
                &[PointerButtonInput {
                    time: 8,
                    button: 273,
                    state: PointerButtonState::Released,
                }],
            )
            .unwrap();
        runtime.pointer_frame(9, 0, 0, &[press(9)]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));

        // A HELD grab must not keep re-asserting its focus, or the keyboard
        // could never move it while a button is down: `Super+Left` here would
        // be undone by the next twitch of the mouse.
        runtime.command(Command::Focus(Direction::Left)).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
        runtime.pointer_frame(10, 1, 0, &[]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
    }

    #[test]
    fn a_release_and_press_in_one_report_focuses_where_the_press_landed() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-click-focus-retarget-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, 120, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second, surface([4, 5, 6, 0])).unwrap();
        let layout = runtime.layout_snapshot();
        let first_rect = layout.get(&first).unwrap().rect;
        let second_rect = layout.get(&second).unwrap().rect;

        // Press on the left tile, then drag onto the right one holding it.
        runtime
            .pointer_frame(
                1,
                i32::try_from(first_rect.x.saturating_add(2)).unwrap(),
                i32::try_from(first_rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        runtime
            .pointer_frame(
                2,
                0,
                0,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
        let dx = i32::try_from(second_rect.x)
            .unwrap()
            .saturating_sub(i32::try_from(first_rect.x.saturating_add(2)).unwrap())
            .saturating_add(2);
        runtime.pointer_frame(3, dx, 0, &[]).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));

        // Rolling from one button to the other inside ONE report: the mouse
        // reports its whole bitmap per poll, so both edges arrive before a
        // SYN_REPORT. The grab moves to the right tile and so must focus.
        runtime
            .pointer_frame(
                4,
                0,
                0,
                &[
                    PointerButtonInput {
                        time: 4,
                        button: 272,
                        state: PointerButtonState::Released,
                    },
                    PointerButtonInput {
                        time: 4,
                        button: 273,
                        state: PointerButtonState::Pressed,
                    },
                ],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));
    }

    #[test]
    fn the_help_sheet_toggles_is_modal_to_the_pointer_and_survives_a_failed_paint() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-help-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 900, 600, 900 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second, surface([4, 5, 6, 0])).unwrap();
        assert!(!runtime.help_visible());

        assert!(runtime.help(HelpAction::Toggle).unwrap());
        assert!(runtime.help_visible());
        // Modal to the pointer exactly as the launcher is: a click cannot
        // reach the tile the sheet covers.
        let rect = runtime.layout_snapshot().get(&first).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        runtime
            .pointer_frame(
                2,
                0,
                0,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));

        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        assert!(!runtime.help_visible());
        // Closing it hands the pointer back, so the same click now takes.
        runtime
            .pointer_frame(
                3,
                0,
                0,
                &[PointerButtonInput {
                    time: 3,
                    button: 273,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));

        // The two halves of "modal" are independent, and the legs above are
        // held up by either one alone. This is the other half on its own: a
        // grab held from BEFORE the sheet opened keeps its pointer target, so
        // only the button filter stops a new press reaching that client.
        let subscription = runtime
            .subscribe_input_with_activity(
                1,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        runtime
            .pointer_frame(
                4,
                0,
                0,
                &[PointerButtonInput {
                    time: 4,
                    button: 274,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        while events.try_recv().is_ok() {}
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        while events.try_recv().is_ok() {}
        runtime
            .pointer_frame(
                5,
                0,
                0,
                &[PointerButtonInput {
                    time: 5,
                    button: 275,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert!(events.try_recv().is_err(), "a press reached a covered tile");
        stop.stop();
        runtime.unsubscribe_keyboard(1);
        runtime.help(HelpAction::Close).unwrap();

        // A paint that fails puts the bit back, or the compositor would think
        // a sheet is up that the screen never showed — and every key after it
        // would be swallowed as a dismissal.
        runtime.fail_next_repaint();
        assert!(runtime.help(HelpAction::Toggle).is_err());
        assert!(!runtime.help_visible());
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        runtime.fail_next_repaint();
        assert!(runtime.help(HelpAction::Close).is_err());
        assert!(runtime.help_visible());

    }

    #[test]
    fn the_sheet_cannot_be_raised_behind_the_launcher() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-help-exclusive-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 900, 600, 900 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.launcher(LauncherAction::Open).unwrap();
        assert!(runtime.launcher_visible());
        // The dispatch cannot ask for this — the launcher branch swallows
        // `/` — so the refusal is where "never both up" actually lives, not
        // in an ordering nobody can see.
        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        assert!(!runtime.help_visible());
        assert!(runtime.launcher_visible());

        runtime.launcher(LauncherAction::Close).unwrap();
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        assert!(runtime.help_visible());
    }

    #[test]
    fn the_sheet_withdraws_pointer_hover_from_the_tile_it_covers() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-help-hover-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 900, 600, 900 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime
            .subscribe_input_with_activity(
                7,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 7,
            object: 3,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(4)).unwrap(),
                i32::try_from(rect.y.saturating_add(4)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        // No button is held, so the filter has nothing to drop: leaving is
        // the hover withdrawal's own work. A client under the sheet must not
        // keep tracking a pointer the operator cannot aim at it.
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        assert_eq!(
            recv_pointer(&events).events,
            vec![PointerEvent::Leave { surface: key }]
        );
        runtime.pointer_frame(2, 3, 3, &[]).unwrap();
        assert!(events.try_recv().is_err(), "motion crossed the sheet");

        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        // Withdrawing that hover is what advances the pointer revision, so
        // exhausting it is the only injectable `refresh_focus` failure — and
        // that failure lands AFTER a successful paint, with the sheet already
        // on the screen. Leaving the bit set there would withdraw hover for
        // good, since `settle_help` is never reached on an error and nothing
        // would be able to dismiss it.
        runtime.exhaust_pointer_revision();
        assert!(runtime.help(HelpAction::Toggle).is_err());
        assert!(!runtime.help_visible());
        stop.stop();
        runtime.unsubscribe_keyboard(7);
    }

    #[test]
    fn a_press_while_the_launcher_is_modal_does_not_move_focus() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-click-focus-modal-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, 120, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second, surface([4, 5, 6, 0])).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));
        let rect = runtime.layout_snapshot().get(&first).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();

        runtime.launcher(LauncherAction::Open).unwrap();
        runtime
            .pointer_frame(
                2,
                0,
                0,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));

        // Close it and the same click takes, so the assertion above is about
        // the overlay rather than about a click that could never focus.
        runtime.launcher(LauncherAction::Close).unwrap();
        runtime
            .pointer_frame(
                3,
                0,
                0,
                &[PointerButtonInput {
                    time: 3,
                    button: 273,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
    }

    #[test]
    fn a_press_over_no_surface_leaves_focus_alone() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-click-focus-empty-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, 120, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let first = SurfaceKey {
            client: 1,
            object: 10,
        };
        let second = SurfaceKey {
            client: 2,
            object: 20,
        };
        runtime.commit(first, surface([1, 2, 3, 0])).unwrap();
        runtime.commit(second, surface([4, 5, 6, 0])).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));
        // The gap between tiles belongs to no surface. A click there is not a
        // request to focus nothing — it is a click on the desktop.
        runtime.pointer_frame(1, 0, 0, &[]).unwrap();
        runtime
            .pointer_frame(
                2,
                0,
                0,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(second));
        // Same click one tile over DOES take, so the assertion above is about
        // the gap rather than about a harness that focuses nothing.
        runtime
            .pointer_frame(
                3,
                0,
                0,
                &[PointerButtonInput {
                    time: 3,
                    button: 272,
                    state: PointerButtonState::Released,
                }],
            )
            .unwrap();
        let rect = runtime.layout_snapshot().get(&first).unwrap().rect;
        runtime
            .pointer_frame(
                3,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        runtime
            .pointer_frame(
                4,
                0,
                0,
                &[PointerButtonInput {
                    time: 4,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(first));
    }

    #[test]
    fn launcher_modal_pointer_releases_an_existing_grab_without_new_presses() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-launcher-pointer-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime
            .subscribe_input_with_activity(
                5,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 5,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        let press = PointerButtonInput {
            time: 2,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        runtime.pointer_frame(2, 0, 0, &[press]).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Button { input, .. }] if *input == press
        ));
        runtime.launcher(LauncherAction::Open).unwrap();
        runtime.pointer_frame(3, 5, 0, &[]).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Motion { target, .. }] if target.surface == key
        ));

        let modal_press = PointerButtonInput {
            time: 4,
            button: 273,
            state: PointerButtonState::Pressed,
        };
        runtime.pointer_frame(4, 0, 0, &[modal_press]).unwrap();
        assert!(events.try_recv().is_err());

        let release = PointerButtonInput {
            time: 5,
            button: 272,
            state: PointerButtonState::Released,
        };
        runtime.pointer_frame(5, 0, 0, &[release]).unwrap();
        assert_eq!(
            recv_pointer(&events).events,
            vec![
                PointerEvent::Button {
                    surface: key,
                    input: release,
                },
                PointerEvent::Leave { surface: key },
            ]
        );
        runtime.launcher(LauncherAction::Close).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));
        runtime.pointer_frame(6, 0, 0, &[]).unwrap();
        assert!(events.try_recv().is_err());

        stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn launcher_modal_unmap_clears_an_existing_grab_immediately() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-launcher-pointer-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime
            .subscribe_input_with_activity(
                5,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 5,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));
        let press = PointerButtonInput {
            time: 2,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        runtime.pointer_frame(2, 0, 0, &[press]).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Button { input, .. }] if *input == press
        ));
        runtime.launcher(LauncherAction::Open).unwrap();

        runtime.unmap(key).unwrap();
        assert_eq!(
            recv_pointer(&events).events,
            [PointerEvent::Leave { surface: key }]
        );
        assert_eq!(runtime.pointer.snapshot().focus, None);
        assert_eq!(runtime.pointer.grab_surface(), None);

        stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn launcher_visibility_refreshes_ungrabbed_pointer_focus_immediately() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-launcher-pointer-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime
            .subscribe_input_with_activity(
                5,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 5,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        runtime.launcher(LauncherAction::Open).unwrap();
        assert_eq!(
            recv_pointer(&events).events,
            [PointerEvent::Leave { surface: key }]
        );
        runtime.launcher(LauncherAction::Close).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn workspace_switch_cancels_a_grab_through_runtime_focus_refresh() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-workspace-grab-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime
            .subscribe_input_with_activity(
                5,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 5,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(1)).unwrap(),
                i32::try_from(rect.y.saturating_add(1)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));
        runtime
            .pointer_frame(
                2,
                0,
                0,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            )
            .unwrap();
        recv_pointer(&events);

        runtime.command(Command::SwitchWorkspace(2)).unwrap();
        assert_eq!(
            recv_pointer(&events).events,
            [PointerEvent::Leave { surface: key }]
        );
        assert_eq!(runtime.pointer_snapshot().focus, None);
        runtime.command(Command::SwitchWorkspace(1)).unwrap();
        assert!(matches!(
            recv_pointer(&events).events.as_slice(),
            [PointerEvent::Enter { target }] if target.surface == key
        ));

        stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn inactive_pointer_delivery_is_quiet_and_pointer_overflow_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-pointer-bound-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let pointer_active = Arc::new(AtomicBool::new(false));
        let subscription = runtime
            .subscribe_input_with_activity(
                5,
                Arc::new(AtomicBool::new(false)),
                Arc::clone(&pointer_active),
            )
            .unwrap();
        let (events, stop) = subscription.split();
        let key = SurfaceKey {
            client: 5,
            object: 1,
        };
        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let rect = runtime.layout_snapshot().get(&key).unwrap().rect;
        runtime
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(2)).unwrap(),
                &[],
            )
            .unwrap();
        assert!(events.try_recv().is_err());
        assert!(!runtime.queue_keyboard_delete(5, 8).unwrap());

        pointer_active.store(true, Ordering::Release);
        for time in 2..=u32::try_from(MAX_PENDING_KEYBOARD_DELIVERIES)
            .unwrap()
            .saturating_add(2)
        {
            let dx = if time % 2 == 0 { 1 } else { -1 };
            runtime.pointer_frame(time, dx, 0, &[]).unwrap();
        }
        let retained: Vec<RoutedPointerFrame> = events
            .iter()
            .filter_map(|delivery| match delivery {
                KeyboardDelivery::Pointer(frame) => Some(frame),
                KeyboardDelivery::Event(_) | KeyboardDelivery::DeleteId(_) => None,
            })
            .collect();
        assert_eq!(retained.len(), MAX_PENDING_KEYBOARD_DELIVERIES);
        assert!(runtime.queue_keyboard_delete(5, 9).is_err());
        stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn a_full_keyboard_queue_is_dropped_without_blocking_input() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-keyboard-bound-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(5).unwrap();
        let (events, stop) = subscription.split();
        runtime
            .commit(
                SurfaceKey {
                    client: 5,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        recv_event(&events);
        recv_event(&events);
        for time in 0..=MAX_PENDING_KEYBOARD_DELIVERIES {
            runtime
                .key(KeyInput {
                    time: u32::try_from(time).unwrap(),
                    key: 30,
                    state: if time % 2 == 0 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                })
                .unwrap();
        }
        let retained: Vec<RoutedKeyboardEvent> = events
            .iter()
            .filter_map(|delivery| match delivery {
                KeyboardDelivery::Event(event) => Some(event),
                KeyboardDelivery::Pointer(_) | KeyboardDelivery::DeleteId(_) => None,
            })
            .collect();
        assert_eq!(retained.len(), MAX_PENDING_KEYBOARD_DELIVERIES);
        assert!(runtime.queue_keyboard_delete(5, 7).is_err());
        assert!(runtime.subscribe_keyboard(5).is_err());
        stop.stop();
        runtime.unsubscribe_keyboard(5);
        let replacement = runtime.subscribe_keyboard(5).unwrap();
        let (_, replacement_stop) = replacement.split();
        replacement_stop.stop();
        runtime.unsubscribe_keyboard(5);
    }

    #[test]
    fn inactive_keyboard_subscription_ignores_events_and_surface_deletion() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-keyboard-inactive-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let active = Arc::new(AtomicBool::new(false));
        let subscription = runtime
            .subscribe_keyboard_with_activity(7, Arc::clone(&active))
            .unwrap();
        let (events, stop) = subscription.split();
        runtime
            .commit(
                SurfaceKey {
                    client: 7,
                    object: 1,
                },
                surface([1, 2, 3, 0]),
            )
            .unwrap();
        assert!(events.try_recv().is_err());
        assert!(!runtime.queue_keyboard_delete(7, 1).unwrap());

        active.store(true, Ordering::Release);
        runtime
            .key(KeyInput {
                time: 12,
                key: 30,
                state: KeyState::Pressed,
            })
            .unwrap();
        assert!(matches!(
            events.recv().unwrap(),
            KeyboardDelivery::Event(RoutedKeyboardEvent {
                event: KeyboardEvent::Key { .. },
                ..
            })
        ));
        stop.stop();
        runtime.unsubscribe_keyboard(7);
    }

    #[test]
    fn keyboard_delete_is_queued_or_sent_directly_after_worker_exit() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-keyboard-delete-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(5).unwrap();
        let (deliveries, stop) = subscription.split();
        assert!(runtime.queue_keyboard_delete(5, 7).unwrap());
        assert!(matches!(
            deliveries.recv().unwrap(),
            KeyboardDelivery::DeleteId(7)
        ));
        drop(deliveries);
        assert!(!runtime.queue_keyboard_delete(5, 8).unwrap());
        stop.stop();
        assert!(!runtime.queue_keyboard_delete(5, 9).unwrap());
    }

    #[test]
    fn keyboard_stop_closes_its_receiver_without_runtime_cleanup() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-keyboard-stop-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(6).unwrap();
        let (events, stop) = subscription.split();
        stop.stop();
        assert!(stop.is_stopped());
        assert!(events.recv().is_err());
        runtime.unsubscribe_keyboard(6);
    }
}
