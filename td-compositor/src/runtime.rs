use crate::framebuffer::Framebuffer;
use crate::help::HelpAction;
use crate::keyboard::{
    KeyInput, KeyboardSnapshot, KeyboardState, ModifierState, RoutedKeyboardEvent, MOD_ALT,
};
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

/// `BTN_LEFT`. The only button a drag answers to: the others are a client's
/// to interpret, and a band has no client to hand them to.
const POINTER_BUTTON_LEFT: u32 = 272;

pub struct Runtime {
    scene: Scene,
    framebuffer: Framebuffer,
    layout: Arc<BTreeMap<SurfaceKey, ViewLayout>>,
    subscribers: BTreeMap<u64, SyncSender<()>>,
    keyboard: KeyboardState,
    pointer: PointerState,
    keyboard_subscribers: BTreeMap<u64, KeyboardSender>,
    pending_paint: bool,
    /// The window a press picked up, held until the button is released.
    /// Nothing in the pointer model carries it: neither press a drag begins
    /// with is delivered, so no client is told any of this.
    dragging: Option<Drag>,
}

/// A drag in progress, and what holds it open: a band drag by the BUTTON
/// alone, an Alt drag by the modifier as well.
struct Drag {
    key: SurfaceKey,
    held_by_alt: bool,
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
            dragging: None,
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
        self.settle(layout_changed)
    }

    pub fn commit_with_input_region(
        &mut self,
        key: SurfaceKey,
        surface: Surface,
        input_region: Option<SharedInputRegion>,
    ) -> Result<(), String> {
        let layout_changed = self.scene.commit(key, surface)?;
        self.scene.set_input_region(key, input_region);
        self.settle(layout_changed)
    }

    /// Settle the screen, the input aim and the clients for a change already
    /// made to the scene. `layout_changed` false skips rebuilding the
    /// published map, which the commit path takes on every client frame.
    ///
    /// Every step runs whatever the one before it did: only the paint is
    /// owed anywhere, so a step skipped here is lost outright. Failures are
    /// reported together rather than first-wins, as the overlay rollback
    /// paths report theirs.
    fn settle(&mut self, layout_changed: bool) -> Result<(), String> {
        let mut failures = Vec::new();
        if let Err(error) = self.repaint() {
            failures.push(error);
        }
        if let Err(error) = self.refresh_focus() {
            failures.push(error);
        }
        if layout_changed && self.refresh_layout() {
            self.publish_layout();
        }
        if failures.is_empty() {
            return Ok(());
        }
        Err(failures.join("; "))
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

    /// Take a toplevel's title, and repaint if the band showing it is on
    /// screen. Both halves of that condition matter: an unchanged title is
    /// what a client resending the same string every commit sends, and a
    /// title on a surface with no pixels is the ordinary opening sequence —
    /// set before the first buffer, drawn by the commit that maps it.
    ///
    /// Deferring instead would not do. `flush_paint` is driven from the input
    /// loop, so a rename on an idle machine would sit owed and unpainted, and
    /// the band would show the previous name with nothing on screen to say so.
    pub fn set_title(&mut self, key: SurfaceKey, title: String) -> Result<(), String> {
        if self.scene.set_title(key, title) && self.scene.is_mapped(key) {
            self.repaint()?;
        }
        Ok(())
    }

    /// The xdg_toplevel went. Its wl_surface may not have.
    pub fn forget_title(&mut self, key: SurfaceKey) -> Result<(), String> {
        self.scene.forget_title(key);
        Ok(())
    }

    #[cfg(test)]
    pub fn title(&self, key: SurfaceKey) -> Option<String> {
        self.scene.title(key).map(str::to_string)
    }

    /// Forget a drag whose window has gone. Object ids are recycled per
    /// client, so a stale one does not merely name nothing — it can come to
    /// name a DIFFERENT window, and the release would move one the operator
    /// never picked up.
    fn forget_drag(&mut self, gone: impl Fn(SurfaceKey) -> bool) {
        if self.dragging.as_ref().is_some_and(|drag| gone(drag.key)) {
            self.dragging = None;
        }
    }

    pub fn remove(&mut self, key: SurfaceKey) -> Result<(), String> {
        self.forget_drag(|dragged| dragged == key);
        let layout_changed = self.scene.remove(key);
        self.settle(layout_changed)
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> Result<(), String> {
        self.forget_drag(|dragged| dragged == key);
        let layout_changed = self.scene.unmap(key);
        self.settle(layout_changed)
    }

    pub fn remove_client(&mut self, client: u64) -> Result<(), String> {
        self.forget_drag(|dragged| dragged.client == client);
        let layout_changed = self.scene.remove_client(client);
        self.settle(layout_changed)
    }

    pub fn command(&mut self, command: Command) -> Result<(), String> {
        self.scene.command(command);
        // A tiling command is what a user reaches for when the screen looks
        // wrong, so make it the immediate repair for pixels the compositor did
        // not write and its shadow copy therefore cannot see.
        self.framebuffer.resend();
        self.settle(true)
    }

    pub fn launcher(&mut self, action: LauncherAction) -> Result<Option<LaunchRequest>, String> {
        let checkpoint = self.scene.launcher_checkpoint();
        let request = self.scene.launcher(action);
        // Published BEFORE the paint that can fail. The arrangement a drag
        // was put back to is the clients' business and nothing below owes it
        // to them; the rollback restores only the overlay, which no tiling
        // geometry reads, so it cannot invalidate this.
        if self.cancel_drag_under_overlay() && self.refresh_layout() {
            self.publish_layout();
        }
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

    /// An overlay going up drops whatever a title band was holding. Cleared
    /// HERE rather than only on the next pointer frame, because an overlay is
    /// opened from the keyboard: press a band, raise the sheet and dismiss it
    /// without moving the mouse, and no modal pointer frame ever happens — so
    /// the release would perform a drop the operator could not see themselves
    /// aiming at.
    ///
    /// Folded into the paint the overlay already owes rather than settling on
    /// its own: a failure here would otherwise be the one path through these
    /// two that returns without restoring what it changed. Answers whether
    /// putting the arrangement back moved the screen, which is a round of
    /// configures the caller owes its clients.
    fn cancel_drag_under_overlay(&mut self) -> bool {
        if !self.scene.modal() {
            return false;
        }
        self.dragging = None;
        self.scene.clear_preview()
    }

    /// Put the window back down where it was picked up and end the drag.
    fn cancel_drag(&mut self) -> Result<(), String> {
        self.dragging = None;
        if self.scene.clear_preview() {
            self.settle(true)?;
        }
        Ok(())
    }

    /// Take a new status line. Repaints only when the TEXT changed, so a
    /// sampler that ticks faster than the line does costs nothing — and the
    /// clock's seconds are what make it change at all.
    pub fn set_status(&mut self, status: String) -> Result<(), String> {
        let Some(previous) = self.scene.set_status(status) else {
            return Ok(());
        };
        if let Err(error) = self.repaint() {
            self.scene.set_status(previous);
            return Err(error);
        }
        Ok(())
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
        if self.cancel_drag_under_overlay() && self.refresh_layout() {
            self.publish_layout();
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

    #[cfg(test)]
    pub fn clear_repaint_failure(&mut self) {
        self.framebuffer.clear_paint_failure();
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
        // What a claim looks like; the pointer model says when one holds,
        // since the grab a claim must not steal moves WITHIN a report. Gated
        // on picking something up, so a press that could move nothing is
        // never taken from its client. The release needs no such care: the
        // model sends one only for a press it accepted.
        let alt = self.keyboard.held().depressed & MOD_ALT != 0
            && self
                .scene
                .draggable_at_pointer(self.framebuffer.width, self.framebuffer.height)
                .is_some();
        let result = self.pointer.frame(time, hover, grab, buttons, |input| {
            alt && input.button == POINTER_BUTTON_LEFT
        })?;
        let alt_press = result.claimed;
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
        self.drag(modal, dx != 0 || dy != 0, alt_press, buttons)?;
        Ok(())
    }

    /// The drag. A press picks a window up and focuses it; from the first
    /// motion off that window the screen shows the drop itself, and the
    /// release keeps it. Two presses start one: a BARE press on a title band,
    /// and an ALT press anywhere on a window — the band is the handle that
    /// exists without a modifier, and the modifier is what makes the whole
    /// window one.
    ///
    /// Entirely outside the pointer model, and it has to be: a band belongs to
    /// no client, so a press on one establishes no grab and delivers nothing.
    /// That is the same seam that makes a band's click reach no client, used
    /// rather than worked around, and an Alt press is withheld to reach it.
    /// A modal overlay owns every button while it is up, so nothing is picked
    /// up under one — and anything already held is dropped, since the operator
    /// can no longer see where it would land.
    fn drag(
        &mut self,
        modal: bool,
        moved: bool,
        alt_press: bool,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String> {
        if modal {
            return self.cancel_drag();
        }
        let (width, height) = (self.framebuffer.width, self.framebuffer.height);
        // One pass, in the order the transitions happened. A frame can carry
        // several — evdev keeps every transition up to its SYN_REPORT — and a
        // release followed by a press is a window dropped and the next one
        // picked up in one batch. Handling all the presses first would let the
        // new drag consume the old one's release: the old drop lost and the
        // new drag ended where it started.
        for input in buttons {
            if input.button != POINTER_BUTTON_LEFT {
                continue;
            }
            match input.state {
                PointerButtonState::Pressed => {
                    // An ALT press takes the whole window; it is the press the
                    // model reported claiming, so no second answer exists to
                    // disagree with. A bare one takes the band, and only with
                    // no grab held: such a press was DELIVERED to the grabbing
                    // surface, so taking it would make one button both the
                    // window's and the compositor's.
                    let picked = if alt_press {
                        self.scene.draggable_at_pointer(width, height)
                    } else if self.pointer.grab_surface().is_some() {
                        continue;
                    } else {
                        self.scene.band_at_pointer(width, height)
                    };
                    // A press that picks nothing up ENDS whatever was live: a
                    // picture with no drag to commit or clear it would strand
                    // the screen on an arrangement the layout never took.
                    match picked {
                        Some(key) => {
                            self.dragging = Some(Drag {
                                key,
                                held_by_alt: alt_press,
                            });
                            self.focus_surface(key)?;
                        }
                        None => self.cancel_drag()?,
                    }
                }
                PointerButtonState::Released => {
                    let Some(Drag { key: dragged, .. }) = self.dragging.take() else {
                        continue;
                    };
                    // The reader coalesces a batch, so the motion that chose
                    // the drop and the release that takes it commonly arrive
                    // together and the picture is a frame behind the pointer.
                    // Brought up to date only when this frame MOVED: on a
                    // still pointer there is nothing new to account for, and
                    // computing one anyway would commit an answer the
                    // operator never saw — which is exactly a release that
                    // follows a window mapping the preview away.
                    let settled = if moved && self.scene.preview_drop(dragged, width, height) {
                        self.settle(true)
                    } else {
                        Ok(())
                    };
                    // Nothing is decided here: the drop went into the picture
                    // when the pointer reached it, so the release only KEEPS
                    // it, owing neither a paint nor a round of configures.
                    // Run whatever the settle did — the drag is already taken,
                    // so skipping it would strand the picture with nothing
                    // able to commit or clear it.
                    self.scene.commit_preview();
                    settled?;
                }
            }
        }
        if let Some(dragged) = self.dragging.as_ref().map(|drag| drag.key) {
            if self.scene.preview_drop(dragged, width, height) {
                self.settle(true)?;
            }
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
        self.settle(true)
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
        // The modifier is half of what holds an Alt gesture open, so letting
        // it go abandons the drag and the picture with it — which is the
        // whole of the revert, the layout underneath never having moved.
        if self
            .dragging
            .as_ref()
            .is_some_and(|drag| drag.held_by_alt && modifiers.depressed & MOD_ALT == 0)
        {
            self.cancel_drag()?;
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

    /// Infallible, and typed so: an overlay publishes before its own paint, so
    /// a `?` here would return with the overlay changed, unpainted and NOT
    /// rolled back — the hazard folding it into the paint exists to avoid.
    fn refresh_layout(&mut self) -> bool {
        let next: BTreeMap<SurfaceKey, ViewLayout> = self
            .scene
            .views(self.framebuffer.width, self.framebuffer.height)
            .into_iter()
            .map(|view| (view.key, view))
            .collect();
        if self.layout.as_ref() == &next {
            return false;
        }
        self.layout = Arc::new(next);
        true
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
    use crate::keyboard::MOD_ALT as TEST_MOD_ALT;
    use crate::keyboard::{KeyState, KeyboardEvent, MOD_SHIFT};
    use crate::layout::{Axis, Direction};
    use crate::pointer::{PointerButtonState, PointerEvent};
    use crate::scene::SHM_XRGB8888;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::TryRecvError;

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
    fn dragging_a_title_band_drops_the_window_beside_where_it_was_released() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-band-drag-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        // Tall enough that THREE tiles stacked in a column each keep a real
        // client area: `least_output_height` reserves rows for one, and with
        // three the band eats the whole tile and every client rect is empty —
        // which a drop onto "the top half of a client" then silently answers
        // from a band instead.
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        // `H[1, V[2, 3]]`: a window beside a column of two.
        let order = |runtime: &Runtime| {
            runtime
                .scene
                .tiled_placements(240, height)
                .iter()
                .map(|placement| placement.key.object)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&runtime), [1, 2, 3]);
        // Guards the arrangement every drop below actually targets — a
        // column of THREE — rather than the two-tall one it starts as.
        let clients_are_real = |runtime: &Runtime| {
            runtime
                .scene
                .tiled_placements(240, height)
                .iter()
                .all(|placement| placement.rect.height > 0)
        };
        assert!(
            clients_are_real(&runtime),
            "the output is too short for these tiles to have client areas"
        );

        let band = |runtime: &Runtime, object: u32| {
            let placements = runtime.scene.tiled_placements(240, height);
            let at = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(at).unwrap()
        };
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
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons).unwrap()
        };

        // Pick 1 up by its band and drop it on the TOP half of 3's client.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(2)]);
        assert_eq!(runtime.keyboard_snapshot().focus, keys.first().copied());
        // Captured AFTER the press: picking the window up focuses it, and
        // focus publishes a layout of its own, so a snapshot from before it
        // would be moved by the press rather than by the drop.
        let published = runtime.layout_snapshot();
        let target = band(&runtime, 3).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(3)]);
        assert_eq!(order(&runtime), [2, 1, 3], "the drop did not land above 3");
        assert!(clients_are_real(&runtime));
        // A drop that rearranges the tree owes the CLIENTS a round of
        // configures: a move nobody is told about is a move that did not
        // happen as far as they are concerned. Compared against the snapshot
        // taken before the drop, which is a different source from the tree.
        assert_ne!(
            runtime.layout_snapshot(),
            published,
            "the drop published no new layout"
        );
        runtime.scene.layout().check_invariants().unwrap();

        // Bottom half of the same window puts it below instead.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(4)]);
        let target = band(&runtime, 3).rect;
        goto(
            &mut runtime,
            target.x + 2,
            target.y + target.height - 2,
            &[release(5)],
        );
        assert_eq!(order(&runtime), [2, 3, 1], "the drop did not land below 3");

        // A release over the DESKTOP cancels rather than moving to nowhere.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(6)]);
        goto(&mut runtime, 0, height - 1, &[release(7)]);
        assert_eq!(order(&runtime), [2, 3, 1]);

        // And a press on a CLIENT area picks nothing up, so the release that
        // follows is not a drop: only the band is a handle.
        let source = band(&runtime, 1).rect;
        goto(&mut runtime, source.x + 2, source.y + 2, &[press(8)]);
        let target = band(&runtime, 2).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(9)]);
        assert_eq!(order(&runtime), [2, 3, 1], "a client press began a drag");

        // An overlay going up mid-drag DROPS what was held: it covers the
        // screen the operator was aiming at, so there is no longer anywhere
        // they can be said to have meant.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(10)]);
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        let target = band(&runtime, 2).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(11)]);
        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        assert_eq!(order(&runtime), [2, 3, 1], "a drag survived an overlay");

        // An overlay raised and DISMISSED from the keyboard, with the button
        // still held and the mouse never moved: no modal pointer frame ever
        // happens, so a guard that only ran on one would let this drop.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(14)]);
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        let target = band(&runtime, 2).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(15)]);
        assert_eq!(
            order(&runtime),
            [2, 3, 1],
            "a drag outlived an overlay raised from the keyboard"
        );

        // A client already holding a grab owns every button. A RIGHT press on
        // a client area establishes one; the LEFT press that follows is routed
        // to that client, so picking a band up with it would make one button
        // both the window's and the compositor's — and move focus mid-grab.
        let right = |time, state| PointerButtonInput {
            time,
            button: 273,
            state,
        };
        let held = band(&runtime, 2).rect;
        goto(
            &mut runtime,
            held.x + 2,
            held.y + 2,
            &[right(18, PointerButtonState::Pressed)],
        );
        assert!(runtime.pointer.grab_surface().is_some(), "no grab was held");
        let focused = runtime.keyboard_snapshot().focus;
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(19)]);
        assert_eq!(runtime.keyboard_snapshot().focus, focused, "focus moved");
        let target = band(&runtime, 3).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(20)]);
        assert_eq!(order(&runtime), [2, 3, 1], "a grabbed button began a drag");
        goto(
            &mut runtime,
            target.x + 2,
            target.y + 2,
            &[right(21, PointerButtonState::Released)],
        );

        // A whole CLIENT going away is the second forget path and a second
        // call site, so it is asserted rather than taken on trust. Its window
        // joins as a fourth, is picked up, and the client is destroyed.
        let guest = SurfaceKey {
            client: 2,
            object: 1,
        };
        runtime.commit(guest, surface([2, 2, 2, 0])).unwrap();
        let handle = runtime
            .scene
            .tiled_placements(240, height)
            .iter()
            .position(|placement| placement.key == guest)
            .and_then(|at| runtime.scene.tiled_placements(240, height).get(at).copied())
            .unwrap()
            .band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(24)]);
        runtime.remove_client(2).unwrap();
        // Reconnecting reuses the same key, which is the whole hazard: a
        // forget that only made the drag name nothing would be invisible,
        // since a drop of a window no longer in the tree is refused anyway.
        runtime.commit(guest, surface([3, 3, 3, 0])).unwrap();
        let survivors = order(&runtime);
        let target = band(&runtime, 3).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(25)]);
        assert_eq!(
            order(&runtime),
            survivors,
            "a destroyed client's drag outlived it"
        );
        runtime.remove_client(2).unwrap();

        // A window that GOES AWAY mid-drag takes the drag with it. Object ids
        // are recycled per client, so a stale one can come to name a different
        // window and the release would move one nobody picked up.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(22)]);
        runtime.remove(*keys.first().unwrap()).unwrap();
        runtime
            .commit(*keys.first().unwrap(), surface([9, 9, 9, 0]))
            .unwrap();
        let reopened = order(&runtime);
        let target = band(&runtime, 3).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(23)]);
        assert_eq!(
            order(&runtime),
            reopened,
            "a recycled id inherited a drag it never began"
        );

        // UNMAPPING is the third way a window leaves, and unlike the two
        // above it can be undone: the same surface maps again and is the same
        // window, so a drag that survived would come back to life and move it
        // under a button pressed before it ever vanished.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(26)]);
        runtime.unmap(*keys.first().unwrap()).unwrap();
        runtime
            .commit(*keys.first().unwrap(), surface([6, 6, 6, 0]))
            .unwrap();
        let remapped = order(&runtime);
        let target = band(&runtime, 3).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(27)]);
        assert_eq!(
            order(&runtime),
            remapped,
            "a remapped window inherited the drag it was unmapped under"
        );

        // The LAUNCHER is the other overlay and a second call site, so it
        // gets the same assertion rather than being taken on trust.
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(16)]);
        runtime.launcher(LauncherAction::Open).unwrap();
        assert!(runtime.launcher_visible());
        runtime.launcher(LauncherAction::Close).unwrap();
        let target = band(&runtime, 2).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(17)]);
        assert_eq!(
            order(&runtime),
            [2, 3, 1],
            "a drag outlived the launcher opening over it"
        );

        // And nothing is picked up while one is up, so the release after it
        // closes is not a drop either.
        assert!(runtime.help(HelpAction::Toggle).unwrap());
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(12)]);
        assert!(!runtime.help(HelpAction::Toggle).unwrap());
        let target = band(&runtime, 2).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[release(13)]);
        assert_eq!(order(&runtime), [2, 3, 1], "an overlay let a drag begin");
        runtime.scene.layout().check_invariants().unwrap();
    }

    #[test]
    fn a_drag_shows_its_drop_before_the_button_is_released() {
        // The drag as an operator performs it, in the frames it actually
        // arrives in: press, then MOTION, then release. The old indicator was
        // computed at the release, so nothing on screen between those frames
        // said where the window would land. Now the motion frame IS the
        // landing, and the release keeps it.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-drag-preview-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        let tiles = |runtime: &Runtime| runtime.scene.tiled_placements(240, height);
        let order = |runtime: &Runtime| {
            tiles(runtime)
                .iter()
                .map(|placement| placement.key.object)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&runtime), [1, 2, 3]);
        let band = |runtime: &Runtime, object: u32| {
            let placements = tiles(runtime);
            let at = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(at).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons).unwrap()
        };
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

        // Picking 1 up moves nothing yet: the pointer has not left its band.
        // Geometry rather than whole placements, since the press legitimately
        // takes focus and focus is one of their fields.
        let geometry = |runtime: &Runtime| {
            tiles(runtime)
                .iter()
                .map(|placement| (placement.key.object, placement.rect, placement.band))
                .collect::<Vec<_>>()
        };
        let undragged = geometry(&runtime);
        let handle = band(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(2)]);
        assert_eq!(
            geometry(&runtime),
            undragged,
            "the press alone moved a window"
        );
        let picked_up = runtime.layout_snapshot();

        // The motion frame, with the button still down and no release in it.
        let target = band(&runtime, 3).rect;
        goto(
            &mut runtime,
            target.x + 2,
            target.y + target.height - 2,
            &[],
        );
        assert_eq!(
            order(&runtime),
            [2, 3, 1],
            "the drop was not on screen before the release"
        );
        let previewed = tiles(&runtime);
        let published = runtime.layout_snapshot();
        // Clients are configured for the preview, or the picture is a lie the
        // moment any of them redraws into it.
        assert_ne!(published, picked_up, "the preview published no layout");
        // And the KEYBOARD is aimed at what the picture says is active. These
        // come from different sources — one the arrangement's own focus, the
        // other the published map's activation — so a preview read off the
        // layout underneath would aim them at different windows.
        assert_eq!(
            runtime.keyboard_snapshot().focus,
            keys.first().copied(),
            "the keyboard and the picture disagree about what is focused"
        );

        // The release. It has nothing left to decide, so it owes no paint and
        // no round of configures — and the map published to clients must come
        // out byte-identical, which a release that recomputed the drop could
        // not promise.
        goto(
            &mut runtime,
            target.x + 2,
            target.y + target.height - 2,
            &[release(3)],
        );
        assert_eq!(
            tiles(&runtime),
            previewed,
            "the release moved what the preview had promised"
        );
        assert_eq!(runtime.layout_snapshot(), published);
        runtime.scene.layout().check_invariants().unwrap();

        // Aiming somewhere and then leaving every tile puts the arrangement
        // back, so a cancelled drag looks like one BEFORE the button is let
        // go rather than only after.
        let settled = geometry(&runtime);
        let handle = band(&runtime, 2).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(4)]);
        let target = band(&runtime, 1).rect;
        goto(&mut runtime, target.x + 2, target.y + 2, &[]);
        assert_ne!(geometry(&runtime), settled, "the drag previewed nothing");
        goto(&mut runtime, 0, height - 1, &[]);
        assert_eq!(
            geometry(&runtime),
            settled,
            "leaving every tile did not put the arrangement back"
        );
        goto(&mut runtime, 0, height - 1, &[release(5)]);
        assert_eq!(geometry(&runtime), settled);
        runtime.scene.layout().check_invariants().unwrap();

        // A frame carrying a RELEASE and then a PRESS — one window dropped
        // and the next picked up in a single batch, which evdev keeps whole
        // up to its SYN_REPORT. Handled out of order, the new drag would
        // consume the old one's release: the drop lost and the drag ended
        // where it began.
        let start = order(&runtime);
        let dragged = *start.first().unwrap();
        let handle = band(&runtime, dragged).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(6)]);
        // The BOTTOM half of the topmost band, so the drop lands after it and
        // the order comes out different from the one it started in.
        let onto = band(&runtime, *start.get(1).unwrap()).band;
        goto(
            &mut runtime,
            onto.x + 2,
            onto.y + onto.height - 2,
            &[release(7), press(8)],
        );
        assert_eq!(
            order(&runtime),
            [*start.get(1).unwrap(), dragged, *start.get(2).unwrap()],
            "the release in a release-then-press frame dropped nothing"
        );
        assert!(
            runtime.dragging.is_some(),
            "the press in a release-then-press frame picked nothing up"
        );
        goto(&mut runtime, 0, height - 1, &[release(9)]);
        runtime.scene.layout().check_invariants().unwrap();

        // A release that arrives with the pointer STILL, after something
        // invalidated the preview, lands nothing. Recomputing one on a still
        // pointer would commit an answer the operator never saw — and the
        // reflow can move the dead zone under them, so it could move a window
        // for a title-bar click that never moved the mouse.
        let settled = geometry(&runtime);
        let handle = band(&runtime, *order(&runtime).first().unwrap()).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(10)]);
        let onto = band(&runtime, *order(&runtime).get(1).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        assert_ne!(geometry(&runtime), settled, "the drag previewed nothing");
        // A mutation that changes NOTHING itself still drops the picture, and
        // owes the clients the round of configures that puts them back: a
        // surface that was never in the layout detaching is the case, and its
        // own answer is "no change".
        let previewed = runtime.layout_snapshot();
        runtime
            .remove(SurfaceKey {
                client: 8,
                object: 8,
            })
            .unwrap();
        assert_eq!(geometry(&runtime), settled, "the picture outlived its base");
        assert_ne!(
            runtime.layout_snapshot(),
            previewed,
            "the clients were left holding previewed geometry"
        );
        goto(&mut runtime, 0, height - 1, &[release(13)]);

        // A press that picks NOTHING up ends whatever was live rather than
        // leaving its picture standing with nothing able to commit or clear
        // it. Not reachable while every release arrives — a second press of a
        // held button is not forwarded — but a batch that overflows its
        // transition limit is reset, and the release in it is what goes.
        let handle = band(&runtime, *order(&runtime).first().unwrap()).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(14)]);
        let onto = band(&runtime, *order(&runtime).get(1).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        assert_ne!(geometry(&runtime), settled, "the drag previewed nothing");
        goto(&mut runtime, 0, height - 1, &[press(15)]);
        assert!(runtime.dragging.is_none(), "the drag outlived a lost press");
        assert_eq!(
            geometry(&runtime),
            settled,
            "a press that picked nothing up stranded the picture"
        );
        goto(&mut runtime, 0, height - 1, &[release(16)]);

        // A window arriving drops the picture too, since it is derived from
        // an arrangement that no longer holds.
        let handle = band(&runtime, *order(&runtime).first().unwrap()).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(12)]);
        let onto = band(&runtime, *order(&runtime).get(1).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        assert_ne!(geometry(&runtime), settled, "the drag previewed nothing");
        let guest = SurfaceKey {
            client: 3,
            object: 1,
        };
        runtime.commit(guest, surface([5, 5, 5, 0])).unwrap();
        let arrived = geometry(&runtime);
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[release(11)]);
        assert_eq!(
            geometry(&runtime),
            arrived,
            "a still release after an invalidated preview moved a window"
        );
        runtime.remove(guest).unwrap();
        runtime.scene.layout().check_invariants().unwrap();
    }

    #[test]
    fn an_alt_press_drags_a_window_and_letting_alt_go_puts_it_back() {
        // The second way to pick a window up: ALT and the left button, from
        // anywhere on it rather than from its title band alone. The band is
        // the handle that exists without a modifier; the modifier is what
        // makes the whole window one. Letting ALT go before the button
        // abandons the gesture — the modifier is half of what holds it open —
        // where letting the BUTTON go completes it.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-alt-drag-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        let tiles = |runtime: &Runtime| runtime.scene.tiled_placements(240, height);
        let order = |runtime: &Runtime| {
            tiles(runtime)
                .iter()
                .map(|placement| placement.key.object)
                .collect::<Vec<_>>()
        };
        let geometry = |runtime: &Runtime| {
            tiles(runtime)
                .iter()
                .map(|placement| (placement.key.object, placement.rect, placement.band))
                .collect::<Vec<_>>()
        };
        let at = |runtime: &Runtime, object: u32| {
            let placements = tiles(runtime);
            let index = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(index).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons).unwrap()
        };
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
        let alt = |runtime: &mut Runtime, down: bool| {
            runtime
                .modifiers(ModifierState {
                    depressed: if down { TEST_MOD_ALT } else { 0 },
                    ..ModifierState::default()
                })
                .unwrap();
        };
        assert_eq!(order(&runtime), [1, 2, 3]);

        // What the CLIENT is told, which is the half no geometry can show. A
        // grab is only established by a press, so it answers for the press
        // alone; that the RELEASE of a withheld press is inert is the pointer
        // model's own rule — it sends one only for a press it delivered — and
        // is asserted here rather than assumed, since the gesture rests on
        // it in place of bookkeeping of its own.
        let subscription = runtime
            .subscribe_input_with_activity(
                1,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(true)),
            )
            .unwrap();
        let (events, events_stop) = subscription.split();
        let buttons_seen = |events: &Receiver<KeyboardDelivery>| {
            let mut seen = Vec::new();
            loop {
                match events.try_recv() {
                    Ok(KeyboardDelivery::Pointer(frame)) => {
                        seen.extend(frame.events.into_iter().filter(|event| {
                            matches!(event, crate::pointer::PointerEvent::Button { .. })
                        }));
                    }
                    Ok(_) => {}
                    // A dead channel drains empty, which is what every
                    // assertion below expects to see, so it is refused rather
                    // than counted as "nothing was delivered".
                    Err(TryRecvError::Disconnected) => panic!("the client's channel closed"),
                    Err(TryRecvError::Empty) => break,
                }
            }
            seen
        };

        // Without ALT, a press on a CLIENT area is the client's: it picks
        // nothing up and DOES establish a grab. That contrast is the point of
        // the modifier, so it is asserted rather than assumed.
        let body = at(&runtime, 1).rect;
        goto(&mut runtime, body.x + 2, body.y + 2, &[press(2)]);
        assert!(runtime.dragging.is_none(), "a bare client press dragged");
        assert!(
            runtime.pointer.grab_surface().is_some(),
            "a bare client press was withheld"
        );
        goto(&mut runtime, body.x + 2, body.y + 2, &[release(3)]);
        assert_eq!(
            buttons_seen(&events).len(),
            2,
            "a bare press and release did not reach the client"
        );

        // With ALT the same press is the COMPOSITOR's. No grab is
        // established, because the client is never told about it.
        alt(&mut runtime, true);
        let settled = geometry(&runtime);
        goto(&mut runtime, body.x + 2, body.y + 2, &[press(4)]);
        assert!(
            runtime.dragging.is_some(),
            "an alt press on a client area picked nothing up"
        );
        assert!(
            runtime.pointer.grab_surface().is_none(),
            "the alt press established a grab"
        );
        assert_eq!(
            buttons_seen(&events),
            [],
            "the alt press reached the client"
        );
        assert_eq!(
            geometry(&runtime),
            settled,
            "the press alone moved a window"
        );

        // It drags like any other: the picture is the drop before the button
        // comes up, and the release keeps it.
        let onto = at(&runtime, 3).rect;
        goto(&mut runtime, onto.x + 2, onto.y + onto.height - 2, &[]);
        assert_eq!(order(&runtime), [2, 3, 1], "the alt drag previewed nothing");
        let previewed = geometry(&runtime);
        goto(
            &mut runtime,
            onto.x + 2,
            onto.y + onto.height - 2,
            &[release(5)],
        );
        assert_eq!(
            geometry(&runtime),
            previewed,
            "the release moved the picture"
        );
        assert!(runtime.dragging.is_none());
        assert_eq!(
            buttons_seen(&events),
            [],
            "the release of a withheld press reached the client"
        );
        runtime.scene.layout().check_invariants().unwrap();

        // Letting ALT go first puts the window back, and the release that
        // follows still reaches no client: the press it belongs to was never
        // delivered, whatever became of the drag in between.
        let settled = geometry(&runtime);
        let body = at(&runtime, *order(&runtime).first().unwrap()).rect;
        goto(&mut runtime, body.x + 2, body.y + 2, &[press(6)]);
        let onto = at(&runtime, *order(&runtime).get(2).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        assert_ne!(
            geometry(&runtime),
            settled,
            "the alt drag previewed nothing"
        );
        alt(&mut runtime, false);
        assert_eq!(
            geometry(&runtime),
            settled,
            "letting alt go did not put the window back"
        );
        assert!(runtime.dragging.is_none(), "the alt drag outlived alt");

        // Moving on with the button still down previews nothing, since the
        // gesture is over rather than merely paused.
        let elsewhere = at(&runtime, *order(&runtime).get(1).unwrap()).rect;
        goto(&mut runtime, elsewhere.x + 2, elsewhere.y + 2, &[]);
        assert_eq!(geometry(&runtime), settled, "an abandoned drag came back");
        goto(
            &mut runtime,
            elsewhere.x + 2,
            elsewhere.y + 2,
            &[release(7)],
        );
        assert_eq!(geometry(&runtime), settled, "the release moved a window");
        assert_eq!(
            buttons_seen(&events),
            [],
            "the release of a press abandoned with alt reached the client"
        );
        runtime.scene.layout().check_invariants().unwrap();

        // A band pressed UNDER alt is an alt drag like any other, so letting
        // the modifier go abandons it. Which gesture a press begins is
        // decided by whether alt was down AT THE PRESS.
        alt(&mut runtime, true);
        let settled = geometry(&runtime);
        let handle = at(&runtime, *order(&runtime).first().unwrap()).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(8)]);
        let onto = at(&runtime, *order(&runtime).get(2).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        assert_ne!(
            geometry(&runtime),
            settled,
            "the band drag previewed nothing"
        );
        alt(&mut runtime, false);
        assert_eq!(
            geometry(&runtime),
            settled,
            "a band pressed under alt was not held by it"
        );
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[release(9)]);

        // And one pressed WITHOUT it is not: the modifier going up and down
        // during that drag changes nothing, and the drop lands exactly the
        // picture that was on screen.
        let handle = at(&runtime, *order(&runtime).first().unwrap()).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(22)]);
        alt(&mut runtime, true);
        alt(&mut runtime, false);
        let onto = at(&runtime, *order(&runtime).get(2).unwrap()).rect;
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[]);
        let previewed = geometry(&runtime);
        assert_ne!(previewed, settled, "a band drag was abandoned with alt");
        goto(&mut runtime, onto.x + 2, onto.y + 2, &[release(23)]);
        assert_eq!(geometry(&runtime), previewed, "the drop moved the picture");
        runtime.scene.layout().check_invariants().unwrap();

        // A client already holding a grab keeps its button, ALT or not. Such
        // a press IS delivered to the grabbing surface, so taking it for a
        // gesture would make one button both the window's and the
        // compositor's — and the filter and the drag would disagree about
        // which of them owned it.
        let right = |time, state| PointerButtonInput {
            time,
            button: 273,
            state,
        };
        let body = at(&runtime, *order(&runtime).first().unwrap()).rect;
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[right(10, PointerButtonState::Pressed)],
        );
        assert!(runtime.pointer.grab_surface().is_some(), "no grab was held");
        let held = geometry(&runtime);
        buttons_seen(&events);
        alt(&mut runtime, true);
        goto(&mut runtime, body.x + 2, body.y + 2, &[press(11)]);
        assert!(
            runtime.dragging.is_none(),
            "an alt press took a button its client was holding"
        );
        assert_eq!(
            buttons_seen(&events).len(),
            1,
            "the alt press was withheld from the client holding the button"
        );
        assert_eq!(geometry(&runtime), held);
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[release(12), right(13, PointerButtonState::Released)],
        );

        // The same rule when the grab is made INSIDE the report: evdev keeps
        // every transition up to its SYN_REPORT, so the right press that
        // establishes it and the left press beside it arrive together. An
        // answer computed once for the report, before either was processed,
        // would take a button the client had just grabbed.
        buttons_seen(&events);
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[right(14, PointerButtonState::Pressed), press(15)],
        );
        assert!(
            runtime.dragging.is_none(),
            "an alt press took a button grabbed beside it in one report"
        );
        assert_eq!(
            buttons_seen(&events).len(),
            2,
            "the left press was withheld from a client that had just grabbed"
        );
        assert_eq!(geometry(&runtime), held);
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[release(16), right(17, PointerButtonState::Released)],
        );

        // And the other direction, which the same one answer gets wrong the
        // other way: the grab ENDS earlier in the report, so the left press
        // beside it is the compositor's after all.
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[right(18, PointerButtonState::Pressed)],
        );
        assert!(runtime.pointer.grab_surface().is_some(), "no grab was held");
        buttons_seen(&events);
        goto(
            &mut runtime,
            body.x + 2,
            body.y + 2,
            &[right(19, PointerButtonState::Released), press(20)],
        );
        assert!(
            runtime.dragging.is_some(),
            "an alt press was refused for a grab that had already ended"
        );
        assert_eq!(
            buttons_seen(&events).len(),
            1,
            "the claimed press reached the client"
        );
        assert_eq!(geometry(&runtime), held, "the press alone moved a window");
        goto(&mut runtime, body.x + 2, body.y + 2, &[release(21)]);
        alt(&mut runtime, false);
        runtime.scene.layout().check_invariants().unwrap();

        // A press that could move NOTHING is left to its client. Under
        // fullscreen the one placement covers the output, so every alt click
        // anywhere in that client would be taken and none of them could ever
        // land — the whole application silently losing the modifier.
        alt(&mut runtime, true);
        runtime.command(Command::ToggleFullscreen).unwrap();
        let whole = at(&runtime, *order(&runtime).first().unwrap()).rect;
        let full = geometry(&runtime);
        buttons_seen(&events);
        goto(&mut runtime, whole.x + 2, whole.y + 2, &[press(24)]);
        assert!(
            runtime.dragging.is_none(),
            "an alt press picked up a fullscreen window"
        );
        assert_eq!(
            buttons_seen(&events).len(),
            1,
            "a fullscreen client lost its alt click"
        );
        goto(&mut runtime, whole.x + 2, whole.y + 2, &[release(25)]);
        assert_eq!(geometry(&runtime), full);
        runtime.command(Command::ToggleFullscreen).unwrap();

        // A LONE window is the other case: there is nothing to land beside,
        // so the gesture could not move it either.
        runtime.remove(*keys.get(1).unwrap()).unwrap();
        runtime.remove(*keys.get(2).unwrap()).unwrap();
        let alone = at(&runtime, *order(&runtime).first().unwrap()).rect;
        buttons_seen(&events);
        goto(&mut runtime, alone.x + 2, alone.y + 2, &[press(26)]);
        assert!(
            runtime.dragging.is_none(),
            "an alt press picked up the only window"
        );
        assert_eq!(
            buttons_seen(&events).len(),
            1,
            "the only window lost its alt click"
        );
        goto(&mut runtime, alone.x + 2, alone.y + 2, &[release(27)]);
        alt(&mut runtime, false);
        runtime.scene.layout().check_invariants().unwrap();
        events_stop.stop();
        runtime.unsubscribe_keyboard(1);
    }

    #[test]
    fn a_failed_paint_owes_the_screen_but_never_the_clients_their_geometry() {
        // The screen is the one thing a failed paint owes: `pending_paint`
        // holds that debt and a later flush pays it. NOTHING owes the clients
        // their configures, so a settle that gave up at the paint lost them
        // outright — every client would go on drawing at the size it was last
        // told, which the compositor clips into a rectangle of the wrong
        // shape. EVERY caller is driven, not a representative one: they share
        // `settle` today and the claim is about each of them, so a site that
        // stopped routing through it would be caught here rather than by
        // whichever of them a single case happened to use.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-failed-paint-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let key = |client, object| SurfaceKey { client, object };
        for object in 1..=3 {
            runtime
                .commit(key(1, object), surface([1, 2, 3, 0]))
                .unwrap();
        }
        // A second client, so the `remove_client` case below leaves a survivor
        // to be owed anything at all.
        runtime.commit(key(2, 1), surface([4, 5, 6, 0])).unwrap();
        let subscription = runtime.subscribe(1).unwrap();
        let (woken, stop) = subscription.split();

        // The map changing proves the rebuild ran; only a WAKE proves anyone
        // was told to go and read it, which is the half that reaches a client.
        // The baseline is taken INSIDE the macro, after whatever set the case
        // up, so nothing that ran between two cases can satisfy it.
        macro_rules! under_failed_paint {
            ($what:expr, $op:expr) => {{
                while woken.try_recv().is_ok() {}
                let before = runtime.layout_snapshot();
                runtime.fail_next_repaint();
                let outcome: Result<(), String> = $op;
                assert!(
                    outcome.is_err(),
                    "{}: the paint was supposed to fail",
                    $what
                );
                assert!(
                    runtime.paint_pending(),
                    "{}: the screen was not owed",
                    $what
                );
                assert_ne!(
                    runtime.layout_snapshot(),
                    before,
                    "{}: a failed paint cost the clients their configures",
                    $what
                );
                assert!(
                    woken.try_recv().is_ok(),
                    "{}: the clients were never woken to read the new geometry",
                    $what
                );
                // The screen still gets there, one flush later.
                runtime.clear_repaint_failure();
                runtime.flush_paint().unwrap();
                assert!(
                    !runtime.paint_pending(),
                    "{}: the screen stayed owed",
                    $what
                );
            }};
        }
        let at = |runtime: &Runtime, client, object| {
            let placements = runtime.scene.tiled_placements(240, height);
            let index = placements
                .iter()
                .position(|placement| placement.key == key(client, object))
                .unwrap();
            *placements.get(index).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons)
        };
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

        // A new window mapping, which is the path taken on EVERY client frame.
        // Both entry points, since they settle separately: the one clients
        // really arrive through, and the shorthand the tests above use.
        under_failed_paint!(
            "commit_with_input_region",
            runtime.commit_with_input_region(key(1, 4), surface([7, 8, 9, 0]), None)
        );
        under_failed_paint!("commit", runtime.commit(key(1, 5), surface([9, 9, 9, 0])));
        // A tiling command. `Move` rather than `SetSplit`, which only arms the
        // next split and so moves no tile at all.
        under_failed_paint!("command", runtime.command(Command::Move(Direction::Down)));
        // Click to focus, which republishes the activation every client reads.
        // A press on a client AREA, so it establishes a grab and takes focus
        // without the title band picking the window up — and on a window that
        // is not focused ALREADY, or `focus_key` answers false and settles
        // nothing.
        let focused = runtime.scene.focused();
        let mut elsewhere = None;
        for object in 1..=4 {
            if Some(key(1, object)) != focused {
                elsewhere = Some(at(&runtime, 1, object).rect);
                break;
            }
        }
        let elsewhere = elsewhere.unwrap();
        // Near the tile's ORIGIN, not its centre: a tile is often taller than
        // the 100x100 buffer in it, and a press off the buffer routes to no
        // surface, establishes no grab and so focuses nothing.
        under_failed_paint!(
            "focus_surface",
            goto(&mut runtime, elsewhere.x + 4, elsewhere.y + 4, &[press(2)])
        );
        // Let the button go, so the grab that press established does not ride
        // into the cases below.
        goto(&mut runtime, 0, 0, &[release(3)]).unwrap();
        // A surface unmapping, and one being destroyed outright.
        under_failed_paint!("unmap", runtime.unmap(key(1, 4)));
        under_failed_paint!("remove", runtime.remove(key(1, 3)));
        // A whole client leaving, which is the path a crash takes: the
        // survivors are owed their new size whatever the screen did.
        under_failed_paint!("remove_client", runtime.remove_client(1));

        stop.stop();
        runtime.unsubscribe(1);
    }

    #[test]
    fn a_failed_paint_keeps_the_clients_in_step_with_a_drag() {
        // The drag's own three settles: the motion that puts a picture up, the
        // release that keeps it, and the Alt release that takes it away again.
        // Each publishes what the screen is showing, so a failed paint must not
        // cost it — the clients would then be configured for an arrangement
        // nobody is looking at, in one direction or the other.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-failed-drag-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        let at = |runtime: &Runtime, object: u32| {
            let placements = runtime.scene.tiled_placements(240, height);
            let index = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(index).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons)
        };
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
        let alt = |runtime: &mut Runtime, down: bool| {
            runtime.modifiers(ModifierState {
                depressed: if down { TEST_MOD_ALT } else { 0 },
                ..ModifierState::default()
            })
        };
        // The GEOMETRY alone: picking a window up focuses it, so the activation
        // flag legitimately differs across a gesture.
        let published = |runtime: &Runtime| {
            runtime
                .layout_snapshot()
                .values()
                .map(|view| (view.key, view.rect))
                .collect::<Vec<_>>()
        };

        // The MOTION's settle: pick 1 up by its band and move onto 3 in a frame
        // of its own, so the picture goes up with no release to promote it.
        let handle = at(&runtime, 1).band;
        goto(&mut runtime, handle.x + 2, handle.y + 2, &[press(2)]).unwrap();
        let settled = published(&runtime);
        let target = at(&runtime, 3).rect;
        runtime.fail_next_repaint();
        assert!(
            goto(
                &mut runtime,
                target.x + 2,
                target.y + target.height - 2,
                &[]
            )
            .is_err(),
            "the paint was supposed to fail"
        );
        let previewed = published(&runtime);
        assert_ne!(
            previewed, settled,
            "a failed paint left the clients on the arrangement the drag had left"
        );
        runtime.clear_repaint_failure();
        runtime.flush_paint().unwrap();

        // The RELEASE's settle. Its frame must carry a motion that lands
        // SOMEWHERE ELSE — a pointer moving inside one half re-derives the same
        // drop, which is deliberately no repaint and so no settle to fail.
        runtime.fail_next_repaint();
        assert!(
            goto(&mut runtime, target.x + 2, target.y + 2, &[release(3)]).is_err(),
            "the paint was supposed to fail"
        );
        let dropped = published(&runtime);
        assert_ne!(
            dropped, previewed,
            "a failed paint cost the drop its own configures"
        );
        assert!(!runtime.scene.clear_preview(), "the drop was stranded");
        runtime.clear_repaint_failure();
        runtime.flush_paint().unwrap();

        // And `cancel_drag`, which is the Alt gesture's revert: the picture
        // comes down and the clients are owed the arrangement underneath it.
        alt(&mut runtime, true).unwrap();
        let before = published(&runtime);
        let body = at(&runtime, 2).rect;
        goto(
            &mut runtime,
            body.x + body.width / 2,
            body.y + body.height / 2,
            &[press(4)],
        )
        .unwrap();
        let away = at(&runtime, 1).rect;
        goto(&mut runtime, away.x + 2, away.y + away.height - 2, &[]).unwrap();
        assert_ne!(
            published(&runtime),
            before,
            "the Alt drag previewed nothing"
        );
        runtime.fail_next_repaint();
        assert!(alt(&mut runtime, false).is_err(), "the paint was to fail");
        assert_eq!(
            published(&runtime),
            before,
            "a failed paint left the clients on a reverted preview"
        );
        runtime.clear_repaint_failure();
        runtime.flush_paint().unwrap();
        runtime.scene.layout().check_invariants().unwrap();
    }

    #[test]
    fn a_failed_paint_does_not_strand_a_drop_that_was_already_drawn() {
        // The release takes the drag before it settles, so a settle that gave
        // up on a failed paint left the picture standing with nothing able to
        // commit or clear it — the screen and the clients holding an
        // arrangement the layout never took, and no drag left to promote it.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-failed-drop-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        let order = |runtime: &Runtime| {
            runtime
                .scene
                .tiled_placements(240, height)
                .iter()
                .map(|placement| placement.key.object)
                .collect::<Vec<_>>()
        };
        let at = |runtime: &Runtime, object: u32| {
            let placements = runtime.scene.tiled_placements(240, height);
            let index = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(index).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons)
        };
        assert_eq!(order(&runtime), [1, 2, 3]);

        // Pick 1 up and release it over 3 with the motion in the SAME frame,
        // which is what makes the release re-derive the picture and settle.
        let handle = at(&runtime, 1).band;
        goto(
            &mut runtime,
            handle.x + 2,
            handle.y + 2,
            &[PointerButtonInput {
                time: 2,
                button: 272,
                state: PointerButtonState::Pressed,
            }],
        )
        .unwrap();
        let target = at(&runtime, 3).rect;
        runtime.fail_next_repaint();
        let dropped = goto(
            &mut runtime,
            target.x + 2,
            target.y + target.height - 2,
            &[PointerButtonInput {
                time: 3,
                button: 272,
                state: PointerButtonState::Released,
            }],
        );
        assert!(dropped.is_err(), "the paint was supposed to fail");

        // The drop is the LAYOUT's now, not a picture over it: nothing is left
        // to clear, and the arrangement is what the operator was shown.
        assert!(
            !runtime.scene.clear_preview(),
            "the drop was stranded as a preview"
        );
        assert_eq!(order(&runtime), [2, 3, 1], "the drop was lost");
        assert!(runtime.dragging.is_none());
        runtime.scene.layout().check_invariants().unwrap();
        runtime.clear_repaint_failure();
        runtime.flush_paint().unwrap();
    }

    #[test]
    fn an_overlay_publishes_the_drag_it_cancelled_even_when_its_paint_fails() {
        // The overlay's own rollback restores the overlay and nothing else, so
        // the arrangement a cancelled drag was put back to is published BEFORE
        // the paint that can fail. Published after it, a failed paint would
        // leave every client configured for a preview the screen no longer
        // shows and the layout never took.
        let path = std::env::temp_dir().join(format!(
            "td-runtime-overlay-cancel-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let height = 600;
        let framebuffer = Framebuffer::test_file(&cleanup.0, 240, height, 240 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let keys: Vec<SurfaceKey> = (1..=3)
            .map(|object| SurfaceKey { client: 1, object })
            .collect();
        runtime
            .commit(*keys.first().unwrap(), surface([1, 2, 3, 0]))
            .unwrap();
        runtime
            .commit(*keys.get(1).unwrap(), surface([4, 5, 6, 0]))
            .unwrap();
        runtime.command(Command::SetSplit(Axis::Vertical)).unwrap();
        runtime
            .commit(*keys.get(2).unwrap(), surface([7, 8, 9, 0]))
            .unwrap();
        let at = |runtime: &Runtime, object: u32| {
            let placements = runtime.scene.tiled_placements(240, height);
            let index = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(index).unwrap()
        };
        let goto = |runtime: &mut Runtime, x: usize, y: usize, buttons: &[PointerButtonInput]| {
            let (at_x, at_y) = runtime.scene.pointer_at();
            let (dx, dy) = (
                i32::try_from(x).unwrap() - at_x,
                i32::try_from(y).unwrap() - at_y,
            );
            runtime.pointer_frame(1, dx, dy, buttons).unwrap()
        };
        // The GEOMETRY the clients were configured for, which is what a
        // configure carries. Not the whole view: picking a window up focuses
        // it, so the activation flag legitimately differs across the gesture.
        let published = |runtime: &Runtime| {
            runtime
                .layout_snapshot()
                .values()
                .map(|view| (view.key, view.rect))
                .collect::<Vec<_>>()
        };

        // Both overlays are the same call site twice over, so both are driven.
        for overlay in 0..2 {
            let settled = published(&runtime);
            let handle = at(&runtime, 1).band;
            goto(
                &mut runtime,
                handle.x + 2,
                handle.y + 2,
                &[PointerButtonInput {
                    time: 2,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
            );
            let target = at(&runtime, 3).rect;
            goto(&mut runtime, target.x + 2, target.y + 2, &[]);
            assert_ne!(published(&runtime), settled, "the drag previewed nothing");

            // The overlay goes up, drops the drag — and its paint fails.
            runtime.fail_next_repaint();
            if overlay == 0 {
                assert!(runtime.help(HelpAction::Toggle).is_err());
            } else {
                assert!(runtime.launcher(LauncherAction::Open).is_err());
            }
            assert_eq!(
                published(&runtime),
                settled,
                "a failed overlay paint left the clients on the cancelled preview"
            );
            runtime.clear_repaint_failure();
            runtime.flush_paint().unwrap();

            // Let the button go and put the screen back for the next round.
            goto(
                &mut runtime,
                target.x + 2,
                target.y + 2,
                &[PointerButtonInput {
                    time: 3,
                    button: 272,
                    state: PointerButtonState::Released,
                }],
            );
            assert_eq!(published(&runtime), settled);
            runtime.scene.layout().check_invariants().unwrap();
        }
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
    fn renaming_a_mapped_window_repaints_and_renaming_an_unmapped_one_does_not() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-rename-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            320,
            crate::scene::least_output_height(8),
            320 * 4,
        )
        .unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };

        // Before the first buffer, which is where a client ordinarily sets its
        // title. Nothing is on screen to repaint, and `fail_next_repaint` is
        // what proves no paint was taken rather than that one was harmless.
        runtime.fail_next_repaint();
        runtime.set_title(key, "FIRST".to_string()).unwrap();
        runtime.clear_repaint_failure();

        runtime.commit(key, surface([1, 2, 3, 0])).unwrap();
        let named = fs::read(&cleanup.0).unwrap();

        // Renaming a MAPPED window paints, because its band is on screen and
        // nothing else is going to come along and repaint it: `flush_paint`
        // runs from the input loop, and an idle machine has no input.
        runtime.set_title(key, "SECOND".to_string()).unwrap();
        let renamed = fs::read(&cleanup.0).unwrap();
        assert_ne!(renamed, named, "the new name never reached the screen");

        // The same title again owes nothing — a client that resends its title
        // on every commit is the case this is for.
        runtime.fail_next_repaint();
        runtime.set_title(key, "SECOND".to_string()).unwrap();
        runtime.clear_repaint_failure();
        assert_eq!(fs::read(&cleanup.0).unwrap(), renamed);

        // And a failed paint surfaces rather than being swallowed, leaving the
        // screen owed: the scene holds a name the screen has not shown.
        runtime.fail_next_repaint();
        assert!(runtime.set_title(key, "THIRD".to_string()).is_err());
        assert!(runtime.paint_pending());
        runtime.flush_paint().unwrap();
        assert_ne!(fs::read(&cleanup.0).unwrap(), renamed);
    }

    #[test]
    fn a_status_line_repaints_only_when_its_text_changes() {
        let path = std::env::temp_dir().join(format!(
            "td-runtime-status-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer = Framebuffer::test_file(&cleanup.0, 320, 200, 320 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime.repaint().unwrap();
        let blank = fs::read(&cleanup.0).unwrap();

        runtime.set_status("LOAD 0.42".to_string()).unwrap();
        let painted = fs::read(&cleanup.0).unwrap();
        assert_ne!(painted, blank, "the line was never painted");

        // Same text: the sampler ticks every second and the line only moves
        // when the clock's seconds do, so an unchanged line owes no paint.
        // Armed to fail, and it does not, because it never paints.
        runtime.fail_next_repaint();
        runtime.set_status("LOAD 0.42".to_string()).unwrap();
        assert_eq!(fs::read(&cleanup.0).unwrap(), painted);
        runtime.clear_repaint_failure();

        // Changed text repaints, and a failure surfaces rather than being
        // swallowed — leaving the scene holding the line it DID show, so the
        // very next sample of the same text paints it rather than deciding
        // nothing changed.
        runtime.fail_next_repaint();
        assert!(runtime.set_status("LOAD 9.99".to_string()).is_err());
        assert_eq!(fs::read(&cleanup.0).unwrap(), painted);
        runtime.set_status("LOAD 9.99".to_string()).unwrap();
        assert_ne!(fs::read(&cleanup.0).unwrap(), painted);
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
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            120,
            crate::scene::least_output_height(8),
            120 * 4,
        )
        .unwrap();
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
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            120,
            crate::scene::least_output_height(8),
            120 * 4,
        )
        .unwrap();
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
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            120,
            crate::scene::least_output_height(8),
            120 * 4,
        )
        .unwrap();
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
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            120,
            crate::scene::least_output_height(8),
            120 * 4,
        )
        .unwrap();
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
        let framebuffer = Framebuffer::test_file(
            &cleanup.0,
            120,
            crate::scene::least_output_height(8),
            120 * 4,
        )
        .unwrap();
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
        // Bounded rather than `events.iter()`, which blocks until the channel
        // DISCONNECTS — and the only thing that disconnects it here is the
        // very overflow being asserted. A regression that stopped the pointer
        // landing on a surface queued nothing, overflowed nothing, and so
        // WEDGED `cargo test` instead of failing it. Now the tiling geometry
        // is composed in two places, that is a failure mode worth bounding.
        let mut retained: Vec<RoutedPointerFrame> = Vec::new();
        while let Ok(delivery) = events.recv_timeout(std::time::Duration::from_secs(10)) {
            if let KeyboardDelivery::Pointer(frame) = delivery {
                retained.push(frame);
            }
        }
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
