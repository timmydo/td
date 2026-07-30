use crate::framebuffer::Framebuffer;
use crate::keyboard::{
    KeyInput, KeyboardSnapshot, KeyboardState, ModifierState, RoutedKeyboardEvent,
};
use crate::layout::{Command, ViewLayout};
use crate::scene::{Scene, Surface, SurfaceKey};
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
    active: Arc<AtomicBool>,
}

pub enum KeyboardDelivery {
    Event(RoutedKeyboardEvent),
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
        if !self.active.load(Ordering::Acquire) {
            return KeyboardQueueResult::Sent;
        }
        self.try_send(KeyboardDelivery::Event(event))
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
    keyboard_subscribers: BTreeMap<u64, KeyboardSender>,
}

impl Runtime {
    pub fn new(framebuffer: Framebuffer) -> Runtime {
        Runtime {
            scene: Scene::new(),
            framebuffer,
            layout: Arc::new(BTreeMap::new()),
            subscribers: BTreeMap::new(),
            keyboard: KeyboardState::default(),
            keyboard_subscribers: BTreeMap::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.framebuffer.width
    }

    pub fn height(&self) -> usize {
        self.framebuffer.height
    }

    pub fn repaint(&mut self) -> Result<(), String> {
        self.framebuffer.paint(&self.scene)
    }

    pub fn commit(&mut self, key: SurfaceKey, surface: Surface) -> Result<(), String> {
        let layout_changed = self.scene.commit(key, surface)?;
        self.repaint()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn remove(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.remove(key);
        self.repaint()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> Result<(), String> {
        let layout_changed = self.scene.unmap(key);
        self.repaint()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        let layout_changed = self.scene.remove_client(client);
        self.repaint()?;
        if layout_changed && self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn command(&mut self, command: Command) -> Result<(), String> {
        self.scene.command(command);
        self.repaint()?;
        if self.refresh_layout()? {
            self.publish_layout();
        }
        Ok(())
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        self.scene
            .move_pointer(dx, dy, self.framebuffer.width, self.framebuffer.height);
        self.repaint()
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

    pub fn subscribe_keyboard_with_activity(
        &mut self,
        id: u64,
        active: Arc<AtomicBool>,
    ) -> Result<KeyboardSubscription, String> {
        if self.keyboard_subscribers.contains_key(&id) {
            return Err(format!("keyboard subscriber {id} already exists"));
        }
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_KEYBOARD_DELIVERIES);
        let sender = KeyboardSender {
            channel: Arc::new(Mutex::new(KeyboardChannel::Open(sender))),
            active,
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
        if !sender.active.load(Ordering::Acquire) {
            return Ok(false);
        }
        match sender.try_send(KeyboardDelivery::DeleteId(object)) {
            KeyboardQueueResult::Sent => Ok(true),
            KeyboardQueueResult::Closed => Ok(false),
            KeyboardQueueResult::Overflowed => Err(format!(
                "keyboard event queue overflowed before deleting object {object}"
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
        let events = self.keyboard.set_focus(self.scene.focused())?;
        self.publish_keyboard(events);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::{KeyState, KeyboardEvent, MOD_SHIFT};
    use crate::layout::{Axis, Direction};
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
        runtime.move_pointer(1, 1).unwrap();
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
                KeyboardDelivery::DeleteId(_) => None,
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
