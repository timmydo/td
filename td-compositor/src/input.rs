use crate::help::HelpAction;
use crate::keyboard::{
    KeyInput, KeyState, ModifierState, MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_NUM, MOD_SHIFT,
};
use crate::launcher::{LaunchBackend, LaunchRequest, LauncherAction};
#[cfg(test)]
use crate::launcher::{LaunchOptions, LaunchProcesses};
use crate::layout::{Command, Direction, Presentation};
use crate::pointer::{
    PointerButtonInput, PointerButtonState, PointerScroll, MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME,
};
use crate::runtime::Runtime;
use crate::scene::Fraction;
use crate::sys;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

const EVENT_SIZE: usize = 24;
/// Records drained per read. A batch is what makes a full-speed pointer cost
/// one paint instead of one per report; the ceiling only bounds the buffer,
/// since a reader that falls further behind simply takes another batch.
const READ_BATCH_RECORDS: usize = 64;
const READ_BATCH_BYTES: usize = EVENT_SIZE * READ_BATCH_RECORDS;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const EV_SW: u16 = 5;
/// The kernel's `EV_CNT`: `capabilities/ev` declares no type at or above it.
const EV_CNT: u16 = 0x20;
const SW_LID: u16 = 0;
/// `SW_LID`'s value for a closed lid.
const LID_CLOSED: i32 = 1;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
/// A wheel reports DETENTS, not distance: one notch is `value` 1 whatever the
/// wheel's physical travel. `REL_WHEEL_HI_RES` (11) and its horizontal twin
/// report the same motion again in units of 120 and are deliberately not read
/// — a device that sends both would scroll twice.
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_BACKSPACE: u16 = 14;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_SLASH: u16 = 53;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;
const KEY_KPENTER: u16 = 96;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const BTN_MOUSE: u16 = 0x110;
/// The first mouse button is the left one.
const BTN_LEFT: u16 = BTN_MOUSE;
const BTN_TASK: u16 = 0x117;
/// A touchpad's contact keys. Outside `BTN_MOUSE..=BTN_TASK` and past
/// `MAX_XKB_EVDEV_KEY`, so neither reaches a client as a button or a key.
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOOL_QUINTTAP: u16 = 0x148;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
const BTN_TOOL_QUADTAP: u16 = 0x14f;
/// Two or more fingers: the emulated position may change finger, so these
/// frames move nothing.
const MULTI_FINGER_TOOLS: &[u16] = &[
    BTN_TOOL_DOUBLETAP,
    BTN_TOOL_TRIPLETAP,
    BTN_TOOL_QUADTAP,
    BTN_TOOL_QUINTTAP,
];
const INPUT_PROP_POINTER: usize = 0;
const INPUT_PROP_DIRECT: usize = 1;
/// Where a node's `properties` and `capabilities/key` bitmaps live.
const SYSFS_INPUT: &str = "/sys/class/input";
/// A sysfs attribute is at most one page; anything longer is not one.
const SYSFS_BITMAP_BYTES: u64 = 4096;
/// The kernel's `HID_MAX_DESCRIPTOR_SIZE`; a longer read is not a descriptor.
const REPORT_DESCRIPTOR_BYTES: u64 = 4096;
/// Levels walked up from an input device to its USB device. Five hub tiers
/// and the controller path sit well inside it.
const SYSFS_ANCESTOR_LIMIT: usize = 32;
/// Entries read from one USB device or interface directory.
const SYSFS_DIRECTORY_ENTRIES: usize = 256;
/// The FIDO Alliance usage page a CTAP HID interface declares.
const FIDO_USAGE_PAGE: u32 = 0xf1d0;
/// Short-item prefixes with the size bits cleared: the global Usage Page,
/// and the local Usage, Usage Minimum and Usage Maximum, whose four-byte
/// (extended) form carries a page in its high half (HID 1.11 §6.2.2.8).
const HID_USAGE_PAGE: u8 = 0x04;
const HID_EXTENDED_USAGES: &[u8] = &[0x08, 0x18, 0x28];
/// Any prefix with these four bits set frames a long item, as the kernel's
/// `fetch_item` reads it; the specification defines only `0xfe`.
const HID_LONG_ITEM: u8 = 0xf0;
/// Touchpad gain with no acceleration curve: one millimetre of finger travel
/// is 16 pixels, so a 60-70 mm pad crosses about 1000 px per stroke while a
/// pixel stays a sixteenth of a millimetre.
const TOUCHPAD_PX_PER_MM: i64 = 16;
/// The same gain for a pad whose driver states no resolution: its X span is
/// taken as 64 mm, and Y is scaled by X's units, assuming square units.
const TOUCHPAD_SPAN_PX: i64 = 1024;
const MAX_XKB_EVDEV_KEY: u16 = 247;
const KEY_EQUAL: u16 = 13;
const KEY_LEFTBRACE: u16 = 26;
const KEY_RIGHTBRACE: u16 = 27;
const KEY_SEMICOLON: u16 = 39;
const KEY_APOSTROPHE: u16 = 40;
const KEY_GRAVE: u16 = 41;
const KEY_BACKSLASH: u16 = 43;
const KEY_COMMA: u16 = 51;
const KEY_DOT: u16 = 52;
const KEY_KPASTERISK: u16 = 55;
const KEY_KP7: u16 = 71;
const KEY_KP8: u16 = 72;
const KEY_KP9: u16 = 73;
const KEY_KPMINUS: u16 = 74;
const KEY_KP4: u16 = 75;
const KEY_KP5: u16 = 76;
const KEY_KP6: u16 = 77;
const KEY_KPPLUS: u16 = 78;
const KEY_KP1: u16 = 79;
const KEY_KP2: u16 = 80;
const KEY_KP3: u16 = 81;
const KEY_KP0: u16 = 82;
const KEY_KPDOT: u16 = 83;
const KEY_KPSLASH: u16 = 98;
/// The PIN field's keymap, `us`: each key's byte, then its byte with Shift.
/// The keypad gives its NumLock character whatever Shift and NumLock say.
/// Every byte is printable ASCII; no other key types.
const PIN_KEYS: &[(u16, u8, u8)] = &[
    (KEY_GRAVE, b'`', b'~'),
    (KEY_1, b'1', b'!'),
    (KEY_2, b'2', b'@'),
    (KEY_3, b'3', b'#'),
    (KEY_4, b'4', b'$'),
    (KEY_5, b'5', b'%'),
    (KEY_6, b'6', b'^'),
    (KEY_7, b'7', b'&'),
    (KEY_8, b'8', b'*'),
    (KEY_9, b'9', b'('),
    (KEY_0, b'0', b')'),
    (KEY_MINUS, b'-', b'_'),
    (KEY_EQUAL, b'=', b'+'),
    (KEY_Q, b'q', b'Q'),
    (KEY_W, b'w', b'W'),
    (KEY_E, b'e', b'E'),
    (KEY_R, b'r', b'R'),
    (KEY_T, b't', b'T'),
    (KEY_Y, b'y', b'Y'),
    (KEY_U, b'u', b'U'),
    (KEY_I, b'i', b'I'),
    (KEY_O, b'o', b'O'),
    (KEY_P, b'p', b'P'),
    (KEY_LEFTBRACE, b'[', b'{'),
    (KEY_RIGHTBRACE, b']', b'}'),
    (KEY_BACKSLASH, b'\\', b'|'),
    (KEY_A, b'a', b'A'),
    (KEY_S, b's', b'S'),
    (KEY_D, b'd', b'D'),
    (KEY_F, b'f', b'F'),
    (KEY_G, b'g', b'G'),
    (KEY_H, b'h', b'H'),
    (KEY_J, b'j', b'J'),
    (KEY_K, b'k', b'K'),
    (KEY_L, b'l', b'L'),
    (KEY_SEMICOLON, b';', b':'),
    (KEY_APOSTROPHE, b'\'', b'"'),
    (KEY_Z, b'z', b'Z'),
    (KEY_X, b'x', b'X'),
    (KEY_C, b'c', b'C'),
    (KEY_V, b'v', b'V'),
    (KEY_B, b'b', b'B'),
    (KEY_N, b'n', b'N'),
    (KEY_M, b'm', b'M'),
    (KEY_COMMA, b',', b'<'),
    (KEY_DOT, b'.', b'>'),
    (KEY_SLASH, b'/', b'?'),
    (KEY_SPACE, b' ', b' '),
    (KEY_KP0, b'0', b'0'),
    (KEY_KP1, b'1', b'1'),
    (KEY_KP2, b'2', b'2'),
    (KEY_KP3, b'3', b'3'),
    (KEY_KP4, b'4', b'4'),
    (KEY_KP5, b'5', b'5'),
    (KEY_KP6, b'6', b'6'),
    (KEY_KP7, b'7', b'7'),
    (KEY_KP8, b'8', b'8'),
    (KEY_KP9, b'9', b'9'),
    (KEY_KPDOT, b'.', b'.'),
    (KEY_KPSLASH, b'/', b'/'),
    (KEY_KPASTERISK, b'*', b'*'),
    (KEY_KPMINUS, b'-', b'-'),
    (KEY_KPPLUS, b'+', b'+'),
];
const KEY_RELEASE: i32 = 0;
const KEY_PRESS: i32 = 1;
const KEY_REPEAT: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    timestamp: u128,
    time: u32,
    kind: u16,
    code: u16,
    value: i32,
}

/// Only the evdev adapter can construct this origin witness.
pub(crate) struct EvdevOrigin {
    _private: (),
}

#[cfg(test)]
pub(crate) fn test_origin() -> EvdevOrigin {
    EvdevOrigin { _private: () }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum AttentionState {
    #[default]
    Closed,
    Open,
    Draining,
}

#[derive(Default)]
struct KeyBindings {
    attention_enabled: bool,
    attention: AttentionState,
    /// Devices whose keys secure attention drains, and whose Escape cancels,
    /// but never selects or confirms with: a security key's own keyboard
    /// (`security_key_keyboard`).
    attention_excluded: BTreeSet<usize>,
    /// One operation per attention lifetime: set once one is selected, or
    /// once a refusal or a failed screen consumed the lifetime.
    secret_selected: bool,
    /// The key-management screen `K` opens within an attention lifetime.
    login_screen: LoginScreen,
    /// That screen's latest paint: a choice on it waits until it is on
    /// glass, as a prompt's consent waits for its receipt.
    login_shown: Option<crate::runtime::NoticePresentation>,
    /// Root's last `1a` answer, which the authority worker keeps current:
    /// an enrolled record's keys in canonical slot order, which removal
    /// digits name, or why there are none.
    login: crate::authority::Login,
    /// Resume detection, checked before each batch is routed; the paired
    /// profile's alone.
    resume: Option<Arc<Resume>>,
    cutoff: Option<u128>,
    /// The screen draining now began closing without a key
    /// (`Seat::close_itself`, `lock_for_suspend`).
    keyless_close: Option<KeylessClose>,
    /// After such a close, the end of the settle window that replaces the
    /// first-report discard for `cutoff`.
    settled: Option<u128>,
    /// The reports that window dropped, until the first past it says how
    /// many (`SETTLED_MARKER`).
    settle_dropped: Option<u32>,
    pressed: BTreeSet<(usize, u16)>,
    forwarded: BTreeSet<(usize, u16)>,
    caps_lock: bool,
    num_lock: bool,
    launcher_open: bool,
    help_open: bool,
    consumed: BTreeSet<(usize, u16)>,
    pointer_pending: BTreeSet<usize>,
    pointer_pressed: BTreeSet<(usize, u16)>,
    pointer_forwarded: BTreeSet<u16>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum LoginScreen {
    #[default]
    Closed,
    Menu,
    Removing(crate::secret_client::Removal),
}

#[derive(Debug, Eq, PartialEq)]
struct KeyDecision {
    attention: Option<bool>,
    secret: Option<crate::secret_client::Selection>,
    /// A screen of the attention lifetime that selects nothing yet.
    notice: Option<crate::attention::Notice>,
    confirm_install: Option<u128>,
    /// An approval-key digit (`approval_digit`) and its evdev time. The
    /// attempt takes it only for its presented elevation prompt.
    approval: Option<(u8, u128)>,
    /// A key for the PIN field and its evdev time. The attempt takes it only
    /// while its field is open and was on glass before the press.
    field: Option<(crate::secret_client::FieldKey, u128)>,
    /// This chord may open the lock surface's unlock, which is its
    /// selection: a fresh Escape and held Control and Alt, each from a
    /// device secure attention reads.
    unlock: bool,
    /// Lock the session: `Super+l`, or the attention menu's `L`, while
    /// root's last `1a` answer is enrolled or unavailable.
    lock: bool,
    draining: bool,
    command: Option<Command>,
    launcher: Option<LauncherAction>,
    help: Option<HelpAction>,
    launch: Option<LaunchRequest>,
    forward: Option<KeyInput>,
    modifiers: Option<ModifierState>,
}

impl KeyBindings {
    #[cfg(test)]
    fn feed(&mut self, event: Event) -> KeyDecision {
        self.feed_device(0, event)
    }

    /// After a self-close, counts each report its settle window dropped
    /// and, at the first report taken past it, says how many, once.
    fn settle_evidence(&mut self, event: Event, accepted: bool) -> Option<String> {
        let dropped = self.settle_dropped.as_mut()?;
        if event.kind != EV_SYN || event.code != SYN_REPORT {
            return None;
        }
        let cutoff = self.cutoff?;
        if !accepted {
            if event.timestamp > cutoff {
                *dropped = dropped.saturating_add(1);
            }
            return None;
        }
        let dropped = self.settle_dropped.take()?;
        Some(format!(
            "{SETTLED_MARKER} cutoff={cutoff} dropped={dropped}"
        ))
    }

    fn feed_device(&mut self, device: usize, event: Event) -> KeyDecision {
        let mut decision = KeyDecision {
            attention: None,
            secret: None,
            notice: None,
            confirm_install: None,
            approval: None,
            field: None,
            unlock: false,
            lock: false,
            draining: false,
            command: None,
            launcher: None,
            help: None,
            launch: None,
            forward: None,
            modifiers: None,
        };
        if event.kind != EV_KEY
            || event.code > MAX_XKB_EVDEV_KEY
            || event.value == KEY_REPEAT
            || (event.value != KEY_PRESS && event.value != KEY_RELEASE)
        {
            return decision;
        }
        let physical = (device, event.code);
        let before = self.modifiers();
        let logical_pressed = self.pressed(event.code);
        let attention_held = self.attention_pressed(event.code);
        let changed = if event.value == KEY_PRESS {
            self.pressed.insert(physical)
        } else {
            self.pressed.remove(&physical)
        };
        if !changed {
            return decision;
        }
        if self.attention != AttentionState::Closed {
            // An excluded device is tracked above so the drain completes, and
            // its Escape still cancels; it never selects or confirms.
            let readable = !self.attention_excluded.contains(&device);
            // Only a press after this lifetime's operation was chosen can
            // type: never the press that chose it.
            let chosen = self.secret_selected;
            if self.attention == AttentionState::Open
                && readable
                && !self.secret_selected
                && !attention_held
                && event.value == KEY_PRESS
            {
                self.select(event.code, &mut decision);
                self.secret_selected |= decision.secret.is_some();
            }
            if self.attention == AttentionState::Open
                && readable
                && chosen
                && !attention_held
                && event.value == KEY_PRESS
            {
                decision.field = self.field_key(event.code).map(|key| (key, event.timestamp));
                decision.approval = self
                    .approval_digit(event.code)
                    .map(|digit| (digit, event.timestamp));
            }
            if self.attention == AttentionState::Open
                && readable
                && self.secret_selected
                && decision.secret.is_none()
                && !attention_held
                && event.value == KEY_PRESS
                && event.code == KEY_ENTER
            {
                decision.confirm_install = Some(event.timestamp);
            }
            if self.attention == AttentionState::Open
                && event.code == KEY_ESC
                && event.value == KEY_PRESS
            {
                self.attention = AttentionState::Draining;
                decision.draining = true;
            }
            if self.attention_can_close() {
                decision.attention = Some(false);
            }
            return decision;
        }
        if self.attention_enabled
            && event.code == KEY_ESC
            && event.value == KEY_PRESS
            && (self.pressed(KEY_LEFTCTRL) || self.pressed(KEY_RIGHTCTRL))
            && (self.pressed(KEY_LEFTALT) || self.pressed(KEY_RIGHTALT))
        {
            let held = |left, right| self.attention_pressed(left) || self.attention_pressed(right);
            decision.unlock = !self.attention_excluded.contains(&device)
                && !attention_held
                && held(KEY_LEFTCTRL, KEY_RIGHTCTRL)
                && held(KEY_LEFTALT, KEY_RIGHTALT);
            self.attention = AttentionState::Open;
            self.keyless_close = None;
            self.secret_selected = false;
            self.login_screen = LoginScreen::Closed;
            self.login_shown = None;
            self.forwarded.clear();
            self.pointer_forwarded.clear();
            self.consumed.clear();
            self.launcher_open = false;
            self.help_open = false;
            decision.attention = Some(true);
            return decision;
        }
        if event.value == KEY_PRESS && logical_pressed {
            let consumed = self.consumed.iter().any(|(_, code)| *code == event.code);
            let forwarded = self.forwarded.iter().any(|(_, code)| *code == event.code);
            if consumed {
                self.consumed.insert(physical);
            } else if forwarded {
                decision.forward = self.forward(physical, event);
            } else {
                self.consumed.insert(physical);
            }
            return decision;
        }
        match (event.code, event.value, logical_pressed) {
            (KEY_CAPSLOCK, KEY_PRESS, false) => self.caps_lock = !self.caps_lock,
            (KEY_NUMLOCK, KEY_PRESS, false) => self.num_lock = !self.num_lock,
            _ => {}
        }
        let after = self.modifiers();
        if before != after {
            decision.modifiers = Some(after);
        }
        if event.value == KEY_RELEASE && self.consumed.remove(&physical) {
            return decision;
        }
        if matches!(
            event.code,
            KEY_LEFTCTRL
                | KEY_RIGHTCTRL
                | KEY_LEFTMETA
                | KEY_RIGHTMETA
                | KEY_LEFTSHIFT
                | KEY_RIGHTSHIFT
                | KEY_LEFTALT
                | KEY_RIGHTALT
                | KEY_CAPSLOCK
                | KEY_NUMLOCK
        ) {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        if event.value == KEY_RELEASE {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        // `Super+l` in the paired profile alone (td-login/TOKEN-LOGIN.md,
        // "Session lock"), checked before the sheet's and the launcher's
        // capture so that neither swallows it, and always consumed, so no
        // client sees it. It locks while root's last `1a` answer is enrolled
        // or unavailable and does nothing while unenrolled.
        if self.attention_enabled
            && event.code == KEY_L
            && (self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA))
        {
            self.consumed.insert(physical);
            decision.lock = self.locks();
            return decision;
        }
        // Any NON-MODIFIER key dismisses the sheet: there is nothing to type
        // into it and nothing to select, so such a key can only mean "seen
        // it". Modifiers returned above, which is what lets someone release
        // Super to read and then press a whole chord that is swallowed whole.
        // Checked before the chords so `Super+t` closes rather than launching,
        // and before the launcher so a sheet is always dismissable.
        if self.help_open {
            self.consumed.insert(physical);
            decision.help = Some(HelpAction::Close);
            return decision;
        }
        if self.launcher_open {
            self.consumed.insert(physical);
            let control = self.pressed(KEY_LEFTCTRL) || self.pressed(KEY_RIGHTCTRL);
            let alt = self.pressed(KEY_LEFTALT) || self.pressed(KEY_RIGHTALT);
            let meta = self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA);
            decision.launcher = match event.code {
                KEY_DOWN => Some(LauncherAction::Next),
                KEY_UP => Some(LauncherAction::Previous),
                KEY_N if control => Some(LauncherAction::Next),
                KEY_P if control => Some(LauncherAction::Previous),
                KEY_ENTER | KEY_KPENTER => Some(LauncherAction::Activate),
                KEY_ESC => Some(LauncherAction::Close),
                KEY_G if control => Some(LauncherAction::Close),
                KEY_BACKSPACE if !control && !alt && !meta => Some(LauncherAction::Backspace),
                code if !control && !alt && !meta => {
                    launcher_character(code).map(LauncherAction::Insert)
                }
                _ => None,
            };
            return decision;
        }
        let meta = self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA);
        if !meta {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        let shift = self.pressed(KEY_LEFTSHIFT) || self.pressed(KEY_RIGHTSHIFT);
        // One chord per operation: no prefix, so a chord is read entirely
        // from what is held at this press.
        let chord = match event.code {
            KEY_F => Some(Command::ToggleFullscreen),
            KEY_S => Some(Command::ToggleGrouped),
            // V and H name the direction the BANDS run, not the container's
            // axis: stacked bands go down the column, tabs across it.
            KEY_V => Some(Command::SetPresentation(Presentation::Stacked)),
            KEY_H => Some(Command::SetPresentation(Presentation::Tabbed)),
            _ => None,
        };
        if let Some(command) = chord {
            self.consumed.insert(physical);
            decision.command = Some(command);
            return decision;
        }
        // Both Enters, because the OPEN overlay already activates on either
        // and a keypad that opens nothing while it activates is a coin toss.
        if event.code == KEY_ENTER || event.code == KEY_KPENTER {
            self.consumed.insert(physical);
            decision.launcher = Some(LauncherAction::Open);
            return decision;
        }
        // `?` is Shift+/ on this keymap, and Shift is not required: the sheet
        // is what someone reaches for when they do not know the bindings, so
        // demanding an exact one to see them would be the wrong way round.
        if event.code == KEY_SLASH {
            self.consumed.insert(physical);
            decision.help = Some(HelpAction::Toggle);
            return decision;
        }
        // The terminal without going through the launcher: it is the one entry
        // anybody opens repeatedly, and the registry still carries it.
        if event.code == KEY_T {
            self.consumed.insert(physical);
            decision.launch = Some(LaunchRequest::Terminal);
            return decision;
        }
        if let Some(direction) = direction(event.code) {
            self.consumed.insert(physical);
            decision.command = if shift {
                Some(Command::Move(direction))
            } else {
                Some(Command::Focus(direction))
            };
            return decision;
        }
        decision.command = workspace(event.code).map(|number| {
            if shift {
                Command::MoveToWorkspace(number)
            } else {
                Command::SwitchWorkspace(number)
            }
        });
        if decision.command.is_some() {
            self.consumed.insert(physical);
        } else {
            decision.forward = self.forward(physical, event);
        }
        decision
    }

    /// A fresh physical press on the open attention screen, from a device it
    /// reads, before the lifetime's one operation: a menu selection, `K`'s
    /// key-management screen, or a choice on that screen.
    fn select(&mut self, code: u16, decision: &mut KeyDecision) {
        use crate::attention::Notice;
        use crate::authority::consent::{Recovery, Role};
        use crate::secret_client::{Elevation, LoginSelection, Removal, Selection};
        // Nothing on a screen the person may not see yet is a choice.
        if self.login_screen != LoginScreen::Closed
            && !self
                .login_shown
                .as_ref()
                .is_some_and(crate::runtime::NoticePresentation::on_glass)
        {
            return;
        }
        if let LoginScreen::Removing(removal) = &mut self.login_screen {
            if code == KEY_ENTER {
                decision.secret = removal.selection().map(Selection::Login);
            } else if digit(code).is_some_and(|position| removal.toggle(position)) {
                decision.notice = Some(removal.shown());
            }
            return;
        }
        if self.login_screen == LoginScreen::Menu {
            decision.secret = match code {
                KEY_1 => Some(Selection::Login(LoginSelection::Enroll(1))),
                KEY_2 => Some(Selection::Login(LoginSelection::Enroll(2))),
                KEY_A => Some(Selection::Login(LoginSelection::Add)),
                KEY_D => {
                    use crate::authority::LoginState;
                    let state = self.login.state();
                    let removal = match &state {
                        Some(LoginState::Enrolled(keys)) => Removal::new(keys.clone()),
                        _ => None,
                    };
                    match removal {
                        Some(removal) => {
                            decision.notice = Some(removal.shown());
                            self.login_screen = LoginScreen::Removing(removal);
                        }
                        // No key list: refused here with why, and nothing
                        // is sent.
                        None => {
                            decision.notice = Some(no_login_keys(state));
                            self.secret_selected = true;
                        }
                    }
                    None
                }
                _ => None,
            };
            return;
        }
        decision.secret = match code {
            KEY_W => Some(Selection::Write),
            KEY_I => Some(Selection::Install),
            KEY_U => Some(Selection::Unlock(Role::Primary)),
            KEY_R => Some(Selection::Unlock(Role::Recovery)),
            KEY_E => Some(Selection::Enroll(Recovery::SecondToken)),
            KEY_X => Some(Selection::Enroll(Recovery::Unrecoverable)),
            KEY_K => {
                self.login_screen = LoginScreen::Menu;
                decision.notice = Some(Notice::LoginKeys);
                None
            }
            // A rollback: root's `1d`.
            KEY_B => Some(Selection::Elevation(Elevation::Rollback)),
            // The queued hostname change: root's `1e`, which refuses when
            // none waits.
            KEY_H => Some(Selection::Elevation(Elevation::Hostname)),
            // The lifetime's one selection: it ends attention into the
            // lock, or says why there is none and sends nothing.
            KEY_L => {
                self.secret_selected = true;
                if self.locks() {
                    decision.lock = true;
                } else {
                    decision.notice = Some(no_login_keys(self.login.state()));
                }
                None
            }
            _ => None,
        };
    }

    /// An approval-key digit for a fresh press from a device secure
    /// attention reads: a number-row 2 to 9, as its ASCII digit, with no
    /// Control, Alt or Super held on such a device. Shift is allowed, for
    /// layouts that shift digits; 0, 1 and the keypad are none.
    fn approval_digit(&self, code: u16) -> Option<u8> {
        let held = |left, right| self.attention_pressed(left) || self.attention_pressed(right);
        if held(KEY_LEFTCTRL, KEY_RIGHTCTRL)
            || held(KEY_LEFTALT, KEY_RIGHTALT)
            || held(KEY_LEFTMETA, KEY_RIGHTMETA)
            || !(KEY_2..=KEY_9).contains(&code)
        {
            return None;
        }
        let offset = u8::try_from(code.checked_sub(KEY_2)?).ok()?;
        b'2'.checked_add(offset)
    }

    /// Whether root's last `1a` answer locks: enrolled or unavailable.
    fn locks(&self) -> bool {
        self.login
            .current()
            .as_ref()
            .is_some_and(crate::authority::Answer::locks)
    }

    /// A fresh press for the PIN field, from a device secure attention
    /// reads: Enter submits, Backspace erases one byte, and a key of the
    /// `us` keymap types its byte, at Shift's level when a read device holds
    /// Shift. Under Control, Alt or Super nothing types; Caps Lock and every
    /// other key type nothing. Escape is the drain's.
    fn field_key(&self, code: u16) -> Option<crate::secret_client::FieldKey> {
        use crate::secret_client::FieldKey;
        let held = |left, right| self.attention_pressed(left) || self.attention_pressed(right);
        if held(KEY_LEFTCTRL, KEY_RIGHTCTRL)
            || held(KEY_LEFTALT, KEY_RIGHTALT)
            || held(KEY_LEFTMETA, KEY_RIGHTMETA)
        {
            return None;
        }
        match code {
            KEY_ENTER | KEY_KPENTER => Some(FieldKey::Submit),
            KEY_BACKSPACE => Some(FieldKey::Erase),
            _ => pin_byte(code, held(KEY_LEFTSHIFT, KEY_RIGHTSHIFT)).map(FieldKey::Byte),
        }
    }

    fn forward(&mut self, physical: (usize, u16), event: Event) -> Option<KeyInput> {
        if event.value == KEY_PRESS {
            let already_forwarded = self.forwarded.iter().any(|(_, code)| *code == event.code);
            self.forwarded.insert(physical);
            (!already_forwarded).then(|| event.key_input())
        } else if self.forwarded.remove(&physical)
            && !self.forwarded.iter().any(|(_, code)| *code == event.code)
        {
            Some(event.key_input())
        } else {
            None
        }
    }

    fn pressed(&self, code: u16) -> bool {
        self.pressed.iter().any(|(_, pressed)| *pressed == code)
    }

    /// Held on a device secure attention reads.
    fn attention_pressed(&self, code: u16) -> bool {
        self.pressed
            .iter()
            .any(|(device, pressed)| *pressed == code && !self.attention_excluded.contains(device))
    }

    fn settle_launcher(&mut self, visible: Option<bool>) {
        if let Some(visible) = visible {
            self.launcher_open = visible;
        }
    }

    fn settle_help(&mut self, visible: Option<bool>) {
        if let Some(visible) = visible {
            self.help_open = visible;
        }
    }

    fn modifiers(&self) -> ModifierState {
        // In attention an excluded device's modifiers are drained, not read;
        // that branch is for the PIN field's Shift. A value taken while Closed
        // is not comparable with one taken while Open.
        let held = |code| {
            if self.attention == AttentionState::Closed {
                self.pressed(code)
            } else {
                self.attention_pressed(code)
            }
        };
        let mut depressed = 0;
        if held(KEY_LEFTSHIFT) || held(KEY_RIGHTSHIFT) {
            depressed |= MOD_SHIFT;
        }
        if held(KEY_LEFTCTRL) || held(KEY_RIGHTCTRL) {
            depressed |= MOD_CONTROL;
        }
        if held(KEY_LEFTALT) || held(KEY_RIGHTALT) {
            depressed |= MOD_ALT;
        }
        if held(KEY_LEFTMETA) || held(KEY_RIGHTMETA) {
            depressed |= MOD_LOGO;
        }
        let mut locked = 0;
        if self.caps_lock {
            locked |= MOD_CAPS;
        }
        if self.num_lock {
            locked |= MOD_NUM;
        }
        ModifierState {
            depressed,
            latched: 0,
            locked,
            group: 0,
        }
    }

    fn pointer_changes(&self, time: u32) -> Vec<PointerButtonInput> {
        self.pointer_changes_with(time, |code| {
            self.pointer_pressed
                .iter()
                .any(|(_, pressed)| *pressed == code)
        })
    }

    fn pointer_device_changes(
        &self,
        device: usize,
        transitions: &[PointerButtonTransition],
        time: u32,
    ) -> Vec<PointerButtonInput> {
        if transitions.is_empty() {
            return Vec::new();
        }
        let mut device_pressed: BTreeSet<u16> = self
            .pointer_pressed
            .iter()
            .filter_map(|(owner, code)| (*owner == device).then_some(*code))
            .collect();
        let mut forwarded = self.pointer_forwarded.clone();
        let mut changes = Vec::new();
        for transition in transitions {
            if transition.pressed {
                device_pressed.insert(transition.code);
            } else {
                device_pressed.remove(&transition.code);
            }
            let physical = device_pressed.contains(&transition.code)
                || self
                    .pointer_pressed
                    .iter()
                    .any(|(owner, code)| *owner != device && *code == transition.code);
            if physical == forwarded.contains(&transition.code) {
                continue;
            }
            let state = if physical {
                forwarded.insert(transition.code);
                PointerButtonState::Pressed
            } else {
                forwarded.remove(&transition.code);
                PointerButtonState::Released
            };
            changes.push(PointerButtonInput {
                time,
                button: u32::from(transition.code),
                state,
            });
        }
        changes
    }

    fn pointer_changes_with(
        &self,
        time: u32,
        mut physical: impl FnMut(u16) -> bool,
    ) -> Vec<PointerButtonInput> {
        let mut changes = Vec::new();
        for code in BTN_MOUSE..=BTN_TASK {
            let pressed = physical(code);
            if pressed == self.pointer_forwarded.contains(&code) {
                continue;
            }
            changes.push(PointerButtonInput {
                time,
                button: u32::from(code),
                state: if pressed {
                    PointerButtonState::Pressed
                } else {
                    PointerButtonState::Released
                },
            });
        }
        changes
    }

    fn commit_pointer(&mut self, buttons: &[PointerButtonInput]) {
        for button in buttons {
            let Ok(code) = u16::try_from(button.button) else {
                continue;
            };
            match button.state {
                PointerButtonState::Pressed => {
                    self.pointer_forwarded.insert(code);
                }
                PointerButtonState::Released => {
                    self.pointer_forwarded.remove(&code);
                }
            }
        }
    }

    fn commit_pointer_device(
        &mut self,
        device: usize,
        pressed: &BTreeSet<u16>,
        buttons: &[PointerButtonInput],
    ) {
        self.remove_pointer_device(device);
        self.pointer_pressed
            .extend(pressed.iter().map(|code| (device, *code)));
        self.commit_pointer(buttons);
    }

    fn attention_can_close(&self) -> bool {
        self.attention == AttentionState::Draining
            && self.pressed.is_empty()
            && self.pointer_pressed.is_empty()
            && self.pointer_pending.is_empty()
    }

    fn remove_pointer_device(&mut self, device: usize) {
        self.pointer_pressed.retain(|(owner, _)| *owner != device);
    }
}

impl Event {
    fn key_input(self) -> KeyInput {
        KeyInput {
            time: self.time,
            key: u32::from(self.code),
            state: if self.value == KEY_RELEASE {
                KeyState::Released
            } else {
                KeyState::Pressed
            },
        }
    }
}

/// Where a device's absolute axes are and what they report over, read once per
/// device when its reader starts. A device with neither is relative and carries
/// `None`, which is what makes an ordinary mouse cost no ioctl and no branch it
/// does not use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AbsoluteAxes {
    x: sys::AbsInfo,
    y: sys::AbsInfo,
}

impl AbsoluteAxes {
    /// A device is absolute exactly when BOTH its axes have a span to place a
    /// value in. Asked of `fraction` rather than of the numbers, so the
    /// admission test and the scaling cannot come to different conclusions
    /// about a span.
    fn declared(x: sys::AbsInfo, y: sys::AbsInfo) -> Option<Self> {
        (Self::fraction(x, x.maximum).denominator > 0
            && Self::fraction(y, y.maximum).denominator > 0)
            .then_some(Self { x, y })
    }

    /// Where along the axis a raw value sits, as the EXACT ratio of the
    /// device's own offset to the device's own span — nothing is rescaled
    /// here, since a second division is a second flooring.
    ///
    /// Values outside the declared range are CLAMPED rather than refused: a
    /// device may report past its own bounds, and the honest reading of one
    /// that does is the edge it went past. A span that is not positive answers
    /// a zero DENOMINATOR rather than an error, which is `declared`'s question
    /// and which `across` reads as the near edge.
    fn fraction(axis: sys::AbsInfo, value: i32) -> Fraction {
        let span = i64::from(axis.maximum).saturating_sub(i64::from(axis.minimum));
        let offset = i64::from(value)
            .saturating_sub(i64::from(axis.minimum))
            .clamp(0, span.max(0));
        Fraction {
            numerator: u32::try_from(offset).unwrap_or(u32::MAX),
            denominator: u32::try_from(span).unwrap_or(0),
        }
    }
}

/// What an absolute device's ABS_X/ABS_Y mean. A tablet's are a place on the
/// screen; a touchpad's are a finger on a pad, whose MOTION moves the pointer.
/// Tablet is the default because it is today's reading of any declared span:
/// a device that cannot be classified keeps it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AbsoluteKind {
    #[default]
    Tablet,
    Touchpad,
}

/// One bit of a sysfs input bitmap: hex words separated by spaces, most
/// significant first, leading zero words omitted, each as wide as the
/// kernel's `unsigned long` as this process sees it (a compat reader is shown
/// 32-bit words). `None` when the text is not such a bitmap.
fn bitmap_bit(text: &str, bit: usize) -> Option<bool> {
    let word_bits = usize::try_from(usize::BITS).ok()?;
    let mut words = Vec::new();
    for word in text.split_ascii_whitespace() {
        if !word.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        words.push(usize::from_str_radix(word, 16).ok()?);
    }
    let last = words.len().checked_sub(1)?;
    let Some(at) = last.checked_sub(bit.checked_div(word_bits)?) else {
        return Some(false);
    };
    let word = words.get(at)?;
    let shift = u32::try_from(bit.checked_rem(word_bits)?).ok()?;
    Some(word.checked_shr(shift)? & 1 == 1)
}

/// A touchpad is a pointer (`INPUT_PROP_POINTER`), not a direct surface
/// (`INPUT_PROP_DIRECT`), that reports a finger (`BTN_TOOL_FINGER`) and no pen
/// (`BTN_TOOL_PEN`, udev's `finger_but_no_pen`). Anything missing or
/// unreadable keeps the tablet reading.
fn classify(properties: Option<&str>, keys: Option<&str>) -> AbsoluteKind {
    let touchpad = (|| {
        let (properties, keys) = (properties?, keys?);
        Some(
            bitmap_bit(properties, INPUT_PROP_POINTER)?
                && !bitmap_bit(properties, INPUT_PROP_DIRECT)?
                && bitmap_bit(keys, usize::from(BTN_TOOL_FINGER))?
                && !bitmap_bit(keys, usize::from(BTN_TOOL_PEN))?,
        )
    })();
    if touchpad == Some(true) {
        AbsoluteKind::Touchpad
    } else {
        AbsoluteKind::Tablet
    }
}

fn read_bitmap(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path)
        .ok()?
        .take(SYSFS_BITMAP_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    (u64::try_from(text.len()).ok()? <= SYSFS_BITMAP_BYTES).then_some(text)
}

/// Classify an absolute node from sysfs rather than `EVIOCGPROP`/`EVIOCGBIT`,
/// which would be new ioctls. The node's basename is its sysfs name, as
/// `framebuffer.rs` reads `/sys/class/graphics`.
fn absolute_kind(sysfs: &Path, node: &Path) -> AbsoluteKind {
    let Some(name) = node.file_name() else {
        return AbsoluteKind::Tablet;
    };
    let device = sysfs.join(name).join("device");
    classify(
        read_bitmap(&device.join("properties")).as_deref(),
        read_bitmap(&device.join("capabilities").join("key")).as_deref(),
    )
}

/// Whether a node is a lid switch (td-compositor/DESIGN.md item 6): a
/// switch-only device, whose `capabilities/ev` declares `EV_SW` and no type
/// but `EV_SYN` beside it, and whose `capabilities/sw` declares `SW_LID`.
/// Read from sysfs, as `absolute_kind` is, rather than through new ioctls.
/// Anything unreadable answers `false`, and the node is read as before.
fn lid_switch(sysfs: &Path, node: &Path) -> bool {
    let Some(name) = node.file_name() else {
        return false;
    };
    let capabilities = sysfs.join(name).join("device").join("capabilities");
    let (Some(types), Some(switches)) = (
        read_bitmap(&capabilities.join("ev")),
        read_bitmap(&capabilities.join("sw")),
    ) else {
        return false;
    };
    let switch_only = (0..EV_CNT).all(|kind| {
        bitmap_bit(&types, usize::from(kind))
            .is_some_and(|set| set == (kind == EV_SW) || kind == EV_SYN)
    });
    switch_only && bitmap_bit(&switches, usize::from(SW_LID)) == Some(true)
}

/// Whether a lid switch is admitted (td-compositor/DESIGN.md item 6): in
/// the paired profile, where root's answer at connect is enrolled or
/// unavailable, the answer the generation's first paint locks on.
fn lid_admitted(attention_enabled: bool, connected: Option<&crate::authority::Answer>) -> bool {
    attention_enabled && connected.is_some_and(crate::authority::Answer::locks)
}

/// Splits the roster into the ordinary readers' nodes and the lid
/// switches `read_lid` reads, which only `lid` admits: an unadmitted lid
/// switch is not opened at all. Either way no lid switch reaches an
/// ordinary reader, which would take it for a keyboard or pointer.
fn admit_lids(paths: Vec<PathBuf>, sysfs: &Path, lid: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (lids, ordinary): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.into_iter().partition(|path| lid_switch(sysfs, path));
    (ordinary, if lid { lids } else { Vec::new() })
}

/// Whether a node shares its USB device with a FIDO interface: a security
/// key's own OTP keyboard, whose touch types modhex and Enter. Read from
/// sysfs with plain file reads. Anything unreadable, and a node with no USB
/// device above it (PS/2, UHID, virtio), answers `false`: it stays admitted.
fn security_key_keyboard(sysfs: &Path, node: &Path) -> bool {
    let Some(name) = node.file_name() else {
        return false;
    };
    usb_device(&sysfs.join(name).join("device")).is_some_and(|usb| shares_fido_interface(&usb))
}

/// The nearest ancestor of an input device carrying `idVendor` and `busnum`:
/// the USB device, not the interface or HID device below it, and not a hub
/// above it, which is a USB device of its own and is never reached.
fn usb_device(input: &Path) -> Option<PathBuf> {
    let mut at = fs::canonicalize(input).ok()?;
    for _ in 0..SYSFS_ANCESTOR_LIMIT {
        // Nothing at or above `/sys/devices` is a USB device.
        if at.file_name()? == "devices" {
            return None;
        }
        if at.join("idVendor").is_file() && at.join("busnum").is_file() {
            return Some(at);
        }
        at = at.parent()?.to_path_buf();
    }
    None
}

/// Whether any interface of a USB device (`1-2:1.0` under `1-2`) has a HID
/// child whose report descriptor declares the FIDO usage page.
fn shares_fido_interface(usb: &Path) -> bool {
    let Some(name) = usb.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let prefix = format!("{name}:");
    let Some(interfaces) = sysfs_directories(usb) else {
        return false;
    };
    interfaces
        .iter()
        .filter(|(interface, _)| interface.starts_with(&prefix))
        .any(|(_, interface)| {
            sysfs_directories(interface).is_some_and(|children| {
                children.iter().any(|(_, child)| {
                    read_report_descriptor(&child.join("report_descriptor"))
                        .and_then(|descriptor| declares_fido_page(&descriptor))
                        == Some(true)
                })
            })
        })
}

/// A sysfs directory's real subdirectories. Symlinks (`driver`, `subsystem`,
/// `port`) lead out of the device's own subtree and are skipped. More than
/// `SYSFS_DIRECTORY_ENTRIES` entries is not a device directory.
fn sysfs_directories(directory: &Path) -> Option<Vec<(String, PathBuf)>> {
    let mut directories = Vec::new();
    for (index, entry) in fs::read_dir(directory).ok()?.enumerate() {
        if index >= SYSFS_DIRECTORY_ENTRIES {
            return None;
        }
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string() {
            directories.push((name, entry.path()));
        }
    }
    Some(directories)
}

fn read_report_descriptor(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(REPORT_DESCRIPTOR_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).ok()? <= REPORT_DESCRIPTOR_BYTES).then_some(bytes)
}

/// Walk a HID report descriptor's items and say whether one declares the
/// FIDO usage page: a Usage Page item (`0x05`/`0x06`/`0x07`, one, two or four
/// data bytes, the whole value compared), or a four-byte extended Usage,
/// Usage Minimum or Usage Maximum whose high half names the page. A long
/// item (prefix `0xf0`-`0xff`, size, tag, data) is skipped whole: the
/// kernel refuses a descriptor holding one, and skipping can only exclude
/// more. `None` for a truncated item, which the kernel refuses as well.
fn declares_fido_page(descriptor: &[u8]) -> Option<bool> {
    let mut declared = false;
    let mut rest = descriptor;
    while let Some((&prefix, tail)) = rest.split_first() {
        if prefix & HID_LONG_ITEM == HID_LONG_ITEM {
            let (&size, tail) = tail.split_first()?;
            let (_tag, tail) = tail.split_first()?;
            rest = tail.get(usize::from(size)..)?;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            size => usize::from(size),
        };
        let data = tail.get(..size)?;
        rest = tail.get(size..)?;
        // Little-endian, at most four bytes.
        let value = data
            .iter()
            .rev()
            .fold(0u32, |value, byte| value << 8 | u32::from(*byte));
        let kind = prefix & 0xfc;
        if kind == HID_USAGE_PAGE {
            declared |= value == FIDO_USAGE_PAGE;
        } else if size == 4 && HID_EXTENDED_USAGES.contains(&kind) {
            declared |= value >> 16 == FIDO_USAGE_PAGE;
        }
    }
    Some(declared)
}

/// One touchpad axis: the value a report last NAMED since open or the last
/// discard, the contact's anchor on it, and the sub-pixel carry. A value the
/// stream has not named is never anchored on: neither absinfo's `value`, a
/// snapshot that can be newer than reports still queued, nor anything held
/// from before a gap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TouchAxis {
    reported: Option<i32>,
    anchor: Option<i32>,
    remainder: i64,
}

impl TouchAxis {
    fn release(&mut self) {
        self.anchor = None;
        self.remainder = 0;
    }

    /// Pixels travelled since the anchor, which then moves here. The first
    /// named value only anchors.
    fn travel(&mut self, scale: (i64, i64)) -> i32 {
        let Some(at) = self.reported else {
            self.release();
            return 0;
        };
        let Some(from) = self.anchor.replace(at) else {
            return 0;
        };
        touch_pixels(
            i64::from(at).saturating_sub(i64::from(from)),
            scale,
            &mut self.remainder,
        )
    }
}

/// A touchpad's contact keys and axes. A discard clears all of it: the
/// kernel re-sends neither an unchanged key nor an unchanged axis, so after a
/// gap none of it is known, and motion waits for a real contact transition
/// and a report naming each axis.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TouchTrack {
    touch: bool,
    finger: bool,
    /// One bit per `MULTI_FINGER_TOOLS` entry held.
    multi: u8,
    x: TouchAxis,
    y: TouchAxis,
}

impl TouchTrack {
    fn tracking(&self) -> bool {
        self.touch && self.finger && self.multi == 0
    }

    fn key(&mut self, code: u16, pressed: bool) {
        match code {
            BTN_TOUCH => self.touch = pressed,
            BTN_TOOL_FINGER => self.finger = pressed,
            _ => {
                if let Some(bit) = MULTI_FINGER_TOOLS
                    .iter()
                    .position(|tool| *tool == code)
                    .and_then(|index| u32::try_from(index).ok())
                    .and_then(|index| 1u8.checked_shl(index))
                {
                    if pressed {
                        self.multi |= bit;
                    } else {
                        self.multi &= !bit;
                    }
                }
            }
        }
    }

    /// The pixels one report moves the pointer: only a single finger in
    /// contact moves it, from the second named value of each axis on.
    fn motion(&mut self, axes: AbsoluteAxes, raw_x: Option<i32>, raw_y: Option<i32>) -> (i32, i32) {
        self.x.reported = raw_x.or(self.x.reported);
        self.y.reported = raw_y.or(self.y.reported);
        if !self.tracking() {
            self.x.release();
            self.y.release();
            return (0, 0);
        }
        let (scale_x, scale_y) = touch_scales(axes);
        (self.x.travel(scale_x), self.y.travel(scale_y))
    }
}

/// Pixels per device unit on each axis, as (numerator, denominator). Each
/// axis's own resolution where both state one; one stated resolution serves
/// both, assuming square units; with none, X's span at `TOUCHPAD_SPAN_PX`
/// serves both.
fn touch_scales(axes: AbsoluteAxes) -> ((i64, i64), (i64, i64)) {
    let per_mm = |resolution: i32| (TOUCHPAD_PX_PER_MM, i64::from(resolution));
    let (x, y) = (axes.x.resolution, axes.y.resolution);
    match (x > 0, y > 0) {
        (true, true) => (per_mm(x), per_mm(y)),
        (true, false) => (per_mm(x), per_mm(x)),
        (false, true) => (per_mm(y), per_mm(y)),
        (false, false) => {
            let span = i64::from(axes.x.maximum).saturating_sub(i64::from(axes.x.minimum));
            ((TOUCHPAD_SPAN_PX, span), (TOUCHPAD_SPAN_PX, span))
        }
    }
}

/// Scale a travel in device units to whole pixels, carrying the rest so slow
/// motion accumulates rather than rounding away.
fn touch_pixels(units: i64, (numerator, denominator): (i64, i64), remainder: &mut i64) -> i32 {
    if denominator <= 0 {
        *remainder = 0;
        return 0;
    }
    let total = units.saturating_mul(numerator).saturating_add(*remainder);
    let pixels = total.checked_div(denominator).unwrap_or(0);
    *remainder = total.checked_rem(denominator).unwrap_or(0);
    i32::try_from(pixels).unwrap_or(if pixels < 0 { i32::MIN } else { i32::MAX })
}

#[derive(Clone, Copy, Default)]
struct EventTimeline {
    cutoff: Option<u128>,
    discard: bool,
}

impl PointerMotion {
    /// Kernel queues and returned batches can outlive cancellation on another
    /// device. Reject those records before they mutate any seat state.
    /// `settled`, after a close no key made, replaces the discard through a
    /// device's first report with a window: only a report stamped in it can
    /// straddle that close, and the person's next key is not lost.
    fn accept_event(&mut self, event: Event, cutoff: Option<u128>, settled: Option<u128>) -> bool {
        if self.timeline.cutoff != cutoff {
            self.reset();
            self.timeline.cutoff = cutoff;
            self.timeline.discard = settled.is_none();
        }
        let stale =
            cutoff.is_some_and(|cutoff| event.timestamp <= settled.unwrap_or(cutoff).max(cutoff));
        let boundary = event.kind == EV_SYN && event.code == SYN_REPORT;
        let rejected = stale || self.timeline.discard;
        if rejected {
            self.reset();
            self.timeline.discard = !boundary;
        }
        !rejected
    }
}

#[derive(Default)]
struct PointerMotion {
    timeline: EventTimeline,
    dx: i32,
    dy: i32,
    /// The absolute value each axis reported in the frame being built. A
    /// tablet sends a POSITION rather than a movement, so the frame carries
    /// the newest one rather than a sum. Separate options because the kernel
    /// omits an axis whose value has not changed, so a report can name one and
    /// say nothing about the other.
    abs_x: Option<i32>,
    abs_y: Option<i32>,
    /// Where THIS DEVICE last was, which is what an omitted axis means. The
    /// cursor's own coordinate will not do: it is shared, so a relative mouse
    /// moving between two tablet reports would leave the axis the tablet did
    /// not mention wherever the MOUSE put it, which is nowhere the stylus is.
    /// `None` until the first report, and answered from `input_absinfo.value`
    /// then — the axis's position at open, which is the only way to know where
    /// a device is before it has said anything.
    held_x: Option<i32>,
    held_y: Option<i32>,
    /// Detents accumulated this frame, summed as the deltas are: a fast flick
    /// puts several notches in one report, and a wheel that changed direction
    /// inside one meant the difference.
    wheel: i32,
    hwheel: i32,
    pressed: BTreeSet<u16>,
    buttons: Vec<PointerButtonTransition>,
    overflowed: bool,
    /// `Some` for a touchpad, whose declared axes are read as finger motion.
    touchpad: Option<TouchTrack>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerButtonTransition {
    code: u16,
    pressed: bool,
}

/// Where a frame says the pointer is. A relative device can only say how far
/// it moved; an absolute one says where it IS, as a fraction of its own span
/// along BOTH axes. Both even though a report may name only one: the reader
/// holds where the device last was, so the axis a report omits is answered
/// from that device rather than from the shared cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerPlace {
    By { dx: i32, dy: i32 },
    At { x: Fraction, y: Fraction },
}

#[derive(Debug, Eq, PartialEq)]
struct PointerFrame {
    time: u32,
    place: PointerPlace,
    buttons: Vec<PointerButtonTransition>,
    scroll: PointerScroll,
}

trait InputTarget {
    fn confirm_install(&mut self, _timestamp: u128) -> Result<(), String> {
        Ok(())
    }
    /// An approval-key digit for the attempt's presented elevation prompt:
    /// answers whether it ended the request. A target with no attempt
    /// takes none.
    fn approval_digit(&mut self, _digit: u8, _timestamp: u128) -> Result<bool, String> {
        Ok(false)
    }
    fn secret_request(&mut self, _role: crate::secret_client::Selection) -> Result<(), String> {
        Err("secret requests unavailable on this input target".into())
    }
    /// A key for the PIN field; a target with no attempt takes none.
    fn pin_key(
        &mut self,
        _key: crate::secret_client::FieldKey,
        _timestamp: u128,
    ) -> Result<(), String> {
        Ok(())
    }
    /// Shows an attention screen that selects nothing yet, answering with
    /// its paint, or none when it failed; a screen not shown selects
    /// nothing.
    fn attention_notice(
        &mut self,
        _notice: crate::attention::Notice,
    ) -> Option<crate::runtime::NoticePresentation> {
        None
    }
    fn attention_closed(&mut self) {}
    /// Whether the lock surface is up; a target with none never is.
    fn session_locked(&mut self) -> bool {
        false
    }
    /// Puts the lock surface up, answering with its paint; a target with
    /// none refuses.
    fn lock_screen(&mut self) -> Result<crate::runtime::NoticePresentation, String> {
        Err("no lock surface on this input target".into())
    }

    fn attention(&mut self, visible: bool) -> Result<u128, String>;
    fn drain_attention(&mut self) -> Result<(), String>;
    fn command(&mut self, command: Command) -> Result<(), String>;
    fn launcher(&mut self, action: LauncherAction) -> Result<bool, String>;
    /// Answers whether the sheet is up afterwards, as `launcher` does: the
    /// adapter must know to route the NEXT key to it.
    fn help(&mut self, action: HelpAction) -> Result<bool, String>;
    /// Spawn a registry entry WITHOUT opening the overlay — `Super+t`'s whole
    /// point. Separate from `launcher` because that one is about the overlay's
    /// model and returns its visibility, which this never changes.
    fn launch(&mut self, request: LaunchRequest) -> Result<(), String>;
    /// What the pointer report just delivered asks of the launcher, which
    /// the adapter then puts through `launcher` as it would a key's.
    /// `pressed` is a left press withheld because the overlay is up. A
    /// target with no launcher answers nothing.
    fn pointer_launcher(&mut self, _pressed: bool) -> Result<Option<LauncherAction>, String> {
        Ok(None)
    }
    fn key(&mut self, input: KeyInput) -> Result<(), String>;
    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String>;
    fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String>;

    /// The absolute form of the same report. Separate rather than one method
    /// with a sum type, because the two are different questions to everything
    /// downstream: one composes with where the pointer was and the other
    /// replaces it.
    fn pointer_frame_at(
        &mut self,
        time: u32,
        x: Fraction,
        y: Fraction,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String>;

    /// Take any paint the delivered reports left owing.
    fn flush(&mut self) -> Result<(), String>;
}

struct LiveInputTarget {
    runtime: Arc<Mutex<Runtime>>,
    launches: LaunchBackend,
    secret_attempt: Option<Arc<crate::secret_client::Attempt>>,
    /// This target's own seat, lent to each attempt it opens.
    seat: Seat,
}

/// How long after a close no key started a report is still taken to
/// straddle it: a driver flushes a report in the call that began it, far
/// inside this.
const SELF_CLOSE_SETTLE: u128 = 100_000_000;
/// Said on standard error, which reaches the console, when the seat closes
/// a screen itself, by which close, and at the first report past its
/// settle window: times and a count only, never which keys, since the next
/// keys may be td-setup's typed-back recovery key or whatever the person
/// types into the session they just unlocked.
pub(crate) const SELF_CLOSE_MARKER: &str = "TD-ATTENTION-SELF-CLOSE";
pub(crate) const UNLOCK_CLOSE_MARKER: &str = "TD-ATTENTION-UNLOCK-CLOSE";
pub(crate) const SUSPEND_CLOSE_MARKER: &str = "TD-ATTENTION-SUSPEND-CLOSE";
pub(crate) const SETTLED_MARKER: &str = "TD-ATTENTION-SETTLED";

/// A close of the attention screen that no key started. A key's close,
/// Escape's or the menu's `L`, leaves each device's first report to be
/// discarded, and that key's release absorbs the discard; a close no key
/// started has nothing to absorb it, so it settles by time
/// (`SELF_CLOSE_SETTLE`) instead, and the person's next key is not lost.
/// The kind is fixed when the drain starts: a drain that held keys or
/// buttons, or a device's removal, later complete closes as it began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeylessClose {
    /// Root's `06` for a committed login unlock (`Seat::unlocked`).
    Unlock,
    /// A committed device-bound disk installation's notice
    /// (`Seat::release`).
    Install,
    /// A lid close's or a resume's lock (`lock_for_suspend`).
    Suspend,
}

impl KeylessClose {
    fn marker(self) -> &'static str {
        match self {
            Self::Unlock => UNLOCK_CLOSE_MARKER,
            Self::Install => SELF_CLOSE_MARKER,
            Self::Suspend => SUSPEND_CLOSE_MARKER,
        }
    }
}

/// Writes `line` and its newline to standard error in one call: `eprintln!`
/// writes each formatting piece separately, and on the shared console
/// another service's line, td-setup's evidence among them, can land
/// between them. Best effort, as `eprintln!` is not.
fn say_line(line: &str) {
    let mut bytes = Vec::with_capacity(line.len().saturating_add(1));
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let _ = std::io::stderr().lock().write_all(&bytes);
}

/// The evdev adapter's seat, lent to an attention lifetime's attempt so
/// that root's success for a login unlock (`91 06`) can end the lifetime
/// the person opened on the lock surface, and so that a committed
/// device-bound disk installation can close its screen without a key
/// (`release`). The authority worker holds no
/// lock when it calls in, and takes the bindings, then the target, then
/// through it the attempt's field and the runtime: every input reader's
/// order. Weak, since the target holds the attempt that holds this.
#[derive(Clone, Default)]
pub(crate) struct Seat {
    bindings: Weak<Mutex<KeyBindings>>,
    target: Weak<Mutex<LiveInputTarget>>,
}

impl Seat {
    /// Leaves the lock surface and ends `attempt`'s lifetime, which must
    /// still be the open one with its unlock committed and not cancelled:
    /// under the bindings lock no Escape interleaves, and one that came
    /// first ended the lifetime still locked. Held input drains first, as
    /// after Escape, under `RELEASE KEYS AND BUTTONS`; a close that fails
    /// keeps capture, as there. The close settles by time.
    pub(crate) fn unlocked(&self, attempt: &crate::secret_client::Attempt) -> Result<(), String> {
        let closed = self.close_itself(attempt, KeylessClose::Unlock, |bindings, target| {
            if !attempt.unlock_committed() {
                return Ok(());
            }
            let mut runtime = target
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?;
            let origin = EvdevOrigin { _private: () };
            runtime.unlock_session(&origin)?;
            bindings.attention = AttentionState::Draining;
            // The runtime's drain, not the target's: that one cancels the
            // attempt, and this one succeeded.
            runtime.drain_attention(&origin)
        })?;
        if let Some(Err(error)) = closed {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
        Ok(())
    }

    /// Drains and, once no key or button is held, closes the screen, as
    /// Escape does, if it is open and still `attempt`'s: after a committed
    /// device-bound disk installation's notice, whose recovery key td-setup
    /// shows. The close settles by time. A drain that fails leaves the
    /// screen open as it was, for Escape.
    pub(crate) fn release(&self, attempt: &crate::secret_client::Attempt) -> Result<(), String> {
        self.close_itself(attempt, KeylessClose::Install, |bindings, target| {
            bindings.attention = AttentionState::Draining;
            let drained = target.drain_attention();
            if drained.is_err() {
                bindings.attention = AttentionState::Open;
            }
            drained
        })?
        .ok_or_else(|| "the input seat that opened attention is gone".to_string())?
    }

    /// The seat's own close of `attempt`'s screen, which no key started:
    /// under the bindings, then the target, and only while `attempt`'s
    /// screen is the open one. `drain` drains it, or leaves it open; once
    /// it is draining, the close is `close`, which settles by time, and
    /// the screen closes as soon as no key or button is held. Answers
    /// `None` when the seat is gone, and otherwise the close's own result.
    fn close_itself(
        &self,
        attempt: &crate::secret_client::Attempt,
        close: KeylessClose,
        drain: impl FnOnce(&mut KeyBindings, &mut LiveInputTarget) -> Result<(), String>,
    ) -> Result<Option<Result<(), String>>, String> {
        let (Some(bindings), Some(target)) = (self.bindings.upgrade(), self.target.upgrade())
        else {
            return Ok(None);
        };
        let mut bindings = bindings
            .lock()
            .map_err(|_| "input bindings lock poisoned".to_string())?;
        let mut target = target
            .lock()
            .map_err(|_| "input target lock poisoned".to_string())?;
        let current = target
            .secret_attempt
            .as_ref()
            .is_some_and(|current| std::ptr::eq(Arc::as_ptr(current), attempt));
        if !current || bindings.attention != AttentionState::Open {
            return Ok(Some(Ok(())));
        }
        let drained = drain(&mut bindings, &mut target);
        if bindings.attention != AttentionState::Draining {
            return drained.map(|()| Some(Ok(())));
        }
        bindings.keyless_close = Some(close);
        drained?;
        Ok(Some(finish_attention(&mut *target, &mut bindings)))
    }
}

/// The lock surface's live entry (td-login/TOKEN-LOGIN.md, "Session
/// lock"), for `Super+l` and the attention menu's `L`, and through
/// `lock_for_suspend` for a lid close and a resume, through the
/// bindings and in the paired profile alone; the connect-time lock needs
/// no bindings. An open attention lifetime ends first, as Escape ends it:
/// before its operation's commit the attempt is cancelled, after it the
/// screen drains and shows no result, and a login unlock's lifetime leaves
/// the session locked. The launcher's and the sheet's capture close with
/// the overlays the runtime closes, and attention closes onto the lock
/// surface once held input has drained. Answers with the runtime's paint.
fn lock_session<T: InputTarget>(
    target: &mut T,
    bindings: &mut KeyBindings,
) -> Result<crate::runtime::NoticePresentation, String> {
    if !bindings.attention_enabled {
        return Err("the lock surface needs the paired profile".into());
    }
    let drained = if bindings.attention == AttentionState::Open {
        bindings.attention = AttentionState::Draining;
        target.drain_attention()
    } else {
        Ok(())
    };
    // The lock never waits on the drain's paint: a drain that failed to
    // paint left the scene draining, and the session locks all the same,
    // so no error below leads back to an unlocked desktop.
    let locked = target.lock_screen();
    if target.session_locked() {
        bindings.settle_launcher(Some(false));
        bindings.settle_help(Some(false));
    }
    drained?;
    let presentation = locked?;
    finish_attention(target, bindings)?;
    Ok(presentation)
}

/// The live entry for a lid close and a resume (td-compositor/DESIGN.md
/// items 6 and 7), which call it holding the bindings: in the paired
/// profile alone, and only while root's last `1a` answer is enrolled or
/// unavailable. Otherwise it does nothing. An open attention lifetime ends
/// as `lock_session` ends it, as Escape does, except that no key started
/// its close, which therefore settles (`KeylessClose::Suspend`).
fn lock_for_suspend<T: InputTarget>(
    target: &Mutex<T>,
    bindings: &mut KeyBindings,
) -> Result<(), String> {
    if !bindings.attention_enabled || !bindings.locks() {
        return Ok(());
    }
    let mut target = target
        .lock()
        .map_err(|_| "input target lock poisoned".to_string())?;
    if bindings.attention == AttentionState::Open {
        bindings.keyless_close = Some(KeylessClose::Suspend);
    }
    lock_session(&mut *target, bindings).map(|_| ())
}

/// A lid switch's reader (td-compositor/DESIGN.md item 6): a close locks
/// through `lock_for_suspend`, and nothing else is read from it, so none
/// of its reports reaches the bindings, the pointer or a client. A lock
/// that fails is reported and the lid read on, since the entry locks
/// before anything can fail; a read that fails ends the reader.
fn read_lid<T: InputTarget>(
    path: &Path,
    file: &mut impl Read,
    target: &Mutex<T>,
    bindings: &Mutex<KeyBindings>,
) -> Result<(), String> {
    let mut buffer = [0u8; READ_BATCH_BYTES];
    let mut filled = 0usize;
    loop {
        let tail = match buffer.get_mut(filled..) {
            Some(tail) if !tail.is_empty() => tail,
            _ => return Err(format!("input {} overran its batch buffer", path.display())),
        };
        let read = match file.read(tail) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read input {}: {error}", path.display())),
        };
        filled = filled.saturating_add(read);
        let records = filled / EVENT_SIZE;
        for index in 0..records {
            let at = index.saturating_mul(EVENT_SIZE);
            let record = buffer
                .get(at..at.saturating_add(EVENT_SIZE))
                .ok_or_else(|| format!("input {} lost a record", path.display()))?;
            let event = parse(record)?;
            if event.kind == EV_SW && event.code == SW_LID && event.value == LID_CLOSED {
                let mut bindings = bindings
                    .lock()
                    .map_err(|_| "input bindings lock poisoned".to_string())?;
                if let Err(error) = lock_for_suspend(target, &mut bindings) {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: lid: {error}");
                }
            }
        }
        filled = carry_remainder(&mut buffer, records.saturating_mul(EVENT_SIZE), filled);
    }
}

/// More than this between how far the boot-time clock and the monotonic
/// clock advanced since the last accepted sample is a suspend.
const RESUME_GAP: Duration = Duration::from_secs(2);
/// A sample whose two monotonic reads, around its boot-time read, are
/// further apart than this is discarded and retaken.
const SAMPLE_SPREAD: Duration = Duration::from_millis(100);
/// Discards in a row before a check counts as unverifiable.
const SAMPLE_RETAKES: usize = 16;
/// The monitor's period: at least once a second, with room to spare for
/// scheduling.
const RESUME_PERIOD: Duration = Duration::from_millis(500);
/// The boot-time clock, which counts suspended time.
const UPTIME: &str = "/proc/uptime";
/// Its one line is two decimals; anything longer is refused.
const UPTIME_BYTES: usize = 64;

/// The two clocks resume detection compares, a seam for its tests.
trait ResumeClocks: Send {
    /// The monotonic clock, which stops while the machine is suspended.
    fn monotonic(&mut self) -> Duration;
    /// The boot-time clock, which does not.
    fn boot(&mut self) -> Result<Duration, String>;
}

/// Production's clocks, both safe `std`: `Instant`, which is
/// `CLOCK_MONOTONIC` on Linux, and `/proc/uptime`, which the kernel reads
/// from `CLOCK_BOOTTIME`. The file stays open and is read from offset zero
/// each time, so a check opens no descriptor; one that fails to read is
/// closed and opened again by the next.
struct SystemClocks {
    origin: Instant,
    uptime: Option<File>,
}

impl ResumeClocks for SystemClocks {
    fn monotonic(&mut self) -> Duration {
        self.origin.elapsed()
    }

    fn boot(&mut self) -> Result<Duration, String> {
        let file = match self.uptime.take() {
            Some(file) => file,
            None => File::open(UPTIME).map_err(|error| format!("open {UPTIME}: {error}"))?,
        };
        let mut buffer = [0u8; UPTIME_BYTES + 1];
        let read = file
            .read_at(&mut buffer, 0)
            .map_err(|error| format!("read {UPTIME}: {error}"))?;
        self.uptime = Some(file);
        buffer
            .get(..read)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(uptime)
            .ok_or_else(|| format!("{UPTIME} is malformed"))
    }
}

/// `/proc/uptime`'s first field: whole seconds, a point and a fraction of
/// at most nine digits.
fn uptime(text: &str) -> Option<Duration> {
    if text.len() > UPTIME_BYTES {
        return None;
    }
    let (seconds, fraction) = text.split_ascii_whitespace().next()?.split_once('.')?;
    let digits = |field: &str| !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit());
    if !digits(seconds) || !digits(fraction) || fraction.len() > 9 {
        return None;
    }
    let scale = 10u32.checked_pow(9u32.checked_sub(u32::try_from(fraction.len()).ok()?)?)?;
    let nanos = fraction.parse::<u32>().ok()?.checked_mul(scale)?;
    Duration::from_secs(seconds.parse().ok()?).checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// One accepted sample: the monotonic clock just before the boot-time
/// clock was read, and that read.
#[derive(Clone, Copy)]
struct Reading {
    monotonic: Duration,
    boot: Duration,
}

struct Watch {
    clocks: Box<dyn ResumeClocks>,
    /// The last accepted sample; none before the first, while root's last
    /// answer does not lock, and after an unverifiable check.
    baseline: Option<Reading>,
    /// A suspend was seen, and its lock is not yet made.
    owed: bool,
    /// The monitor is asked to check now.
    woken: bool,
}

impl Watch {
    /// One sample, retaken while its monotonic reads are too far apart.
    fn sample(&mut self) -> Result<Reading, String> {
        for _ in 0..SAMPLE_RETAKES {
            let monotonic = self.clocks.monotonic();
            let boot = self.clocks.boot()?;
            if self.clocks.monotonic().saturating_sub(monotonic) <= SAMPLE_SPREAD {
                return Ok(Reading { monotonic, boot });
            }
        }
        Err(format!("{SAMPLE_RETAKES} samples in a row were discarded"))
    }

    /// Compares a new sample with the baseline. A check that cannot be
    /// verified, the boot-time clock unreadable or every sample discarded,
    /// counts as a suspend once: the baseline is dropped, and the next
    /// accepted sample starts a new one.
    fn check(&mut self) {
        match self.sample() {
            Ok(reading) => {
                if let Some(baseline) = self.baseline {
                    let boot = reading.boot.saturating_sub(baseline.boot);
                    let awake = reading.monotonic.saturating_sub(baseline.monotonic);
                    if boot.saturating_sub(awake) > RESUME_GAP {
                        self.owed = true;
                    }
                }
                self.baseline = Some(reading);
            }
            Err(error) => {
                if self.baseline.take().is_some() {
                    self.owed = true;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "td-compositor: resume check unverifiable, locking: {error}"
                    );
                }
            }
        }
    }
}

/// Resume detection (td-compositor/DESIGN.md item 7), in the paired
/// profile: while root's last `1a` answer is enrolled or unavailable, it
/// compares how far the boot-time clock, which counts suspended time, and
/// the monotonic clock have advanced since the last check, and a gap of
/// more than `RESUME_GAP` locks through `lock_for_suspend`. It checks
/// before each input batch is routed and before a reader's teardown
/// releases, and at least once a second in its monitor; while a lock is
/// owed the runtime withholds every client's paint, focus, input and VM
/// clipboard access. Its own lock is a leaf: nothing is taken while it is
/// held.
pub(crate) struct Resume {
    login: crate::authority::Login,
    watch: Mutex<Watch>,
    wake: Condvar,
}

impl Resume {
    fn new(login: crate::authority::Login, clocks: Box<dyn ResumeClocks>) -> Self {
        Self {
            login,
            watch: Mutex::new(Watch {
                clocks,
                baseline: None,
                owed: false,
                woken: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn watch(&self) -> MutexGuard<'_, Watch> {
        self.watch.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Samples while root's last answer locks, and answers whether a
    /// suspend's lock is owed. Otherwise nothing is sampled or owed, and
    /// the baseline is dropped.
    fn check(&self) -> bool {
        let locks = self
            .login
            .current()
            .as_ref()
            .is_some_and(crate::authority::Answer::locks);
        let mut watch = self.watch();
        if !locks {
            watch.baseline = None;
            watch.owed = false;
            return false;
        }
        watch.check();
        watch.owed
    }

    /// Before an input batch is routed, before a reader's teardown
    /// releases what its device held, and the monitor's tick: a lock owed
    /// is made through `lock_for_suspend` first. It stays owed until the
    /// session is locked, or the last answer no longer locks, so a lock
    /// refused before the lock state was set is retaken by the next gate,
    /// and the runtime withholds every client's paint, focus and input
    /// meanwhile.
    fn gate<T: InputTarget>(
        &self,
        target: &Mutex<T>,
        bindings: &Mutex<KeyBindings>,
    ) -> Result<(), String> {
        if !self.check() {
            return Ok(());
        }
        let mut bindings = bindings
            .lock()
            .map_err(|_| "input bindings lock poisoned".to_string())?;
        // Another gate may have made it while this one waited.
        if !self.watch().owed {
            return Ok(());
        }
        let locked = lock_for_suspend(target, &mut bindings);
        let made = target
            .lock()
            .is_ok_and(|mut target| target.session_locked());
        if made || !bindings.locks() {
            self.watch().owed = false;
        }
        locked
    }

    /// A reader's or the monitor's gate. A lock that fails is reported and
    /// stays owed, and the caller reads on, as the lid's reader does.
    fn guard<T: InputTarget>(&self, target: &Mutex<T>, bindings: &Mutex<KeyBindings>) {
        if let Err(error) = self.gate(target, bindings) {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: resume: {error}");
        }
    }

    /// Before a repaint, focus change, client input or VM clipboard access,
    /// with the runtime held, whether a suspend's lock is owed. If it is,
    /// that work waits and
    /// the monitor is woken to make the lock, which the lock order keeps
    /// from here.
    pub(crate) fn holds(&self) -> bool {
        let owed = self.check();
        if owed {
            self.watch().woken = true;
            self.wake.notify_one();
        }
        owed
    }

    /// Waits out the monitor's period, or until a repaint wakes it.
    fn wait(&self) {
        let watch = self.watch();
        let (mut watch, _) = self
            .wake
            .wait_timeout_while(watch, RESUME_PERIOD, |watch| !watch.woken)
            .unwrap_or_else(PoisonError::into_inner);
        watch.woken = false;
    }

    /// The monitor: a check at once, then one each period and whenever a
    /// repaint wakes it, for the whole generation. It holds the seat
    /// itself, so a repaint's wake is answered and the lock made even once
    /// every reader has ended, as a USB-only seat's may at a resume.
    fn monitor<T: InputTarget>(&self, target: &Mutex<T>, bindings: &Mutex<KeyBindings>) {
        loop {
            self.guard(target, bindings);
            self.wait();
        }
    }
}

/// One synthetic seat for an explicitly enabled headless process generation.
/// Keyboard and pointer use distinct device ids under the shared bindings.
#[derive(Default)]
pub(crate) struct AutomationSeat {
    bindings: KeyBindings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomationPointer {
    pub time: u32,
    pub x: u32,
    pub y: u32,
    pub buttons: u8,
    pub vertical: i32,
    pub horizontal: i32,
}

#[derive(Debug)]
pub(crate) enum AutomationFailure {
    Refused(String),
    Unavailable(String),
}

impl From<String> for AutomationFailure {
    fn from(error: String) -> Self {
        Self::Unavailable(error)
    }
}

impl AutomationPointer {
    pub(crate) fn validate(self, width: usize, height: usize) -> Result<(), String> {
        if usize::try_from(self.x).map_or(true, |x| x >= width)
            || usize::try_from(self.y).map_or(true, |y| y >= height)
        {
            return Err("pointer coordinates outside the output".into());
        }
        if !(-120..=120).contains(&self.vertical) || !(-120..=120).contains(&self.horizontal) {
            return Err("pointer wheel outside -120..=120 detents".into());
        }
        Ok(())
    }
}

impl AutomationSeat {
    const KEYBOARD: usize = 0;
    const POINTER: usize = 1;

    pub(crate) fn key(
        &mut self,
        runtime: &mut Runtime,
        time: u32,
        code: u16,
        pressed: bool,
    ) -> Result<(), String> {
        Self::admit(runtime)?;
        if !(1..=MAX_XKB_EVDEV_KEY).contains(&code) {
            return Err("automation key outside 1..=247".into());
        }
        let decision = self.bindings.feed_device(
            Self::KEYBOARD,
            Event {
                timestamp: u128::from(time) * 1_000_000,
                time,
                kind: EV_KEY,
                code,
                value: if pressed { KEY_PRESS } else { KEY_RELEASE },
            },
        );
        deliver_key_decision(
            &mut AutomationTarget { runtime },
            &mut self.bindings,
            decision,
        )
    }

    pub(crate) fn release_keys(&mut self, runtime: &mut Runtime, time: u32) -> Result<(), String> {
        Self::admit(runtime)?;
        release_device_locked(
            &Mutex::new(AutomationTarget { runtime }),
            Self::KEYBOARD,
            &mut self.bindings,
            time,
        )
    }

    pub(crate) fn pointer(
        &mut self,
        runtime: &mut Runtime,
        report: AutomationPointer,
    ) -> Result<(), AutomationFailure> {
        Self::admit(runtime)?;
        report
            .validate(runtime.width(), runtime.height())
            .map_err(AutomationFailure::Refused)?;
        let width =
            u32::try_from(runtime.width()).map_err(|_| "output width outside u32".to_string())?;
        let height =
            u32::try_from(runtime.height()).map_err(|_| "output height outside u32".to_string())?;
        let mut pressed = BTreeSet::new();
        let mut buttons = Vec::new();
        for code in BTN_MOUSE..=BTN_TASK {
            let bit = 1u8
                .checked_shl(u32::from(code - BTN_MOUSE))
                .ok_or_else(|| "automation button code exceeds its mask".to_string())?;
            let down = report.buttons & bit != 0;
            if down {
                pressed.insert(code);
            }
            if down
                != self
                    .bindings
                    .pointer_pressed
                    .contains(&(Self::POINTER, code))
            {
                buttons.push(PointerButtonTransition {
                    code,
                    pressed: down,
                });
            }
        }
        let frame = PointerFrame {
            time: report.time,
            place: PointerPlace::At {
                x: Fraction {
                    numerator: report.x,
                    denominator: width,
                },
                y: Fraction {
                    numerator: report.y,
                    denominator: height,
                },
            },
            buttons,
            scroll: PointerScroll {
                vertical: report.vertical,
                horizontal: report.horizontal,
            },
        };
        let mut target = AutomationTarget { runtime };
        let delivery = deliver_pointer_frame(
            &mut target,
            &mut self.bindings,
            Self::POINTER,
            &frame,
            &pressed,
        );
        // A control request is a complete batch; settle cursor paint even if
        // another part of delivery failed after changing the scene.
        automation_outcome([delivery, target.flush()]).map_err(AutomationFailure::Unavailable)
    }

    pub(crate) fn release_all(&mut self, runtime: &mut Runtime, time: u32) -> Result<(), String> {
        Self::admit(runtime)?;
        let keyboard = self.release_keys(runtime, time);
        let pointer = release_device_locked(
            &Mutex::new(AutomationTarget { runtime }),
            Self::POINTER,
            &mut self.bindings,
            time,
        );
        automation_outcome([keyboard, pointer, runtime.flush_paint()])
    }

    fn admit(runtime: &Runtime) -> Result<(), String> {
        if runtime.attention_enabled() {
            return Err("input automation refuses a trusted-attention runtime".into());
        }
        Ok(())
    }
}

fn automation_outcome<const N: usize>(outcomes: [Result<(), String>; N]) -> Result<(), String> {
    let mut failure = None;
    for outcome in outcomes {
        if let Err(error) = outcome {
            retain_failure(&mut failure, error);
        }
    }
    failure.map_or(Ok(()), Err)
}

struct AutomationTarget<'a> {
    runtime: &'a mut Runtime,
}

impl InputTarget for AutomationTarget<'_> {
    fn attention(&mut self, _visible: bool) -> Result<u128, String> {
        Err("automation has no physical input origin".into())
    }

    fn drain_attention(&mut self) -> Result<(), String> {
        Err("automation has no physical input origin".into())
    }

    fn command(&mut self, command: Command) -> Result<(), String> {
        self.runtime.command(command)
    }

    fn launcher(&mut self, _action: LauncherAction) -> Result<bool, String> {
        Err("headless keyboard has no launcher".into())
    }

    fn help(&mut self, action: HelpAction) -> Result<bool, String> {
        self.runtime.help(action)
    }

    fn launch(&mut self, _request: LaunchRequest) -> Result<(), String> {
        Err("headless keyboard cannot launch processes".into())
    }

    fn key(&mut self, input: KeyInput) -> Result<(), String> {
        self.runtime.key(input)
    }

    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
        self.runtime.modifiers(modifiers)
    }

    fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String> {
        self.runtime.pointer_frame(time, dx, dy, buttons, scroll)
    }

    fn pointer_frame_at(
        &mut self,
        time: u32,
        x: Fraction,
        y: Fraction,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String> {
        self.runtime.pointer_frame_at(time, x, y, buttons, scroll)
    }

    fn flush(&mut self) -> Result<(), String> {
        self.runtime.flush_paint()
    }
}

impl LiveInputTarget {
    /// A native process-launch failure is reported without retiring evdev.
    /// Application activation mutates the scene, so its paint/focus failures
    /// follow every other scene mutation and propagate to the input reader.
    fn spawn(&mut self, request: LaunchRequest) -> Result<(), String> {
        if request == LaunchRequest::UiDemo && self.launches.activates_application() {
            let activated = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .activate_application()?;
            if !activated {
                eprintln!("td-compositor: configured application has no mapped window");
            }
            return Ok(());
        }
        match self.launches.launch(request) {
            Ok(failures) => {
                for failure in failures {
                    eprintln!("td-compositor: {failure}");
                }
            }
            Err(error) => eprintln!("td-compositor: {error}"),
        }
        Ok(())
    }
}

impl InputTarget for LiveInputTarget {
    fn confirm_install(&mut self, timestamp: u128) -> Result<(), String> {
        if let Some(attempt) = &self.secret_attempt {
            attempt.confirm_install(&EvdevOrigin { _private: () }, timestamp)?;
        }
        Ok(())
    }
    fn approval_digit(&mut self, digit: u8, timestamp: u128) -> Result<bool, String> {
        match &self.secret_attempt {
            Some(attempt) => attempt.approve(&EvdevOrigin { _private: () }, digit, timestamp),
            None => Ok(false),
        }
    }
    fn pin_key(
        &mut self,
        key: crate::secret_client::FieldKey,
        timestamp: u128,
    ) -> Result<(), String> {
        if let Some(attempt) = &self.secret_attempt {
            attempt.field_key(&EvdevOrigin { _private: () }, key, timestamp)?;
        }
        Ok(())
    }
    fn secret_request(&mut self, role: crate::secret_client::Selection) -> Result<(), String> {
        if self.secret_attempt.is_some() {
            return Err("physical attention already consumed a request".into());
        }
        let attempt = crate::secret_client::Attempt::seated(
            EvdevOrigin { _private: () },
            Arc::clone(&self.runtime),
            role,
            Some(self.seat.clone()),
        );
        self.secret_attempt = Some(Arc::clone(&attempt));
        if let Err(error) = attempt.notice(crate::attention::Notice::Pending) {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
            return Ok(());
        }
        if let Err(error) = self.launches.unlock(Arc::clone(&attempt)) {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
            if let Err(error) = attempt.notice(crate::attention::Notice::Failed) {
                let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
            }
        }
        Ok(())
    }

    fn attention_notice(
        &mut self,
        notice: crate::attention::Notice,
    ) -> Option<crate::runtime::NoticePresentation> {
        let shown = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())
            .and_then(|mut runtime| {
                runtime.attention_notice(&EvdevOrigin { _private: () }, notice)
            });
        if let Err(error) = &shown {
            let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
        }
        shown.ok()
    }

    fn attention_closed(&mut self) {
        self.secret_attempt = None;
    }

    fn session_locked(&mut self) -> bool {
        // A poisoned runtime is taken as locked: nothing ordinary runs.
        self.runtime
            .lock()
            .map_or(true, |runtime| runtime.session_locked())
    }

    fn lock_screen(&mut self) -> Result<crate::runtime::NoticePresentation, String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .lock_session(&EvdevOrigin { _private: () })
    }

    fn drain_attention(&mut self) -> Result<(), String> {
        if let Some(attempt) = &self.secret_attempt {
            attempt.cancel();
        }
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .drain_attention(&EvdevOrigin { _private: () })
    }

    fn attention(&mut self, visible: bool) -> Result<u128, String> {
        if !visible {
            if let Some(attempt) = &self.secret_attempt {
                attempt.cancel();
            }
        }
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .attention(&EvdevOrigin { _private: () }, visible)
    }

    fn command(&mut self, command: Command) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .command(command)
    }

    fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
        self.spawn(request)
    }

    fn help(&mut self, action: HelpAction) -> Result<bool, String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .help(action)
    }

    fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
        let (request, visible) = {
            let mut runtime = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?;
            let request = runtime.launcher(action)?;
            (request, runtime.launcher_visible())
        };
        if let Some(request) = request {
            self.spawn(request)?;
        }
        Ok(visible)
    }

    fn pointer_launcher(&mut self, pressed: bool) -> Result<Option<LauncherAction>, String> {
        Ok(self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_launcher(pressed))
    }

    fn key(&mut self, input: KeyInput) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .key(input)
    }

    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .modifiers(modifiers)
    }

    fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_frame(time, dx, dy, buttons, scroll)
    }

    fn pointer_frame_at(
        &mut self,
        time: u32,
        x: Fraction,
        y: Fraction,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
    ) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_frame_at(time, x, y, buttons, scroll)
    }

    fn flush(&mut self) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .flush_paint()
    }
}

impl PointerMotion {
    fn touchpad() -> Self {
        PointerMotion {
            touchpad: Some(TouchTrack::default()),
            ..PointerMotion::default()
        }
    }

    fn feed(&mut self, event: Event, axes: Option<AbsoluteAxes>) -> Option<PointerFrame> {
        match (event.kind, event.code) {
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(event.value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(event.value),
            (EV_REL, REL_WHEEL) => self.wheel = self.wheel.saturating_add(event.value),
            (EV_REL, REL_HWHEEL) => self.hwheel = self.hwheel.saturating_add(event.value),
            // Recorded raw. Whether it MEANS anything is the frame's question,
            // since only a declared range turns a value into a place.
            (EV_ABS, ABS_X) => self.abs_x = Some(event.value),
            (EV_ABS, ABS_Y) => self.abs_y = Some(event.value),
            (EV_KEY, BTN_MOUSE..=BTN_TASK)
                if event.value == KEY_PRESS || event.value == KEY_RELEASE =>
            {
                let pressed = event.value == KEY_PRESS;
                let changed = if pressed {
                    self.pressed.insert(event.code)
                } else {
                    self.pressed.remove(&event.code)
                };
                if changed {
                    if self.buttons.len() >= MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
                        // Through `reset` rather than clearing the fields
                        // here: an overflow abandons the frame, which is what
                        // `reset` means, and a second copy of that list is a
                        // field somebody forgets. The wheel was exactly that
                        // — added to the frame and not to the copy.
                        self.reset();
                        self.overflowed = true;
                    } else {
                        self.buttons.push(PointerButtonTransition {
                            code: event.code,
                            pressed,
                        });
                    }
                }
            }
            (EV_KEY, BTN_TOOL_FINGER..=BTN_TOOL_QUADTAP)
                if event.value == KEY_PRESS || event.value == KEY_RELEASE =>
            {
                if let Some(track) = self.touchpad.as_mut() {
                    track.key(event.code, event.value == KEY_PRESS);
                }
            }
            (EV_SYN, SYN_REPORT) => return self.frame(event.time, axes),
            _ => {}
        }
        None
    }

    /// A device with no absolute axes, which is every test about deltas and
    /// buttons — and the ordinary mouse those tests are written for.
    #[cfg(test)]
    fn feed_relative(&mut self, event: Event) -> Option<PointerFrame> {
        self.feed(event, None)
    }

    /// Abandon the frame being accumulated and every button believed held,
    /// which is what a dropped batch and a button overflow both mean.
    ///
    /// The HELD POSITION survives, deliberately: it is not frame state but the
    /// last thing this device said about where it is, and the alternative on
    /// the next one-axis report is the position the kernel gave at OPEN — a
    /// jump to wherever the device was when the compositor started. The
    /// kernel's advice after `SYN_DROPPED` is to re-query the device, which
    /// this reader cannot do: it takes an `impl Read` so its tests can drive
    /// it from a byte slice, and a slice has no descriptor to ask. Keeping
    /// the last known position is the closest thing available, and it is what
    /// every ordinary report between two frames already relies on.
    ///
    /// A touchpad forgets its contact keys and every named axis value, as
    /// buttons are forgotten: the kernel re-sends neither until it changes,
    /// so either belief could be wrong in both directions.
    fn reset(&mut self) {
        let (held_x, held_y) = (self.held_x, self.held_y);
        let timeline = self.timeline;
        let touchpad = self.touchpad.map(|_| TouchTrack::default());
        *self = PointerMotion::default();
        self.timeline = timeline;
        self.held_x = held_x;
        self.held_y = held_y;
        self.touchpad = touchpad;
    }

    /// Adopt a position the DEVICE reported out of band, which only the
    /// recovery may do: it has just re-read the device, so this is where it
    /// is, and anything remembered from before the gap is a guess.
    fn hold(&mut self, x: i32, y: i32) {
        self.held_x = Some(x);
        self.held_y = Some(y);
    }

    /// Close the frame being accumulated, or answer `None` where it would say
    /// nothing. A position WINS over a delta in the same frame rather than
    /// composing with it: the two are different claims about the same pointer,
    /// and a device that sends both means the place, with the deltas its own
    /// smoothing of the way there. A touchpad's axes are no place, so its
    /// frame is always a distance: finger motion plus any delta.
    ///
    /// An absolute device's frame is a PLACE unless the only thing in it is a
    /// distance. A BUTTON needs somewhere to land as much as a motion does,
    /// and a click that did not move is the ordinary case rather than a corner
    /// one: the kernel drops both axes as unchanged, so a tablet tapped twice
    /// in the same spot sends nothing but `BTN_*`. Read as a zero delta it
    /// would click wherever another device last left the shared cursor.
    fn frame(&mut self, time: u32, axes: Option<AbsoluteAxes>) -> Option<PointerFrame> {
        let buttons = std::mem::take(&mut self.buttons);
        let (dx, dy) = (std::mem::take(&mut self.dx), std::mem::take(&mut self.dy));
        let scroll = PointerScroll {
            vertical: std::mem::take(&mut self.wheel),
            horizontal: std::mem::take(&mut self.hwheel),
        };
        let (raw_x, raw_y) = (self.abs_x.take(), self.abs_y.take());
        let place = match axes {
            Some(axes) if self.touchpad.is_some() => {
                let (x, y) = self
                    .touchpad
                    .as_mut()
                    .map_or((0, 0), |track| track.motion(axes, raw_x, raw_y));
                PointerPlace::By {
                    dx: dx.saturating_add(x),
                    dy: dy.saturating_add(y),
                }
            }
            Some(axes) if raw_x.is_some() || raw_y.is_some() || !buttons.is_empty() => {
                let x = raw_x.or(self.held_x).unwrap_or(axes.x.value);
                let y = raw_y.or(self.held_y).unwrap_or(axes.y.value);
                self.held_x = Some(x);
                self.held_y = Some(y);
                PointerPlace::At {
                    x: AbsoluteAxes::fraction(axes.x, x),
                    y: AbsoluteAxes::fraction(axes.y, y),
                }
            }
            _ => PointerPlace::By { dx, dy },
        };
        // A frame that neither moves the pointer nor changes a button is one
        // the compositor owes nothing for. An absolute frame is never that:
        // the arm above is taken only when an axis reported or a button
        // changed, and the kernel drops an axis whose value did not change.
        let silent = match place {
            PointerPlace::By { dx, dy } => dx == 0 && dy == 0,
            PointerPlace::At { .. } => false,
        };
        // A wheel is the third thing a report can carry and the one with no
        // other trace: a notch moves the pointer nowhere and presses nothing,
        // so a frame asked only about motion and buttons would drop every
        // scroll that arrived without one.
        if silent && buttons.is_empty() && scroll.is_still() {
            return None;
        }
        Some(PointerFrame {
            time,
            place,
            buttons,
            scroll,
        })
    }
}

fn direction(code: u16) -> Option<Direction> {
    match code {
        KEY_LEFT => Some(Direction::Left),
        KEY_RIGHT => Some(Direction::Right),
        KEY_UP => Some(Direction::Up),
        KEY_DOWN => Some(Direction::Down),
        _ => None,
    }
}

fn workspace(code: u16) -> Option<u8> {
    match code {
        KEY_1 => Some(1),
        KEY_2 => Some(2),
        KEY_3 => Some(3),
        KEY_4 => Some(4),
        KEY_5 => Some(5),
        KEY_6 => Some(6),
        KEY_7 => Some(7),
        KEY_8 => Some(8),
        KEY_9 => Some(9),
        _ => None,
    }
}

/// A removal digit's key position, 1 through 8, from the main row.
fn digit(code: u16) -> Option<u8> {
    if !(KEY_1..=KEY_8).contains(&code) {
        return None;
    }
    u8::try_from(code.checked_sub(KEY_1)?.checked_add(1)?).ok()
}

/// The PIN field's byte for `code`, or none for a key that types nothing.
fn pin_byte(code: u16, shift: bool) -> Option<u8> {
    PIN_KEYS
        .iter()
        .find(|(key, ..)| *key == code)
        .map(|(_, plain, shifted)| if shift { *shifted } else { *plain })
}

fn launcher_character(code: u16) -> Option<char> {
    match code {
        KEY_A => Some('a'),
        KEY_B => Some('b'),
        KEY_C => Some('c'),
        KEY_D => Some('d'),
        KEY_E => Some('e'),
        KEY_F => Some('f'),
        KEY_G => Some('g'),
        KEY_H => Some('h'),
        KEY_I => Some('i'),
        KEY_J => Some('j'),
        KEY_K => Some('k'),
        KEY_L => Some('l'),
        KEY_M => Some('m'),
        KEY_N => Some('n'),
        KEY_O => Some('o'),
        KEY_P => Some('p'),
        KEY_Q => Some('q'),
        KEY_R => Some('r'),
        KEY_S => Some('s'),
        KEY_T => Some('t'),
        KEY_U => Some('u'),
        KEY_V => Some('v'),
        KEY_W => Some('w'),
        KEY_X => Some('x'),
        KEY_Y => Some('y'),
        KEY_Z => Some('z'),
        KEY_1 => Some('1'),
        KEY_2 => Some('2'),
        KEY_3 => Some('3'),
        KEY_4 => Some('4'),
        KEY_5 => Some('5'),
        KEY_6 => Some('6'),
        KEY_7 => Some('7'),
        KEY_8 => Some('8'),
        KEY_9 => Some('9'),
        KEY_0 => Some('0'),
        KEY_MINUS => Some('-'),
        KEY_SPACE => Some(' '),
        _ => None,
    }
}

fn read_u16(bytes: &[u8]) -> Result<u16, String> {
    let raw: [u8; 2] = bytes
        .get(..2)
        .ok_or_else(|| "truncated input u16".to_string())?
        .try_into()
        .map_err(|_| "truncated input u16".to_string())?;
    Ok(u16::from_ne_bytes(raw))
}

fn read_i32(bytes: &[u8]) -> Result<i32, String> {
    let raw: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated input i32".to_string())?
        .try_into()
        .map_err(|_| "truncated input i32".to_string())?;
    Ok(i32::from_ne_bytes(raw))
}

fn read_i64(bytes: &[u8]) -> Result<i64, String> {
    let raw: [u8; 8] = bytes
        .get(..8)
        .ok_or_else(|| "truncated input i64".to_string())?
        .try_into()
        .map_err(|_| "truncated input i64".to_string())?;
    Ok(i64::from_ne_bytes(raw))
}

fn event_timestamp(bytes: &[u8]) -> Result<u128, String> {
    let seconds = read_i64(bytes)?;
    let micros = read_i64(
        bytes
            .get(8..16)
            .ok_or_else(|| "input_event lacks microseconds".to_string())?,
    )?;
    if seconds < 0 || !(0..1_000_000).contains(&micros) {
        return Err("invalid input timestamp".to_string());
    }
    Ok((seconds as u128) * 1_000_000_000 + (micros as u128) * 1_000)
}

fn parse(bytes: &[u8]) -> Result<Event, String> {
    if bytes.len() != EVENT_SIZE {
        return Err(format!(
            "input_event is {} bytes, expected {EVENT_SIZE}",
            bytes.len()
        ));
    }
    let timestamp = event_timestamp(bytes)?;
    Ok(Event {
        timestamp,
        time: ((timestamp / 1_000_000) % (u128::from(u32::MAX) + 1)) as u32,
        kind: read_u16(
            bytes
                .get(16..18)
                .ok_or_else(|| "input_event lacks type".to_string())?,
        )?,
        code: read_u16(
            bytes
                .get(18..20)
                .ok_or_else(|| "input_event lacks code".to_string())?,
        )?,
        value: read_i32(
            bytes
                .get(20..24)
                .ok_or_else(|| "input_event lacks value".to_string())?,
        )?,
    })
}

fn event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn event_paths(input_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(input_dir).map_err(|e| format!("read {}: {e}", input_dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read input entry: {e}"))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(event_name) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{} has no event devices", input_dir.display()));
    }
    Ok(paths)
}

// Seat assignment is a boot snapshot; late USB nodes may remain root-only.
// Open the complete admitted roster before starting any input thread.
fn open_event_devices<T>(
    paths: Vec<PathBuf>,
    mut open: impl FnMut(&Path) -> std::io::Result<T>,
) -> Result<Vec<(PathBuf, T)>, String> {
    let mut devices = Vec::with_capacity(paths.len());
    for path in paths {
        match open(&path) {
            Ok(device) => devices.push((path, device)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ) || error.raw_os_error() == Some(19) =>
            {
                eprintln!(
                    "td-compositor: skipping unavailable input {}: {error}",
                    path.display()
                );
            }
            Err(error) => return Err(format!("open input {}: {error}", path.display())),
        }
    }
    if devices.is_empty() {
        return Err("no accessible input devices; seat assignment is required".into());
    }
    Ok(devices)
}

#[cfg(test)]
fn apply<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    pointer: &mut PointerMotion,
    axes: Option<AbsoluteAxes>,
) -> Result<(), String> {
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    if !pointer.accept_event(event, bindings.cutoff, bindings.settled) {
        return Ok(());
    }
    apply_locked(runtime, event, device, &mut bindings, pointer, axes)
}

fn apply_locked<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &mut KeyBindings,
    pointer: &mut PointerMotion,
    axes: Option<AbsoluteAxes>,
) -> Result<(), String> {
    let frame = pointer.feed(event, axes);
    if event.kind == EV_REL
        || event.kind == EV_ABS
        || (event.kind == EV_KEY && (BTN_MOUSE..=BTN_TASK).contains(&event.code))
    {
        bindings.pointer_pending.insert(device);
    }
    if event.kind == EV_SYN && event.code == SYN_REPORT {
        bindings.pointer_pending.remove(&device);
    }
    let mut decision = bindings.feed_device(device, event);
    if frame.is_none() && bindings.attention_can_close() {
        decision.attention = Some(false);
    }
    if !decision.draining
        && !decision.lock
        && decision.attention.is_none()
        && decision.secret.is_none()
        && decision.notice.is_none()
        && decision.confirm_install.is_none()
        // Redundant today, since each approval digit also yields a field
        // byte, but kept so a digit never falls through as nothing.
        && decision.approval.is_none()
        && decision.field.is_none()
        && decision.command.is_none()
        && decision.launcher.is_none()
        && decision.help.is_none()
        && decision.launch.is_none()
        && decision.forward.is_none()
        && decision.modifiers.is_none()
        && frame.is_none()
    {
        return Ok(());
    }
    // Keep seat decisions and delivery in one cross-device order.
    let mut runtime = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?;
    deliver_key_decision(&mut *runtime, bindings, decision)?;
    if let Some(frame) = frame {
        deliver_pointer_frame(&mut *runtime, bindings, device, &frame, &pointer.pressed)?;
    }
    Ok(())
}

/// Put one pointer frame through the button bookkeeping, the overlay filter,
/// and the place or distance itself. Two callers: an ordinary report, and the
/// recovery below, which has a position to publish and no event to hang it on.
fn deliver_pointer_frame<T: InputTarget>(
    runtime: &mut T,
    bindings: &mut KeyBindings,
    device: usize,
    frame: &PointerFrame,
    pressed: &BTreeSet<u16>,
) -> Result<(), String> {
    if bindings.attention != AttentionState::Closed {
        bindings.commit_pointer_device(device, pressed, &[]);
        return finish_attention(runtime, bindings);
    }
    let mut buttons = bindings.pointer_device_changes(device, &frame.buttons, frame.time);
    // The launcher is asked only about THIS device's own presses. The seat's
    // changes are no guide while an overlay withholds presses: those never
    // reach the forwarded set, so another device's release can read there
    // as a press. A report that pressed nothing asks nothing, which also
    // keeps plain motion off a second runtime lock.
    let pressed_here = frame.buttons.iter().any(|button| button.pressed);
    // A left press on the open launcher is the launcher's, answered below
    // once the report has put the pointer where the press was made. Not
    // while the sheet is up: that outranks the launcher, and it is never
    // up beside it anyway.
    let launcher_press = bindings.launcher_open
        && !bindings.help_open
        && frame
            .buttons
            .iter()
            .any(|button| button.pressed && button.code == BTN_LEFT);
    if bindings.launcher_open || bindings.help_open {
        buttons.retain(|button| button.state == PointerButtonState::Released);
    }
    let delivery = match frame.place {
        // The wheel joins this silence test for the reason it joined the
        // reader's: a notch is the one thing in a report that leaves no
        // motion and no button behind, so a scroll delivered here would be
        // dropped by a guard asking only about the other two.
        PointerPlace::By { dx, dy }
            if dx == 0 && dy == 0 && buttons.is_empty() && frame.scroll.is_still() =>
        {
            Ok(())
        }
        PointerPlace::By { dx, dy } => {
            runtime.pointer_frame(frame.time, dx, dy, &buttons, frame.scroll)
        }
        PointerPlace::At { x, y } => {
            runtime.pointer_frame_at(frame.time, x, y, &buttons, frame.scroll)
        }
    };
    bindings.commit_pointer_device(device, pressed, &buttons);
    delivery?;
    // The launcher's own door, the one a key takes, so its capture follows
    // the overlay whichever device opened or closed it.
    if pressed_here {
        if let Some(action) = runtime.pointer_launcher(launcher_press)? {
            let visible = runtime.launcher(action)?;
            bindings.settle_launcher(Some(visible));
        }
    }
    Ok(())
}

fn deliver_key_decision<T: InputTarget>(
    runtime: &mut T,
    bindings: &mut KeyBindings,
    mut decision: KeyDecision,
) -> Result<(), String> {
    if decision.draining {
        runtime.drain_attention()?;
    }
    // The lock surface runs no ordinary binding (td-login/TOKEN-LOGIN.md,
    // "Session lock"); the runtime withholds keys from clients itself.
    let bindings_ordinary = decision.command.is_some()
        || decision.launcher.is_some()
        || decision.help.is_some()
        || decision.launch.is_some();
    if bindings_ordinary && runtime.session_locked() {
        decision.command = None;
        decision.launcher = None;
        decision.help = None;
        decision.launch = None;
        bindings.settle_launcher(Some(false));
        bindings.settle_help(Some(false));
    }
    if decision.lock {
        lock_session(runtime, bindings)?;
    }
    if let Some(visible) = decision.attention {
        if !visible {
            finish_attention(runtime, bindings)?;
        } else if !runtime.session_locked() {
            runtime.attention(true)?;
        } else if decision.unlock {
            // On the lock surface the chord is the selection, with no
            // menu: a login unlock, the lifetime's one operation, while the
            // last `1a` answer is enrolled. Otherwise nothing is sent and
            // the screen says why until Escape.
            runtime.attention(true)?;
            bindings.secret_selected = true;
            match bindings.login.state() {
                Some(crate::authority::LoginState::Enrolled(_)) => {
                    decision.secret = Some(crate::secret_client::Selection::Login(
                        crate::secret_client::LoginSelection::Unlock,
                    ));
                }
                state => decision.notice = Some(no_login_keys(state)),
            }
        } else {
            // A security key's own keyboard selects nothing: its chord
            // leaves the lock surface as it was.
            bindings.attention = AttentionState::Closed;
        }
    }
    if let Some(timestamp) = decision.confirm_install {
        runtime.confirm_install(timestamp)?;
    }
    // A wrong digit ended the request unapproved: the screen drains, as
    // Escape's does.
    if let Some((digit, timestamp)) = decision.approval {
        if runtime.approval_digit(digit, timestamp)? && bindings.attention == AttentionState::Open {
            bindings.attention = AttentionState::Draining;
            runtime.drain_attention()?;
        }
    }
    if let Some((key, timestamp)) = decision.field {
        runtime.pin_key(key, timestamp)?;
    }
    if let Some(notice) = decision.notice {
        // Fails closed: a screen the person cannot see selects nothing more
        // in this lifetime, and capture stays until Escape drains it.
        match runtime.attention_notice(notice) {
            Some(shown) => bindings.login_shown = Some(shown),
            None => bindings.secret_selected = true,
        }
    }
    if let Some(role) = decision.secret {
        runtime.secret_request(role)?;
    }
    if let Some(command) = decision.command {
        runtime.command(command)?;
    }
    if let Some(action) = decision.launcher {
        let visible = runtime.launcher(action)?;
        bindings.settle_launcher(Some(visible));
    }
    if let Some(action) = decision.help {
        let visible = runtime.help(action)?;
        bindings.settle_help(Some(visible));
    }
    if let Some(request) = decision.launch {
        runtime.launch(request)?;
    }
    if let Some(input) = decision.forward {
        runtime.key(input)?;
    }
    if let Some(modifiers) = decision.modifiers {
        runtime.modifiers(modifiers)?;
    }
    Ok(())
}

/// Why a login operation that needs enrolled keys is not sent: unenrolled
/// shows `NO LOGIN KEYS ENROLLED`, unavailable its cause's rows, and no
/// answer at all, which the paired profile never has once connected, `NOT
/// AVAILABLE IN THIS BUILD`.
fn no_login_keys(state: Option<crate::authority::LoginState>) -> crate::attention::Notice {
    use crate::authority::LoginState;
    let rows = match state {
        Some(LoginState::Unenrolled) => crate::secret_client::login_failure(0x09, 0),
        Some(LoginState::Unavailable(cause)) => crate::secret_client::login_failure(cause, 0),
        _ => None,
    };
    rows.map_or(
        crate::attention::Notice::NotAvailable,
        crate::attention::Notice::Login,
    )
}

fn finish_attention<T: InputTarget>(
    runtime: &mut T,
    bindings: &mut KeyBindings,
) -> Result<(), String> {
    if bindings.attention_can_close() {
        let cutoff = runtime.attention(false)?;
        if let Err(mut error) = runtime.modifiers(bindings.modifiers()) {
            for recovery in [
                runtime.attention(true).map(|_| ()),
                runtime.drain_attention(),
            ] {
                if let Err(recovery) = recovery {
                    error.push_str("; trusted-screen recovery: ");
                    error.push_str(&recovery);
                }
            }
            return Err(error);
        }
        bindings.cutoff = Some(cutoff);
        let close = bindings.keyless_close.take();
        bindings.settled = close.map(|_| cutoff.saturating_add(SELF_CLOSE_SETTLE));
        bindings.settle_dropped = close.zip(bindings.settled).map(|(close, settled)| {
            say_line(&format!(
                "{} cutoff={cutoff} settle-until={settled}",
                close.marker()
            ));
            0
        });
        bindings.attention = AttentionState::Closed;
        runtime.attention_closed();
    }
    Ok(())
}

fn retain_failure(failure: &mut Option<String>, error: String) {
    match failure {
        Some(current) => {
            current.push_str("; ");
            current.push_str(&error);
        }
        None => *failure = Some(error),
    }
}

fn deliver_key_cleanup<T: InputTarget>(
    target: &mut T,
    decision: KeyDecision,
    failure: &mut Option<String>,
) {
    if let Some(input) = decision.forward {
        if let Err(error) = target.key(input) {
            retain_failure(failure, error);
        }
    }
    if let Some(modifiers) = decision.modifiers {
        if let Err(error) = target.modifiers(modifiers) {
            retain_failure(failure, error);
        }
    }
}

fn release_device<T: InputTarget>(
    runtime: &Mutex<T>,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    time: u32,
) -> Result<(), String> {
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    release_device_locked(runtime, device, &mut bindings, time)
}

fn release_device_locked<T: InputTarget>(
    runtime: &Mutex<T>,
    device: usize,
    bindings: &mut KeyBindings,
    time: u32,
) -> Result<(), String> {
    let codes: Vec<u16> = bindings
        .pressed
        .iter()
        .filter_map(|(owner, code)| (*owner == device).then_some(*code))
        .collect();
    let had_pointer_state = bindings
        .pointer_pressed
        .iter()
        .any(|(owner, _)| *owner == device);
    if codes.is_empty()
        && !had_pointer_state
        && bindings.pointer_forwarded.is_empty()
        && !bindings.pointer_pending.contains(&device)
    {
        return Ok(());
    }
    let mut failure = None;
    let mut runtime = match runtime.lock() {
        Ok(runtime) => Some(runtime),
        Err(_) => {
            retain_failure(&mut failure, "runtime lock poisoned".to_string());
            None
        }
    };
    for code in codes {
        let decision = bindings.feed_device(
            device,
            Event {
                timestamp: u128::from(time) * 1_000_000,
                time,
                kind: EV_KEY,
                code,
                value: KEY_RELEASE,
            },
        );
        if let Some(target) = runtime.as_deref_mut() {
            deliver_key_cleanup(target, decision, &mut failure);
        }
    }
    // Settle attention after both keyboard and pointer cleanup; a key
    // decision alone cannot account for this device's pending mouse report.
    bindings.remove_pointer_device(device);
    bindings.pointer_pending.remove(&device);
    if bindings.attention != AttentionState::Closed {
        if let Some(target) = runtime.as_deref_mut() {
            if let Err(error) = finish_attention(target, bindings) {
                retain_failure(&mut failure, error);
            }
        }
    }
    let mut buttons = bindings.pointer_changes(time);
    buttons.retain(|button| button.state == PointerButtonState::Released);
    if !buttons.is_empty() {
        let delivery = runtime
            .as_deref_mut()
            .map(|target| target.pointer_frame(time, 0, 0, &buttons, PointerScroll::default()));
        bindings.commit_pointer(&buttons);
        if let Some(Err(error)) = delivery {
            retain_failure(&mut failure, error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// What a reader carries between one device's events: the frame being
/// accumulated, whether a dropped batch is still being discarded, what the
/// device said about its absolute axes, and how to ask it again.
struct DeviceState<'a> {
    attention_enabled: bool,
    pointer: PointerMotion,
    dropped: bool,
    axes: Option<AbsoluteAxes>,
    resync: &'a mut dyn FnMut() -> Option<AbsoluteAxes>,
}

impl DeviceState<'_> {
    fn new(
        axes: Option<AbsoluteAxes>,
        kind: AbsoluteKind,
        resync: &mut dyn FnMut() -> Option<AbsoluteAxes>,
        attention_enabled: bool,
    ) -> DeviceState<'_> {
        DeviceState {
            attention_enabled,
            pointer: match kind {
                AbsoluteKind::Tablet => PointerMotion::default(),
                AbsoluteKind::Touchpad => PointerMotion::touchpad(),
            },
            dropped: false,
            axes,
            resync,
        }
    }

    /// A complete discarded report is the boundary for querying the device.
    /// Quarantine and `SYN_DROPPED` lose changes the kernel does not re-send
    /// an axis it believes unchanged — it compares against the value IT last
    /// emitted, not the one that arrived — so an axis that moved inside the
    /// gap would stay stale until it moved again. Only the device still knows.
    /// A resync that fails leaves the held position standing, which is the
    /// best remaining answer rather than a jump back to the position at open.
    ///
    /// Answers the frame that PUBLISHES the fresh position, because nothing
    /// else will: from here the kernel sends only changes, so a device that
    /// moved during the gap and then stopped would leave the cursor wherever
    /// it was until it happened to move again. Buttonless — the drop already
    /// released everything this device held.
    ///
    /// A touchpad publishes nothing and asks nothing: its position is a
    /// finger, not the pointer, and the snapshot can be newer than reports
    /// still queued, so it is no anchor. The `reset` every discard performs
    /// has already forgotten its contact and axes.
    fn recover(&mut self, time: u32) -> Option<PointerFrame> {
        self.axes?;
        if self.pointer.touchpad.is_some() {
            return None;
        }
        let fresh = (self.resync)()?;
        self.axes = Some(fresh);
        self.pointer.hold(fresh.x.value, fresh.y.value);
        Some(PointerFrame {
            time,
            place: PointerPlace::At {
                x: AbsoluteAxes::fraction(fresh.x, fresh.x.value),
                y: AbsoluteAxes::fraction(fresh.y, fresh.y.value),
            },
            buttons: Vec::new(),
            // A recovery frame reports a PLACE and nothing else. Whatever the
            // wheel did inside the gap is among the reports the kernel
            // dropped, and inventing a notch here would scroll a surface by a
            // distance nobody turned.
            scroll: PointerScroll::default(),
        })
    }
}

fn apply_device_event<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    state: &mut DeviceState<'_>,
) -> Result<(), String> {
    if !state.attention_enabled && !state.dropped && event.kind != EV_KEY && event.kind != EV_SYN {
        state.pointer.feed(event, state.axes);
        return Ok(());
    }
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    let accepted = state
        .pointer
        .accept_event(event, bindings.cutoff, bindings.settled);
    if let Some(line) = bindings.settle_evidence(event, accepted) {
        say_line(&line);
    }
    // Both quarantine and SYN_DROPPED lose changes that evdev need not send
    // again. Recover position only at the report boundary, without buttons.
    if !accepted || state.dropped {
        if event.kind == EV_SYN && event.code == SYN_REPORT {
            state.dropped = false;
            if let Some(frame) = state.recover(event.time) {
                let mut runtime = runtime
                    .lock()
                    .map_err(|_| "runtime lock poisoned".to_string())?;
                deliver_pointer_frame(
                    &mut *runtime,
                    &mut bindings,
                    device,
                    &frame,
                    &state.pointer.pressed,
                )?;
            }
        }
        return Ok(());
    }
    if event.kind == EV_SYN && event.code == SYN_DROPPED {
        state.pointer.reset();
        release_device_locked(runtime, device, &mut bindings, event.time)?;
        state.dropped = true;
        return Ok(());
    }
    apply_locked(
        runtime,
        event,
        device,
        &mut bindings,
        &mut state.pointer,
        state.axes,
    )?;
    if state.pointer.overflowed {
        state.pointer.reset();
        release_device_locked(runtime, device, &mut bindings, event.time)?;
        state.dropped = true;
    }
    Ok(())
}

/// Move the bytes after `consumed` to the front, returning how many were kept.
/// Evdev hands out whole records, so this only ever carries a short tail. Both
/// bounds are clamped so `copy_within` cannot be handed a range that panics.
fn carry_remainder(buffer: &mut [u8], consumed: usize, filled: usize) -> usize {
    let filled = filled.min(buffer.len());
    let consumed = consumed.min(filled);
    let kept = filled.saturating_sub(consumed);
    if kept > 0 && consumed > 0 {
        buffer.copy_within(consumed..filled, 0);
    }
    kept
}

fn flush_target<T: InputTarget>(target: &Mutex<T>) -> Result<(), String> {
    target
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .flush()
}

fn read_device<T: InputTarget>(
    path: &Path,
    file: &mut impl Read,
    device: usize,
    target: &Mutex<T>,
    bindings: &Mutex<KeyBindings>,
    absolute: Option<(AbsoluteAxes, AbsoluteKind)>,
    resync: &mut dyn FnMut() -> Option<AbsoluteAxes>,
) -> Result<(), String> {
    let mut buffer = [0u8; READ_BATCH_BYTES];
    let mut filled = 0usize;
    let axes = absolute.map(|(axes, _)| axes);
    let kind = absolute.map_or(AbsoluteKind::Tablet, |(_, kind)| kind);
    // The boot oracle's only evidence that a real device answered, since the
    // gate machine has none to ask. Printed off the argument `state` is built
    // from rather than beside the `EVIOCGABS` in `start`: being ASKED is not
    // the property, being USED is, and an answer dropped between the two would
    // leave this line printed over a device read as relative. A touchpad is
    // named apart, since its axes are not a place.
    if let (Some(axes), AbsoluteKind::Tablet) = (axes, kind) {
        eprintln!(
            "TD-POINTER-ABSOLUTE device={} x={}..{} y={}..{}",
            path.display(),
            axes.x.minimum,
            axes.x.maximum,
            axes.y.minimum,
            axes.y.maximum
        );
    }
    if let (Some(axes), AbsoluteKind::Touchpad) = (axes, kind) {
        eprintln!(
            "TD-POINTER-TOUCHPAD device={} x={}..{} y={}..{} resolution={}x{}",
            path.display(),
            axes.x.minimum,
            axes.x.maximum,
            axes.y.minimum,
            axes.y.maximum,
            axes.x.resolution,
            axes.y.resolution
        );
    }
    let (attention_enabled, resume) = {
        let bindings = bindings
            .lock()
            .map_err(|_| "input bindings lock poisoned".to_string())?;
        (bindings.attention_enabled, bindings.resume.clone())
    };
    let mut state = DeviceState::new(axes, kind, resync, attention_enabled);
    let mut last_time = 0;
    let result = loop {
        // An empty tail, not just an out-of-range one: `get_mut(len..)` yields
        // `Some(&mut [])`, and reading into that returns `Ok(0)`, which the
        // arm below cannot tell from the device closing. A reader that retired
        // silently is exactly what this refuses to do.
        let tail = match buffer.get_mut(filled..) {
            Some(tail) if !tail.is_empty() => tail,
            _ => break Err(format!("input {} overran its batch buffer", path.display())),
        };
        let read = match file.read(tail) {
            Ok(0) => break Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(format!("read input {}: {error}", path.display())),
        };
        filled = filled.saturating_add(read);
        let records = filled / EVENT_SIZE;
        // Before the batch is routed, so no client receives input after a
        // resume before the lock (td-compositor/DESIGN.md item 7).
        if let (Some(resume), true) = (&resume, records > 0) {
            resume.guard(target, bindings);
        }
        let mut failure = None;
        for index in 0..records {
            let at = index.saturating_mul(EVENT_SIZE);
            let Some(record) = buffer.get(at..at.saturating_add(EVENT_SIZE)) else {
                failure = Some(format!("input {} lost a record", path.display()));
                break;
            };
            let event = match parse(record) {
                Ok(event) => event,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            last_time = event.time;
            if let Err(error) = apply_device_event(target, event, device, bindings, &mut state) {
                failure = Some(error);
                break;
            }
        }
        filled = carry_remainder(&mut buffer, records.saturating_mul(EVENT_SIZE), filled);
        if let Some(error) = failure {
            break Err(error);
        }
        // One paint for the whole batch: while the compositor was painting, the
        // kernel queued these reports, and only the last one is on screen now.
        // A read too short to complete a record owes nothing, so it takes no
        // lock.
        if records > 0 {
            if let Err(error) = flush_target(target) {
                break Err(error);
            }
        }
    };
    // A device lost at a resume, as a USB one re-enumerating is, releases
    // what it held only once the lock is made, so a held button's release
    // cannot complete a click on a client after a suspend.
    if let Some(resume) = &resume {
        resume.guard(target, bindings);
    }
    // Both run: a release that failed is the case where the screen is most
    // likely stale, so it must not be the case that skips the final paint.
    let cleanup = match (
        release_device(target, device, bindings, last_time),
        flush_target(target),
    ) {
        (Ok(()), flushed) => flushed,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(flush_error)) => Err(format!("{error}; final paint: {flush_error}")),
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            Err(format!("{error}; release input state: {cleanup_error}"))
        }
    }
}

/// Ask a device whether it reports an absolute position, and over what span.
///
/// Asked at open, and again only at a recovery — after a dropped batch or a
/// button overflow, which discard alike. The SPAN is a property
/// of the device, so asking per frame would be a syscall per motion for an
/// answer that cannot change; the `value` beside it is not, and a
/// discarded report is the one moment it can have moved without a report saying
/// so. Both callers hand it a real `File`, which `read_device` cannot: it
/// takes an `impl Read` so its tests can drive it from a byte slice, and a
/// slice has no descriptor to ask.
///
/// A device that refuses either axis is RELATIVE, not broken: an evdev node
/// with no absinfo table at all answers `EINVAL`, which is what an ordinary
/// mouse is and not worth a diagnostic. One that HAS the table answers for
/// every axis, zeroed where the device has none — so the refusal is not the
/// whole test, and `declared`'s span is what actually separates the two.
fn absolute_axes(device: &File) -> Option<AbsoluteAxes> {
    let x = sys::absolute_info(device, sys::AbsAxis::X).ok()?;
    let y = sys::absolute_info(device, sys::AbsAxis::Y).ok()?;
    AbsoluteAxes::declared(x, y)
}

/// The roster indices secure attention never selects or confirms with, each
/// named once.
fn attention_exclusions<T>(sysfs: &Path, devices: &[(PathBuf, T)]) -> BTreeSet<usize> {
    let mut excluded = BTreeSet::new();
    for (device, (path, _)) in devices.iter().enumerate() {
        if security_key_keyboard(sysfs, path) {
            eprintln!(
                "td-compositor: {} shares a USB device with a FIDO interface; \
                 secure attention never selects or confirms with it",
                path.display()
            );
            excluded.insert(device);
        }
    }
    excluded
}

pub fn start(
    input_dir: &Path,
    runtime: Arc<Mutex<Runtime>>,
    launches: LaunchBackend,
) -> Result<usize, String> {
    let attention_enabled = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .attention_enabled();
    let (login, connected) = match &launches {
        LaunchBackend::Authority(authority) => (authority.login(), authority.connected()),
        LaunchBackend::Direct(_) => (crate::authority::Login::default(), None),
    };
    let lid = lid_admitted(attention_enabled, connected.as_ref());
    let (paths, lids) = admit_lids(event_paths(input_dir)?, Path::new(SYSFS_INPUT), lid);
    let devices = open_event_devices(paths, |path| File::open(path))?;
    let lids = open_lids(lids);
    let count = devices.len().saturating_add(lids.len());
    // Decided once against the fixed roster, as admission is.
    let attention_excluded = if attention_enabled {
        attention_exclusions(Path::new(SYSFS_INPUT), &devices)
    } else {
        BTreeSet::new()
    };
    let resume = attention_enabled.then(|| {
        Arc::new(Resume::new(
            login.clone(),
            Box::new(SystemClocks {
                origin: Instant::now(),
                uptime: None,
            }),
        ))
    });
    if let Some(resume) = &resume {
        runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .hold_for_resume(Arc::clone(resume));
    }
    let bindings = Arc::new(Mutex::new(KeyBindings {
        attention_enabled,
        attention_excluded,
        login,
        resume: resume.clone(),
        ..KeyBindings::default()
    }));
    let target = Arc::new_cyclic(|own| {
        Mutex::new(LiveInputTarget {
            runtime,
            launches,
            secret_attempt: None,
            seat: Seat {
                bindings: Arc::downgrade(&bindings),
                target: own.clone(),
            },
        })
    });
    for (device, (path, mut file)) in devices.into_iter().enumerate() {
        if attention_enabled {
            sys::input_monotonic_clock(&file)?;
        }
        let axes = absolute_axes(&file);
        // Only a device with axes is classified; a mouse reads no sysfs.
        // `path` is under `/dev/input`, whose node names are sysfs's.
        let absolute = axes.map(|axes| (axes, absolute_kind(Path::new(SYSFS_INPUT), &path)));
        // A second handle purely so a dropped batch can ask the device where
        // it is now. The reader takes an `impl Read` so its tests can drive it
        // from a byte slice, and a slice has no descriptor to ask.
        //
        // `try_clone` rather than opening the node again: it is `dup(2)`, so
        // both handles are the same open file and the same evdev CLIENT. A
        // second `open` would make a second client with a buffer of its own,
        // and the kernel writes every event to every client — so the reader
        // would be racing a queue nothing drains, which is what produces the
        // dropped batches this exists to recover from.
        // A touchpad's recovery asks nothing, so only a tablet keeps one.
        let tablet = absolute.filter(|(_, kind)| *kind == AbsoluteKind::Tablet);
        let resync_handle = tablet.and_then(|_| match file.try_clone() {
            Ok(handle) => Some(handle),
            // Reported rather than swallowed: the device still works, but a
            // dropped batch can no longer be recovered from, and every other
            // failure on this path says so.
            Err(error) => {
                eprintln!(
                    "td-compositor: no resync handle for {}: {error}",
                    path.display()
                );
                None
            }
        });
        let path = path.clone();
        let label = path.display().to_string();
        let target = Arc::clone(&target);
        let bindings = Arc::clone(&bindings);
        thread::Builder::new()
            .name(format!(
                "input-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("event")
            ))
            .spawn(move || {
                let mut resync = move || resync_handle.as_ref().and_then(absolute_axes);
                if let Err(error) = read_device(
                    &path,
                    &mut file,
                    device,
                    target.as_ref(),
                    bindings.as_ref(),
                    absolute,
                    &mut resync,
                ) {
                    eprintln!("td-compositor: {error}");
                }
            })
            .map_err(|e| format!("spawn input reader for {label}: {e}"))?;
    }
    for (path, mut file) in lids {
        let label = path.display().to_string();
        let target = Arc::clone(&target);
        let bindings = Arc::clone(&bindings);
        thread::Builder::new()
            .name("input-lid".into())
            .spawn(move || {
                let read = read_lid(&path, &mut file, target.as_ref(), bindings.as_ref());
                if let Err(error) = read {
                    let _ = writeln!(std::io::stderr().lock(), "td-compositor: {error}");
                }
            })
            .map_err(|e| format!("spawn lid reader for {label}: {e}"))?;
    }
    if let Some(resume) = resume {
        thread::Builder::new()
            .name("resume".into())
            .spawn(move || resume.monitor(target.as_ref(), bindings.as_ref()))
            .map_err(|e| format!("spawn resume monitor: {e}"))?;
    }
    Ok(count)
}

/// Opens the admitted lid switches. One that cannot be opened is reported
/// and left out, as an unavailable input is: resume detection still sees
/// the suspend a close would have preceded.
fn open_lids(paths: Vec<PathBuf>) -> Vec<(PathBuf, File)> {
    let mut lids = Vec::with_capacity(paths.len());
    for path in paths {
        match File::open(&path) {
            Ok(file) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "td-compositor: {} is a lid switch; closing it locks",
                    path.display()
                );
                lids.push((path, file));
            }
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "td-compositor: skipping lid switch {}: {error}",
                    path.display()
                );
            }
        }
    }
    lids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn late_unassigned_or_removed_input_does_not_discard_the_seat() {
        let paths = (0..4)
            .map(|n| PathBuf::from(format!("/dev/input/event{n}")))
            .collect();
        let mut attempt = 0;
        let devices = open_event_devices(paths, |_| {
            attempt += 1;
            match attempt {
                1 => Ok("assigned keyboard"),
                2 => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                3 => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                _ => Err(std::io::Error::from_raw_os_error(19)),
            }
        })
        .unwrap();
        assert_eq!(attempt, 4);
        assert_eq!(
            devices,
            [(PathBuf::from("/dev/input/event0"), "assigned keyboard")]
        );
        for code in [2, 13, 19, 5] {
            assert!(
                open_event_devices::<()>(vec!["/dev/input/event0".into()], |_| Err(
                    std::io::Error::from_raw_os_error(code)
                ))
                .is_err()
            );
        }
        assert!(open_event_devices::<()>(Vec::new(), |_| Ok(())).is_err());
        let mut attempt = 0;
        assert!(open_event_devices(vec!["one".into(), "two".into()], |_| {
            attempt += 1;
            if attempt == 1 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(5))
            }
        })
        .is_err());
    }

    fn key(code: u16, value: i32) -> Event {
        Event {
            timestamp: 0,
            time: 0,
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn automation_runtime() -> (Cleanup, Runtime) {
        let cleanup = Cleanup(std::env::temp_dir().join(format!(
            "td-automation-input-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed),
        )));
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 320, 200, 320 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        runtime
            .commit(
                crate::scene::SurfaceKey {
                    client: 1,
                    object: 1,
                },
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [1, 2, 3, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .unwrap(),
            )
            .unwrap();
        (cleanup, runtime)
    }

    fn pointer_report(x: u32, y: u32, buttons: u8) -> AutomationPointer {
        AutomationPointer {
            time: 10,
            x,
            y,
            buttons,
            vertical: 0,
            horizontal: 0,
        }
    }

    #[test]
    fn automation_pointer_reports_share_grabs_wheels_and_explicit_cleanup() {
        use crate::pointer::{PointerAxis, PointerEvent};
        use crate::runtime::KeyboardDelivery;
        let (_cleanup, mut runtime) = automation_runtime();
        let active = || Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (events, _stop) = runtime
            .subscribe_input_with_activity(1, active(), active())
            .unwrap()
            .split();
        let mut seat = AutomationSeat::default();
        runtime.take_writes();
        seat.pointer(&mut runtime, pointer_report(70, 90, 0))
            .unwrap();
        assert!(
            !runtime.take_writes().is_empty(),
            "cursor paint was left pending"
        );
        let target = runtime.pointer_snapshot().focus.unwrap();
        assert_eq!(
            target.surface,
            crate::scene::SurfaceKey {
                client: 1,
                object: 1
            }
        );
        let mut report = pointer_report(75, 95, 1);
        report.vertical = 2;
        report.horizontal = -3;
        seat.pointer(&mut runtime, report).unwrap();
        let focused = runtime.pointer_snapshot().focus.unwrap();
        assert_eq!((focused.x - target.x, focused.y - target.y), (5, 5));
        let pointer_events = |events: &std::sync::mpsc::Receiver<KeyboardDelivery>| {
            events
                .try_iter()
                .filter_map(|delivery| match delivery {
                    KeyboardDelivery::Pointer(frame) => Some(frame.events),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>()
        };
        let delivered = pointer_events(&events);
        assert_eq!(
            delivered
                .iter()
                .filter(|event| matches!(event,
            PointerEvent::Button { input, .. } if input.state == PointerButtonState::Pressed
                && input.button == 272 && input.time == 10))
                .count(),
            1
        );
        for (axis, detents) in [(PointerAxis::Vertical, -2), (PointerAxis::Horizontal, -3)] {
            assert!(delivered.iter().any(|event| matches!(event,
                PointerEvent::Axis { step, time: 10, .. }
                    if step.axis == axis && step.detents == detents)));
        }
        seat.pointer(&mut runtime, pointer_report(75, 95, 1))
            .unwrap();
        assert!(
            pointer_events(&events).is_empty(),
            "unchanged held mask pressed twice"
        );
        // The client grab keeps motion and release even over compositor chrome.
        seat.pointer(&mut runtime, pointer_report(3, 3, 1)).unwrap();
        assert_eq!(
            runtime.pointer_snapshot().focus.unwrap().surface,
            target.surface
        );
        seat.key(&mut runtime, 11, KEY_LEFTSHIFT, true).unwrap();
        seat.release_keys(&mut runtime, 12).unwrap();
        assert!(seat.bindings.pointer_forwarded.contains(&272));
        assert!(!pointer_events(&events).iter().any(|event| matches!(event,
            PointerEvent::Button { input, .. } if input.state == PointerButtonState::Released)));
        seat.release_all(&mut runtime, 13).unwrap();
        assert!(seat.bindings.pointer_pressed.is_empty());
        assert!(seat.bindings.pointer_forwarded.is_empty());
        let released = pointer_events(&events);
        assert_eq!(
            released
                .iter()
                .filter(|event| matches!(event,
            PointerEvent::Button { input, .. } if input.state == PointerButtonState::Released
                && input.button == 272 && input.time == 13))
                .count(),
            1
        );
        seat.release_all(&mut runtime, 14).unwrap();
        assert!(pointer_events(&events).is_empty());
    }

    #[test]
    fn automation_pointer_refuses_bounds_and_attention_without_mutation() {
        let (_cleanup, mut runtime) = automation_runtime();
        let mut seat = AutomationSeat::default();
        seat.pointer(&mut runtime, pointer_report(70, 90, 1))
            .unwrap();
        let before = runtime.pointer_snapshot();
        let owned = seat.bindings.pointer_pressed.clone();
        for report in [
            pointer_report(320, 0, 0),
            pointer_report(0, 200, 0),
            pointer_report(u32::MAX, 0, 0),
            AutomationPointer {
                vertical: i32::MIN,
                ..pointer_report(70, 90, 0)
            },
            AutomationPointer {
                horizontal: 121,
                ..pointer_report(70, 90, 0)
            },
        ] {
            assert!(seat.pointer(&mut runtime, report).is_err());
            assert_eq!(runtime.pointer_snapshot(), before);
            assert_eq!(seat.bindings.pointer_pressed, owned);
        }
        runtime.enable_attention(true);
        assert!(seat.pointer(&mut runtime, pointer_report(3, 3, 0)).is_err());
        assert!(seat.release_all(&mut runtime, 20).is_err());
        assert_eq!(runtime.pointer_snapshot(), before);
        assert_eq!(seat.bindings.pointer_pressed, owned);
        runtime.enable_attention(false);
        seat.release_all(&mut runtime, 21).unwrap();
        for (x, y) in [(0, 0), (319, 0), (0, 199), (319, 199)] {
            seat.pointer(&mut runtime, pointer_report(x, y, 0)).unwrap();
        }
    }

    #[test]
    fn automation_pointer_failure_retains_cleanup_and_all_eight_button_owners() {
        let (_cleanup, mut runtime) = automation_runtime();
        let mut seat = AutomationSeat::default();
        runtime.fail_next_repaint();
        assert!(seat
            .pointer(&mut runtime, pointer_report(70, 90, 255))
            .is_err());
        assert_eq!(seat.bindings.pointer_pressed.len(), 8);
        assert_eq!(seat.bindings.pointer_forwarded.len(), 8);
        runtime.clear_repaint_failure();
        seat.release_all(&mut runtime, 11).unwrap();
        assert!(seat.bindings.pointer_pressed.is_empty());
        assert!(seat.bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn automation_delivers_normal_keys_once_and_releases_its_depressed_state() {
        use crate::keyboard::KeyboardEvent;
        use crate::runtime::KeyboardDelivery;
        let (_cleanup, mut runtime) = automation_runtime();
        let (events, _stop) = runtime.subscribe_keyboard(1).unwrap().split();
        let mut keys = AutomationSeat::default();
        keys.key(&mut runtime, 10, KEY_LEFTSHIFT, true).unwrap();
        keys.key(&mut runtime, 11, KEY_A, true).unwrap();
        keys.key(&mut runtime, 12, KEY_A, true).unwrap();
        assert_eq!(runtime.keyboard_snapshot().keys, [30, 42]);
        assert_eq!(runtime.keyboard_snapshot().modifiers.depressed, MOD_SHIFT);
        let delivered: Vec<_> = events
            .try_iter()
            .filter_map(|delivery| match delivery {
                KeyboardDelivery::Event(event) => Some(event.event),
                _ => None,
            })
            .collect();
        let pressed: Vec<_> = delivered
            .iter()
            .filter_map(|event| match event {
                KeyboardEvent::Key { input, .. } => Some((input.time, input.key, input.state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pressed,
            [(10, 42, KeyState::Pressed), (11, 30, KeyState::Pressed)]
        );
        assert!(delivered.iter().any(|event| matches!(event,
            KeyboardEvent::Modifiers { state, .. } if state.depressed == MOD_SHIFT)));
        keys.release_keys(&mut runtime, 13).unwrap();
        assert!(runtime.keyboard_snapshot().keys.is_empty());
        assert_eq!(runtime.keyboard_snapshot().modifiers.depressed, 0);
        let released: Vec<_> = events
            .try_iter()
            .filter_map(|delivery| match delivery {
                KeyboardDelivery::Event(event) => match event.event {
                    KeyboardEvent::Key { input, .. } => Some(input),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(released.len(), 2);
        assert!(released
            .iter()
            .all(|input| input.time == 13 && input.state == KeyState::Released));
        keys.release_keys(&mut runtime, 14).unwrap();
        keys.key(&mut runtime, 15, KEY_A, false).unwrap();
        assert!(events.try_recv().is_err());
        keys.key(&mut runtime, 16, KEY_CAPSLOCK, true).unwrap();
        keys.release_keys(&mut runtime, 17).unwrap();
        assert_eq!(runtime.keyboard_snapshot().modifiers.locked, MOD_CAPS);
        assert!(runtime.keyboard_snapshot().keys.is_empty());
    }

    #[test]
    fn automation_cannot_mint_attention_and_refuses_trusted_runtime_before_mutation() {
        let (_cleanup, mut runtime) = automation_runtime();
        let mut keys = AutomationSeat::default();
        for code in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_ESC] {
            keys.key(&mut runtime, 1, code, true).unwrap();
        }
        assert!(runtime
            .keyboard_snapshot()
            .keys
            .contains(&u32::from(KEY_ESC)));
        assert!(runtime.keyboard_snapshot().focus.is_some());
        assert!(keys.bindings.attention == AttentionState::Closed);
        keys.release_keys(&mut runtime, 2).unwrap();
        runtime.enable_attention(true);
        let before = runtime.keyboard_snapshot();
        assert!(keys.key(&mut runtime, 3, KEY_A, true).is_err());
        assert!(keys.release_keys(&mut runtime, 3).is_err());
        assert_eq!(runtime.keyboard_snapshot(), before);
        assert!(keys.bindings.pressed.is_empty());
        runtime.enable_attention(false);
        for code in [0, 248, u16::MAX] {
            assert!(keys.key(&mut runtime, 4, code, true).is_err());
        }
        assert_eq!(runtime.keyboard_snapshot(), before);
        let mut target = AutomationTarget {
            runtime: &mut runtime,
        };
        assert!(target.attention(true).is_err());
        assert!(target.drain_attention().is_err());
        let source = include_str!("input.rs")
            .split_once("impl AutomationSeat {")
            .unwrap()
            .1
            .split_once("impl LiveInputTarget {")
            .unwrap()
            .0;
        for forbidden in [
            "EvdevOrigin",
            "sys::",
            "Command::new",
            "enable_attention(",
            ".attention(",
            // No injected digit reaches an approval key.
            "approv",
        ] {
            assert!(!source.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn automation_uses_compositor_bindings_and_disabled_control_cannot_route_it() {
        let (_cleanup, mut runtime) = automation_runtime();
        let mut keys = AutomationSeat::default();
        keys.key(&mut runtime, 1, KEY_LEFTMETA, true).unwrap();
        keys.key(&mut runtime, 2, KEY_2, true).unwrap();
        assert_eq!(runtime.control_snapshot().active_workspace, 2);
        assert!(!runtime.keyboard_snapshot().keys.contains(&u32::from(KEY_2)));
        keys.release_keys(&mut runtime, 3).unwrap();
        keys.key(&mut runtime, 4, KEY_1, true).unwrap();
        assert_eq!(runtime.control_snapshot().active_workspace, 2);
        keys.release_keys(&mut runtime, 5).unwrap();
        keys.key(&mut runtime, 6, KEY_LEFTMETA, true).unwrap();
        assert!(keys.key(&mut runtime, 7, KEY_T, true).is_err());
        assert!(keys.key(&mut runtime, 8, KEY_ENTER, true).is_err());
        assert!(!runtime.launcher_visible());
        keys.release_keys(&mut runtime, 9).unwrap();
        let before = runtime.keyboard_snapshot();
        let runtime = Mutex::new(runtime);
        for request in [
            "key 00000000000000000000000000000007 0 30 down",
            "release-keys 00000000000000000000000000000007 1",
        ] {
            assert_eq!(
                crate::control::answer(&runtime, request),
                "error input automation is disabled\n"
            );
        }
        assert_eq!(runtime.lock().unwrap().keyboard_snapshot(), before);
    }

    /// One axis of QEMU's `virtio-tablet-pci`, which reports 0..=32767 and is
    /// the device the image attaches. `value` is where the kernel says it is
    /// now.
    fn axis(value: i32, minimum: i32, maximum: i32) -> sys::AbsInfo {
        sys::AbsInfo {
            value,
            minimum,
            maximum,
            resolution: 0,
        }
    }

    fn tablet() -> AbsoluteAxes {
        AbsoluteAxes {
            x: axis(0, 0, 32767),
            y: axis(0, 0, 32767),
        }
    }

    /// The ratio a place crosses as. Written out at every assertion rather
    /// than reduced, because the whole claim is that nothing rescales it.
    fn over(numerator: u32, denominator: u32) -> Fraction {
        Fraction {
            numerator,
            denominator,
        }
    }

    fn abs(time: u32, code: u16, value: i32) -> Event {
        Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_ABS,
            code,
            value,
        }
    }

    fn syn(time: u32) -> Event {
        Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }
    }

    #[test]
    fn an_absolute_report_names_a_place_and_a_relative_one_a_distance() {
        let even = Some(AbsoluteAxes {
            x: axis(0, 0, 1000),
            y: axis(0, 0, 1000),
        });
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed(abs(1, ABS_X, 500), even), None);
        assert_eq!(pointer.feed(abs(1, ABS_Y, 0), even), None);
        let frame = pointer.feed(syn(1), even).unwrap();
        // Halfway along, and hard against the near edge — as the device's own
        // numbers, neither reduced nor rescaled.
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(500, 1000),
                y: over(0, 1000)
            }
        );

        // The far edge is reported EXACTLY, which is the whole complaint an
        // absolute pointer answers: a relative one on a warped host cursor
        // cannot be relied on to arrive at the last column.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed(abs(2, ABS_X, 32767), axes), None);
        assert_eq!(pointer.feed(abs(2, ABS_Y, 32767), axes), None);
        let frame = pointer.feed(syn(2), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(32767, 32767),
                y: over(32767, 32767)
            }
        );

        // The SAME events on a device that declared no range are not a
        // position at all: without a span there is nothing to scale against,
        // and a raw 32767 read as a pixel is thousands of columns off screen.
        let mut relative = PointerMotion::default();
        assert_eq!(relative.feed(abs(3, ABS_X, 32767), None), None);
        assert_eq!(
            relative.feed(syn(3), None),
            None,
            "an absolute report moved a device with no absolute axes"
        );
    }

    #[test]
    fn an_axis_a_report_leaves_out_is_where_that_device_last_was() {
        // The kernel drops an axis whose value has not changed, so a stylus
        // moved along one axis reports only that one. Answering the other from
        // the CURSOR would be wrong the moment anything else moved it, which is
        // why the reader holds the device's own position.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(8000, 32767),
                y: over(4000, 32767)
            }
        );
        // X alone, and the row is still the one the stylus is on.
        pointer.feed(abs(2, ABS_X, 9000), axes);
        assert_eq!(
            pointer.feed(syn(2), axes).unwrap().place,
            PointerPlace::At {
                x: over(9000, 32767),
                y: over(4000, 32767)
            }
        );
        // Y alone, likewise.
        pointer.feed(abs(3, ABS_Y, 5000), axes);
        assert_eq!(
            pointer.feed(syn(3), axes).unwrap().place,
            PointerPlace::At {
                x: over(9000, 32767),
                y: over(5000, 32767)
            }
        );
    }

    #[test]
    fn a_first_report_completes_itself_from_the_position_the_kernel_gave() {
        // Before any report there is no held position, and `input_absinfo`'s
        // `value` is the only account of where the device is. Without it a
        // one-axis first report would place the other axis at the near edge,
        // which is a corner of the screen the stylus is not in.
        let axes = Some(AbsoluteAxes {
            x: axis(1000, 0, 32767),
            y: axis(24000, 0, 32767),
        });
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 16000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(16000, 32767),
                y: over(24000, 32767)
            }
        );

        // The other way round, because the two arms are separate lines and
        // one reading the OTHER axis's `value` would survive the case above.
        // The seeds differ from each other and from both reported values.
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_Y, 16000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(1000, 32767),
                y: over(16000, 32767)
            }
        );

        // And a device whose FIRST frame is a button alone — a stylus already
        // resting where it was left, tapped without moving.
        let mut pointer = PointerMotion::default();
        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(1000, 32767),
                y: over(24000, 32767)
            }
        );
    }

    /// `absolute_axes` needs a real descriptor, so the gate can only ask it
    /// about something that is not an evdev device. What that excludes is a
    /// failed ioctl read as a POSITIVE span; a zeroed one `declared` refuses
    /// anyway, so this is the outer half of a defence whose inner half is
    /// `a_device_whose_axes_declare_no_span_is_relative`.
    #[test]
    fn a_file_that_is_not_an_evdev_device_declares_no_absolute_axes() {
        let file = File::open("/dev/null").unwrap();
        assert_eq!(absolute_axes(&file), None);
    }

    #[test]
    fn a_position_wins_over_a_delta_in_the_same_report() {
        // A device that sends both means the PLACE; the deltas are its own
        // account of the way there, and adding them would move the pointer
        // twice for one report.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(
            Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_REL,
                code: REL_X,
                value: 40,
            },
            axes,
        );
        pointer.feed(abs(4, ABS_X, 8192), axes);
        let frame = pointer.feed(syn(4), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(8192, 32767),
                y: over(0, 32767)
            }
        );
    }

    #[test]
    fn an_absolute_value_outside_the_declared_range_is_the_edge_it_went_past() {
        let axes = tablet();
        assert_eq!(AbsoluteAxes::fraction(axes.x, -5), over(0, 32767));
        assert_eq!(AbsoluteAxes::fraction(axes.x, 999_999), over(32767, 32767));
        // A span of nothing cannot be scaled against, so it answers a zero
        // DENOMINATOR — which `across` reads as the near edge rather than
        // dividing by it. `declared` refuses such a device outright.
        assert_eq!(AbsoluteAxes::fraction(axis(7, 7, 7), 7).denominator, 0);
        assert_eq!(AbsoluteAxes::fraction(axis(5, 9, 1), 5).denominator, 0);
    }

    #[test]
    fn a_range_that_does_not_start_at_zero_is_read_from_its_own_base() {
        // A device may report over a window starting anywhere, and taking the
        // value as it stands would offset every report by that base.
        let shifted = axis(1000, 1000, 3000);
        assert_eq!(AbsoluteAxes::fraction(shifted, 1000), over(0, 2000));
        assert_eq!(AbsoluteAxes::fraction(shifted, 2000), over(1000, 2000));
        assert_eq!(AbsoluteAxes::fraction(shifted, 3000), over(2000, 2000));
        // Below the base is the near edge rather than a wrap.
        assert_eq!(AbsoluteAxes::fraction(shifted, 0), over(0, 2000));
    }

    #[test]
    fn each_axis_is_scaled_against_its_own_declared_range() {
        // The two rarely share a range, and one report scaled against the
        // other axis is a well-formed position somewhere else entirely.
        let axes = Some(AbsoluteAxes {
            x: axis(0, 0, 1000),
            y: axis(0, 0, 4000),
        });
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(5, ABS_X, 500), axes);
        pointer.feed(abs(5, ABS_Y, 500), axes);
        let frame = pointer.feed(syn(5), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(500, 1000),
                y: over(500, 4000)
            }
        );
    }

    #[test]
    fn a_device_whose_axes_declare_no_span_is_relative() {
        // The admission asks `fraction`, so a device it lets in is one every
        // later report can actually be placed against.
        let flat = axis(7, 7, 7);
        let real = axis(0, 0, 32767);
        assert_eq!(AbsoluteAxes::declared(flat, real), None);
        assert_eq!(AbsoluteAxes::declared(real, flat), None);
        assert_eq!(AbsoluteAxes::declared(real, real), Some(tablet()));
    }

    #[test]
    fn a_button_alone_from_an_absolute_device_lands_where_that_device_is() {
        // Tapping twice in one spot sends NOTHING but the button: the kernel
        // drops both axes as unchanged. Read as a zero delta, the second tap
        // would click wherever another device last left the shared cursor.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        pointer.feed(syn(1), axes);

        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        let frame = pointer.feed(syn(2), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(8000, 32767),
                y: over(4000, 32767)
            },
            "a button-only frame was not placed where the device is"
        );
        assert_eq!(frame.buttons.len(), 1);

        // The same frame from a RELATIVE device is still a zero delta: there
        // is no position to place it at, and the shared cursor is the answer.
        let mut mouse = PointerMotion::default();
        mouse.feed_relative(key(BTN_MOUSE, KEY_PRESS));
        assert_eq!(
            mouse.feed_relative(syn(2)).unwrap().place,
            PointerPlace::By { dx: 0, dy: 0 }
        );
    }

    #[test]
    fn a_recovery_asks_the_device_once_and_only_where_the_kernel_says_to() {
        // Three properties one batch can prove, and each is a mutation the
        // rest of the suite cannot see: a SECOND `SYN_DROPPED` must not end
        // the discard window (nor may any other EV_SYN code), the device is
        // asked exactly once per recovery rather than per report, and the
        // frame that publishes the answer carries the recovery's own time.
        let drop = |time| Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        };
        let mut data = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            drop(2),
            // Inside the window: neither ends it, and the recovery below must
            // still be the FIRST place anything is published.
            drop(3),
            Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_SYN,
                code: 1,
                value: 0,
            },
            abs(5, ABS_X, 999),
            syn(6),
            // After it: an ordinary report, which must ask nothing.
            abs(7, ABS_X, 7000),
            syn(7),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };
        let (target, result, asked) =
            drain_counting(data.clone(), Vec::new(), Some(tablet()), Some(moved), None);
        assert_eq!(result, Ok(()));
        assert_eq!(asked, 1, "the device was asked {asked} times, not once");
        assert_eq!(
            target.pointer_places,
            [
                (1, over(100, 32767), over(100, 32767), Vec::new()),
                // The recovery, at the time of the SYN_REPORT that ended the
                // window — 6, not 3 or 4, which is what an over-eager guard
                // would answer.
                (6, over(20000, 32767), over(30000, 40000), Vec::new()),
                // And on afterwards, the row still the one the resync gave.
                (7, over(7000, 32767), over(30000, 40000), Vec::new()),
            ]
        );

        // A refused delivery on the recovery path is the reader's failure,
        // not something it carries on past. The batch starts AT the drop so
        // the recovery frame is the first delivery there is: with a report
        // before it, that one would consume the injected error and the
        // assertion would hold whatever the recovery did with its own.
        let mut alone = Vec::new();
        for event in [drop(2), syn(6)] {
            alone.extend_from_slice(&encode(event));
        }
        let (_, result, _) = drain_counting(
            alone,
            Vec::new(),
            Some(tablet()),
            Some(moved),
            Some("paint refused".to_string()),
        );
        assert!(result.is_err(), "a refused recovery paint was swallowed");
    }

    #[test]
    fn a_dropped_batch_forgets_the_frame_but_not_where_the_device_is() {
        // `reset` is what a SYN_DROPPED and a button overflow both do. The
        // half-built frame and the buttons go; the device's POSITION is not
        // frame state, and losing it would send the next one-axis report back
        // to wherever the device was when the compositor started.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        pointer.feed(syn(1), axes);

        pointer.feed(abs(2, ABS_X, 9000), axes);
        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        pointer.reset();
        assert!(pointer.buttons.is_empty());
        assert!(pointer.pressed.is_empty());
        assert_eq!(pointer.abs_x, None);
        // The row the stylus is on survived, so an X-only report still names
        // it rather than the 0 the range was opened at.
        pointer.feed(abs(3, ABS_X, 12000), axes);
        assert_eq!(
            pointer.feed(syn(3), axes).unwrap().place,
            PointerPlace::At {
                x: over(12000, 32767),
                y: over(4000, 32767)
            }
        );
    }

    /// A Synaptics-like pad: 80 units/mm across, 40 down, so one pixel is
    /// five units of X and two and a half of Y at 16 px/mm.
    fn touchpad_axes() -> AbsoluteAxes {
        AbsoluteAxes {
            x: sys::AbsInfo {
                resolution: 80,
                ..axis(1472, 1472, 5472)
            },
            y: sys::AbsInfo {
                resolution: 40,
                ..axis(1408, 1408, 4448)
            },
        }
    }

    fn place_of(frame: Option<PointerFrame>) -> Option<PointerPlace> {
        frame.map(|frame| frame.place)
    }

    /// Put one finger down at (x, y): the report that only anchors.
    fn land(pad: &mut PointerMotion, axes: Option<AbsoluteAxes>, time: u32, x: i32, y: i32) {
        pad.feed(key(BTN_TOUCH, KEY_PRESS), axes);
        pad.feed(key(BTN_TOOL_FINGER, KEY_PRESS), axes);
        pad.feed(abs(time, ABS_X, x), axes);
        pad.feed(abs(time, ABS_Y, y), axes);
        assert_eq!(
            pad.feed(syn(time), axes),
            None,
            "a landing moved the pointer"
        );
    }

    #[test]
    fn a_touchpad_finger_drags_the_pointer_and_a_new_touch_never_jumps() {
        let axes = Some(touchpad_axes());
        let mut pad = PointerMotion::touchpad();
        // Hovering: the finger tool without contact moves nothing.
        pad.feed(key(BTN_TOOL_FINGER, KEY_PRESS), axes);
        pad.feed(abs(1, ABS_X, 2000), axes);
        assert_eq!(pad.feed(syn(1), axes), None);
        pad.feed(key(BTN_TOOL_FINGER, KEY_RELEASE), axes);
        pad.feed(syn(1), axes);

        land(&mut pad, axes, 2, 3000, 2000);
        pad.feed(abs(3, ABS_X, 3050), axes);
        pad.feed(abs(3, ABS_Y, 2025), axes);
        assert_eq!(
            place_of(pad.feed(syn(3), axes)),
            Some(PointerPlace::By { dx: 10, dy: 10 })
        );
        // An omitted axis did not move.
        pad.feed(abs(4, ABS_X, 3000), axes);
        assert_eq!(
            place_of(pad.feed(syn(4), axes)),
            Some(PointerPlace::By { dx: -10, dy: 0 })
        );

        // Lift, then land across the pad: the new contact only anchors.
        pad.feed(key(BTN_TOUCH, KEY_RELEASE), axes);
        pad.feed(key(BTN_TOOL_FINGER, KEY_RELEASE), axes);
        assert_eq!(pad.feed(syn(5), axes), None);
        land(&mut pad, axes, 6, 5000, 4000);
        pad.feed(abs(7, ABS_X, 5005), axes);
        assert_eq!(
            place_of(pad.feed(syn(7), axes)),
            Some(PointerPlace::By { dx: 1, dy: 0 })
        );

        // The same events on a tablet are places, as before.
        let mut tablet = PointerMotion::default();
        tablet.feed(abs(8, ABS_X, 3000), axes);
        assert!(matches!(
            place_of(tablet.feed(syn(8), axes)),
            Some(PointerPlace::At { .. })
        ));
    }

    #[test]
    fn touchpad_travel_is_scaled_by_resolution_and_keeps_its_remainder() {
        let axes = Some(touchpad_axes());
        let mut pad = PointerMotion::touchpad();
        land(&mut pad, axes, 1, 3000, 2000);
        // Three units is three fifths of a pixel; five such steps are three
        // whole pixels, none lost to rounding.
        let mut moved = Vec::new();
        for step in 1..=5 {
            pad.feed(abs(1 + step, ABS_X, 3000 + 3 * step as i32), axes);
            moved.push(place_of(pad.feed(syn(1 + step), axes)));
        }
        let one = Some(PointerPlace::By { dx: 1, dy: 0 });
        assert_eq!(moved, [None, one, None, one, one]);
        // Back the whole way in one report, and Y at its own resolution.
        pad.feed(abs(7, ABS_X, 3000), axes);
        pad.feed(abs(7, ABS_Y, 2005), axes);
        assert_eq!(
            place_of(pad.feed(syn(7), axes)),
            Some(PointerPlace::By { dx: -3, dy: 2 })
        );

        // No stated resolution: the X span is TOUCHPAD_SPAN_PX, for Y too.
        let bare = Some(AbsoluteAxes {
            x: axis(0, 0, 2048),
            y: axis(0, 0, 100),
        });
        let mut pad = PointerMotion::touchpad();
        land(&mut pad, bare, 1, 1000, 50);
        pad.feed(abs(2, ABS_X, 1010), bare);
        pad.feed(abs(2, ABS_Y, 54), bare);
        assert_eq!(
            place_of(pad.feed(syn(2), bare)),
            Some(PointerPlace::By { dx: 5, dy: 2 })
        );
        assert_eq!(touch_pixels(i64::MAX, (16, 1), &mut 0), i32::MAX);
        assert_eq!(touch_pixels(5, (16, 0), &mut 7), 0);
    }

    #[test]
    fn multi_finger_frames_move_nothing_and_the_finger_left_re_anchors() {
        let axes = Some(touchpad_axes());
        let mut pad = PointerMotion::touchpad();
        land(&mut pad, axes, 1, 3000, 2000);
        // A second finger: the emulated position may now be the other one.
        pad.feed(key(BTN_TOOL_FINGER, KEY_RELEASE), axes);
        pad.feed(key(BTN_TOOL_DOUBLETAP, KEY_PRESS), axes);
        pad.feed(abs(2, ABS_X, 4000), axes);
        assert_eq!(pad.feed(syn(2), axes), None);
        for tool in [BTN_TOOL_TRIPLETAP, BTN_TOOL_QUADTAP, BTN_TOOL_QUINTTAP] {
            pad.feed(key(tool, KEY_PRESS), axes);
            pad.feed(abs(3, ABS_X, 4400), axes);
            assert_eq!(pad.feed(syn(3), axes), None, "{tool:#x} moved");
            pad.feed(key(tool, KEY_RELEASE), axes);
        }
        // Back to one finger, somewhere else: anchors, then moves from there.
        pad.feed(key(BTN_TOOL_DOUBLETAP, KEY_RELEASE), axes);
        pad.feed(key(BTN_TOOL_FINGER, KEY_PRESS), axes);
        pad.feed(abs(4, ABS_X, 4500), axes);
        assert_eq!(pad.feed(syn(4), axes), None);
        pad.feed(abs(5, ABS_X, 4510), axes);
        assert_eq!(
            place_of(pad.feed(syn(5), axes)),
            Some(PointerPlace::By { dx: 2, dy: 0 })
        );
        // A finger tool held beside any multi-finger one is still several,
        // and the finger alone again only re-anchors.
        for (tool, x) in [
            (BTN_TOOL_DOUBLETAP, 4600),
            (BTN_TOOL_TRIPLETAP, 4800),
            (BTN_TOOL_QUADTAP, 5000),
            (BTN_TOOL_QUINTTAP, 5200),
        ] {
            pad.feed(key(tool, KEY_PRESS), axes);
            pad.feed(abs(6, ABS_X, x), axes);
            assert_eq!(pad.feed(syn(6), axes), None, "{tool:#x} moved");
            pad.feed(key(tool, KEY_RELEASE), axes);
            pad.feed(abs(7, ABS_X, x + 100), axes);
            assert_eq!(pad.feed(syn(7), axes), None, "{tool:#x} kept an anchor");
        }
    }

    #[test]
    fn a_clickpad_press_reaches_the_client_and_contact_keys_do_not() {
        let mut data = Vec::new();
        for event in [
            key(BTN_TOUCH, KEY_PRESS),
            key(BTN_TOOL_FINGER, KEY_PRESS),
            abs(1, ABS_X, 3000),
            abs(1, ABS_Y, 2000),
            syn(1),
            abs(2, ABS_X, 3050),
            syn(2),
            key(BTN_LEFT, KEY_PRESS),
            syn(3),
            key(BTN_LEFT, KEY_RELEASE),
            key(BTN_TOUCH, KEY_RELEASE),
            key(BTN_TOOL_FINGER, KEY_RELEASE),
            syn(4),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result, _) = drain_kind(
            data,
            Vec::new(),
            Some(touchpad_axes()),
            AbsoluteKind::Touchpad,
            None,
            None,
        );
        assert_eq!(result, Ok(()));
        let left = |time, state| PointerButtonInput {
            time,
            button: u32::from(BTN_LEFT),
            state,
        };
        assert_eq!(
            target.pointer_frames,
            [
                (2, 10, 0, Vec::new()),
                (3, 0, 0, vec![left(3, PointerButtonState::Pressed)]),
                (4, 0, 0, vec![left(4, PointerButtonState::Released)]),
            ]
        );
        assert!(target.pointer_places.is_empty(), "a touchpad was a place");
        assert!(target.keys.is_empty(), "a contact key reached the keyboard");
    }

    fn dropped(time: u32) -> Event {
        Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        }
    }

    /// Drive a touchpad reader over `events`, answering a resync with `fresh`.
    fn drain_touchpad(
        events: &[Event],
        fresh: Option<AbsoluteAxes>,
    ) -> (RecordingTarget, Result<(), String>, usize) {
        let data = events.iter().copied().flat_map(encode).collect();
        drain_kind(
            data,
            Vec::new(),
            Some(touchpad_axes()),
            AbsoluteKind::Touchpad,
            fresh,
            None,
        )
    }

    #[test]
    fn a_touchpad_recovery_publishes_no_place_and_trusts_no_snapshot() {
        // The snapshot's Y is newer than what the queue still holds, and the
        // pre-drop Y may have changed inside the gap: neither is an anchor.
        let fresh = AbsoluteAxes {
            y: sys::AbsInfo {
                value: 2100,
                ..touchpad_axes().y
            },
            ..touchpad_axes()
        };
        let (target, result, asked) = drain_touchpad(
            &[
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                abs(1, ABS_X, 3000),
                abs(1, ABS_Y, 2000),
                syn(1),
                abs(2, ABS_X, 3050),
                syn(2),
                dropped(3),
                abs(3, ABS_X, 3400),
                syn(4),
                key(BTN_TOUCH, KEY_RELEASE),
                key(BTN_TOOL_FINGER, KEY_RELEASE),
                syn(5),
                // A new contact whose Y the kernel omits as unchanged.
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                abs(6, ABS_X, 3500),
                syn(6),
                // Y named for the first time since the drop: it anchors.
                abs(7, ABS_X, 3505),
                abs(7, ABS_Y, 2104),
                syn(7),
                abs(8, ABS_Y, 2109),
                syn(8),
            ],
            Some(fresh),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(asked, 0, "a touchpad recovery asked the device");
        assert!(target.pointer_places.is_empty(), "a recovery placed a pad");
        assert_eq!(
            target.pointer_frames,
            [
                (2, 10, 0, Vec::new()),
                (7, 1, 0, Vec::new()),
                (8, 0, 2, Vec::new()),
            ]
        );
    }

    #[test]
    fn a_drop_forgets_contact_keys_so_a_lost_lift_cannot_leave_the_pad_dead() {
        // Two fingers, then a gap that swallowed DOUBLETAP=0 and FINGER=1.
        // Believing DOUBLETAP still held would refuse every later contact.
        let (target, result, _) = drain_touchpad(
            &[
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_DOUBLETAP, KEY_PRESS),
                abs(1, ABS_X, 3000),
                abs(1, ABS_Y, 2000),
                syn(1),
                abs(2, ABS_X, 3100),
                syn(2),
                dropped(3),
                key(BTN_TOOL_DOUBLETAP, KEY_RELEASE),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                syn(4),
                // Still one finger, unknown since the gap: no motion.
                abs(5, ABS_X, 3200),
                syn(5),
                key(BTN_TOUCH, KEY_RELEASE),
                key(BTN_TOOL_FINGER, KEY_RELEASE),
                syn(6),
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                abs(7, ABS_X, 4000),
                abs(7, ABS_Y, 3000),
                syn(7),
                abs(8, ABS_X, 4010),
                syn(8),
            ],
            None,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames, [(8, 2, 0, Vec::new())]);
    }

    #[test]
    fn a_drop_forgets_contact_keys_so_a_lost_second_finger_cannot_move() {
        // One finger tracking, then a gap that swallowed FINGER=0 and
        // DOUBLETAP=1: the reports after it are two fingers.
        let (target, result, _) = drain_touchpad(
            &[
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                abs(1, ABS_X, 3000),
                abs(1, ABS_Y, 2000),
                syn(1),
                abs(2, ABS_X, 3050),
                syn(2),
                dropped(3),
                key(BTN_TOOL_FINGER, KEY_RELEASE),
                key(BTN_TOOL_DOUBLETAP, KEY_PRESS),
                syn(4),
                abs(5, ABS_X, 3150),
                syn(5),
                abs(6, ABS_X, 3250),
                abs(6, ABS_Y, 2100),
                syn(6),
                // Down to one finger again, re-touched: it moves once anchored.
                key(BTN_TOOL_DOUBLETAP, KEY_RELEASE),
                key(BTN_TOUCH, KEY_RELEASE),
                syn(7),
                key(BTN_TOUCH, KEY_PRESS),
                key(BTN_TOOL_FINGER, KEY_PRESS),
                abs(8, ABS_X, 3300),
                syn(8),
                abs(9, ABS_X, 3305),
                syn(9),
            ],
            None,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            target.pointer_frames,
            [(2, 10, 0, Vec::new()), (9, 1, 0, Vec::new())]
        );
    }

    #[test]
    fn one_stated_resolution_serves_both_axes() {
        let only_x = Some(AbsoluteAxes {
            x: sys::AbsInfo {
                resolution: 80,
                ..axis(0, 0, 4000)
            },
            y: axis(0, 0, 100),
        });
        let mut pad = PointerMotion::touchpad();
        land(&mut pad, only_x, 1, 1000, 50);
        pad.feed(abs(2, ABS_X, 1010), only_x);
        // 20 units at X's 80/mm is 4 px; X's span fallback would say 5.
        pad.feed(abs(2, ABS_Y, 70), only_x);
        assert_eq!(
            place_of(pad.feed(syn(2), only_x)),
            Some(PointerPlace::By { dx: 2, dy: 4 })
        );
        let only_y = Some(AbsoluteAxes {
            x: axis(0, 0, 4000),
            y: sys::AbsInfo {
                resolution: 40,
                ..axis(0, 0, 100)
            },
        });
        let mut pad = PointerMotion::touchpad();
        land(&mut pad, only_y, 1, 1000, 50);
        pad.feed(abs(2, ABS_X, 1005), only_y);
        pad.feed(abs(2, ABS_Y, 55), only_y);
        assert_eq!(
            place_of(pad.feed(syn(2), only_y)),
            Some(PointerPlace::By { dx: 2, dy: 2 })
        );
    }

    /// `capabilities/key` of a Synaptics clickpad and of QEMU's virtio
    /// tablet, as the kernel prints them for a 64-bit reader.
    const SYNAPTICS_KEYS: &str = "6420 30000 0 0 0 0\n";
    const VIRTIO_TABLET_KEYS: &str = "30400 1f0000 0 0 0 0\n";

    #[test]
    fn a_sysfs_bitmap_is_read_from_its_least_significant_word() {
        let finger = usize::from(BTN_TOOL_FINGER);
        assert_eq!(bitmap_bit(SYNAPTICS_KEYS, finger), Some(true));
        assert_eq!(
            bitmap_bit(SYNAPTICS_KEYS, usize::from(BTN_TOUCH)),
            Some(true)
        );
        assert_eq!(
            bitmap_bit(SYNAPTICS_KEYS, usize::from(BTN_LEFT)),
            Some(true)
        );
        assert_eq!(bitmap_bit(SYNAPTICS_KEYS, 0x112), Some(false));
        assert_eq!(
            bitmap_bit(VIRTIO_TABLET_KEYS, usize::from(BTN_TOUCH)),
            Some(true)
        );
        assert_eq!(bitmap_bit(VIRTIO_TABLET_KEYS, finger), Some(false));
        // Leading zero words are omitted, so a high bit past the text is clear.
        assert_eq!(bitmap_bit("1\n", 700), Some(false));
        assert_eq!(bitmap_bit("1\n", 0), Some(true));
        for garbled in ["", "\n", "+1", "1 zz", "-1", "10000000000000000000"] {
            assert_eq!(bitmap_bit(garbled, 0), None, "{garbled:?}");
        }
    }

    #[test]
    fn a_touchpad_is_a_pointer_that_is_not_direct_and_reports_a_finger() {
        let touchpad = AbsoluteKind::Touchpad;
        let tablet = AbsoluteKind::Tablet;
        // POINTER alone, and POINTER with BUTTONPAD (and TOPBUTTONPAD).
        for properties in ["1\n", "5\n", "d\n"] {
            assert_eq!(classify(Some(properties), Some(SYNAPTICS_KEYS)), touchpad);
        }
        // DIRECT is a touchscreen, with or without POINTER.
        for properties in ["2\n", "3\n"] {
            assert_eq!(classify(Some(properties), Some(SYNAPTICS_KEYS)), tablet);
        }
        // QEMU's virtio tablet sets no property and no finger tool; a pen
        // tablet may set POINTER but has no finger either.
        assert_eq!(classify(Some("0\n"), Some(VIRTIO_TABLET_KEYS)), tablet);
        assert_eq!(classify(Some("1\n"), Some(VIRTIO_TABLET_KEYS)), tablet);
        // A finger beside a pen (bit 0x140) is a pen tablet, as udev's
        // `finger_but_no_pen` reads it.
        assert_eq!(classify(Some("1\n"), Some("6421 30000 0 0 0 0\n")), tablet);
        // Missing or unreadable keeps the tablet reading.
        assert_eq!(classify(None, Some(SYNAPTICS_KEYS)), tablet);
        assert_eq!(classify(Some("1\n"), None), tablet);
        assert_eq!(classify(None, None), tablet);
        assert_eq!(classify(Some("x\n"), Some(SYNAPTICS_KEYS)), tablet);
        assert_eq!(classify(Some("1\n"), Some("6420 3000g\n")), tablet);
    }

    #[test]
    fn a_node_is_classified_from_its_own_sysfs_entry_and_fails_to_tablet() {
        struct Tree(PathBuf);
        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tree = Tree(std::env::temp_dir().join(format!(
            "td-input-sysfs-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )));
        let entry = |name: &str, properties: Option<&str>, keys: &str| {
            let device = tree.0.join(name).join("device");
            std::fs::create_dir_all(device.join("capabilities")).unwrap();
            std::fs::write(device.join("capabilities").join("key"), keys).unwrap();
            match properties {
                Some(text) => std::fs::write(device.join("properties"), text).unwrap(),
                // A directory where the file should be: present, unreadable.
                None => std::fs::create_dir(device.join("properties")).unwrap(),
            }
        };
        entry("event3", Some("5\n"), SYNAPTICS_KEYS);
        entry("event4", Some("0\n"), VIRTIO_TABLET_KEYS);
        entry("event5", None, SYNAPTICS_KEYS);
        entry("event6", Some(&"0 ".repeat(4096)), SYNAPTICS_KEYS);
        let kind = |node: &str| absolute_kind(&tree.0, Path::new(node));
        assert_eq!(kind("/dev/input/event3"), AbsoluteKind::Touchpad);
        assert_eq!(kind("/dev/input/event4"), AbsoluteKind::Tablet);
        assert_eq!(kind("/dev/input/event5"), AbsoluteKind::Tablet);
        assert_eq!(kind("/dev/input/event6"), AbsoluteKind::Tablet);
        assert_eq!(kind("/dev/input/event9"), AbsoluteKind::Tablet);
        assert_eq!(kind("/"), AbsoluteKind::Tablet);
    }

    /// A boot keyboard: Generic Desktop, Keyboard, modifiers and six keys.
    const KEYBOARD_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x06, 0x75, 0x08, 0x26, 0xff, 0x00, 0x05,
        0x07, 0x19, 0x00, 0x2a, 0xff, 0x00, 0x81, 0x00, 0xc0,
    ];
    /// A YubiKey's FIDO interface: Usage Page 0xf1d0 in its two-byte form,
    /// CTAPHID usage, 64-byte input and output reports.
    const FIDO_DESCRIPTOR: &[u8] = &[
        0x06, 0xd0, 0xf1, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x20, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
        0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x21, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95,
        0x40, 0x91, 0x02, 0xc0,
    ];
    /// The same interface with no Usage Page item: every usage, the
    /// collection's included, is a singleton range of four-byte extended
    /// Usage Minimum and Maximum naming page 0xf1d0 in its high half.
    const RANGED_FIDO_DESCRIPTOR: &[u8] = &[
        0x1b, 0x01, 0x00, 0xd0, 0xf1, 0x2b, 0x01, 0x00, 0xd0, 0xf1, 0xa1, 0x01, 0x1b, 0x20, 0x00,
        0xd0, 0xf1, 0x2b, 0x20, 0x00, 0xd0, 0xf1, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95,
        0x40, 0x81, 0x02, 0x1b, 0x21, 0x00, 0xd0, 0xf1, 0x2b, 0x21, 0x00, 0xd0, 0xf1, 0x91, 0x02,
        0xc0,
    ];

    #[test]
    fn a_usage_page_is_read_from_whole_items_in_every_size() {
        let fido = |bytes: &[u8]| declares_fido_page(bytes);
        assert_eq!(fido(FIDO_DESCRIPTOR), Some(true));
        assert_eq!(fido(KEYBOARD_DESCRIPTOR), Some(false));
        assert_eq!(fido(&[]), Some(false));
        // Two- and four-byte forms, little-endian, zero-extended.
        assert_eq!(fido(&[0x06, 0xd0, 0xf1]), Some(true));
        assert_eq!(fido(&[0x07, 0xd0, 0xf1, 0x00, 0x00]), Some(true));
        assert_eq!(fido(&[0x07, 0x00, 0x00, 0xd0, 0xf1]), Some(false));
        assert_eq!(fido(&[0x07, 0xd0, 0xf1, 0x01, 0x00]), Some(false));
        assert_eq!(fido(&[0x06, 0xf1, 0xd0]), Some(false));
        // A one-byte page cannot name it, and is stepped over whole.
        assert_eq!(fido(&[0x05, 0xd0]), Some(false));
        assert_eq!(fido(&[0x05, 0xf1]), Some(false));
        assert_eq!(fido(&[0x05, 0x01, 0x06, 0xd0, 0xf1]), Some(true));
        // A four-byte extended Usage carries the page in its high half; a
        // two-byte Usage is a usage within the current page, not a page.
        assert_eq!(fido(&[0x0b, 0x01, 0x00, 0xd0, 0xf1]), Some(true));
        assert_eq!(fido(&[0x0a, 0xd0, 0xf1]), Some(false));
        // Usage Minimum and Maximum carry it the same way, each alone.
        assert_eq!(fido(RANGED_FIDO_DESCRIPTOR), Some(true));
        assert_eq!(fido(&[0x1b, 0x01, 0x00, 0xd0, 0xf1]), Some(true));
        assert_eq!(fido(&[0x2b, 0x01, 0x00, 0xd0, 0xf1]), Some(true));
        assert_eq!(fido(&[0x1a, 0xd0, 0xf1, 0x2a, 0xd0, 0xf1]), Some(false));
        // Other four-byte locals and globals name no page in their high half.
        assert_eq!(fido(&[0x3b, 0x01, 0x00, 0xd0, 0xf1]), Some(false));
        assert_eq!(fido(&[0x17, 0x01, 0x00, 0xd0, 0xf1]), Some(false));
        // Data bytes are never prefixes: a Logical Minimum whose data looks
        // like the page item, and long items carrying it.
        assert_eq!(fido(&[0x17, 0x06, 0xd0, 0xf1, 0x00]), Some(false));
        assert_eq!(fido(&[0xfe, 0x03, 0x00, 0x06, 0xd0, 0xf1]), Some(false));
        assert_eq!(
            fido(&[0xfe, 0x02, 0x00, 0xaa, 0xbb, 0x06, 0xd0, 0xf1]),
            Some(true)
        );
        assert_eq!(fido(&[0xfe, 0x00, 0x00, 0x06, 0xd0, 0xf1]), Some(true));
        // Every 0xf* prefix frames a long item, as the kernel reads it.
        assert_eq!(fido(&[0xf1, 0x03, 0x00, 0x06, 0xd0, 0xf1]), Some(false));
        assert_eq!(fido(&[0xf4, 0x00, 0x00, 0x06, 0xd0, 0xf1]), Some(true));
        // Truncated items refuse the whole descriptor, even after a match.
        for truncated in [
            &[0x06, 0xd0][..],
            &[0x07, 0xd0, 0xf1, 0x00],
            &[0x05],
            &[0xfe],
            &[0xfe, 0x05],
            &[0xfe, 0x05, 0x00, 0x01, 0x02],
            &[0x06, 0xd0, 0xf1, 0x06, 0xd0],
            &[0x1b, 0x01, 0x00, 0xd0],
            &[0xf7, 0x02, 0x00, 0x01],
        ] {
            assert_eq!(fido(truncated), None, "{truncated:02x?}");
        }
    }

    /// A temporary sysfs: `devices/...` trees and `class/input/eventN/device`
    /// links into them, as the kernel lays them out.
    struct SysfsTree(PathBuf);

    impl Drop for SysfsTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl SysfsTree {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "td-input-usb-{}-{}",
                std::process::id(),
                TEST_SEQ.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn class(&self) -> PathBuf {
            self.0.join("class").join("input")
        }

        /// A directory under `devices`, with `idVendor` and `busnum` when it
        /// is a USB device.
        fn device(&self, path: &str, usb: bool) -> PathBuf {
            let directory = self.0.join("devices").join(path);
            std::fs::create_dir_all(&directory).unwrap();
            if usb {
                std::fs::write(directory.join("idVendor"), "1050\n").unwrap();
                std::fs::write(directory.join("busnum"), "1\n").unwrap();
            }
            directory
        }

        /// A HID device under an interface, with its descriptor.
        fn hid(&self, interface: &str, name: &str, descriptor: &[u8]) -> PathBuf {
            let hid = self.device(&format!("{interface}/{name}"), false);
            std::fs::write(hid.join("report_descriptor"), descriptor).unwrap();
            hid
        }

        /// `eventN` whose `device` link names `input` under `devices`.
        fn event(&self, event: &str, input: &str) {
            let input = self.device(input, false);
            let node = self.class().join(event);
            std::fs::create_dir_all(&node).unwrap();
            std::os::unix::fs::symlink(input, node.join("device")).unwrap();
        }

        fn excluded(&self, event: &str) -> bool {
            security_key_keyboard(&self.class(), &Path::new("/dev/input").join(event))
        }
    }

    const HUB: &str = "pci0000:00/0000:00:14.0/usb1";

    /// A YubiKey-shaped composite at `port`: an OTP keyboard interface with an
    /// input node, and a FIDO interface beside it with none.
    fn security_key(tree: &SysfsTree, at: &str, event: &str, fido: &[u8]) {
        let device = format!("{HUB}/{at}");
        let port = at.rsplit('/').next().unwrap();
        tree.device(&device, true);
        let keyboard = format!("{device}/{port}:1.0");
        tree.hid(&keyboard, "0003:1050:0407.0001", KEYBOARD_DESCRIPTOR);
        tree.event(
            event,
            &format!("{keyboard}/0003:1050:0407.0001/input/input5"),
        );
        tree.hid(&format!("{device}/{port}:1.1"), "0003:1050:0407.0002", fido);
    }

    #[test]
    fn a_security_keys_own_keyboard_is_found_from_its_usb_device() {
        let tree = SysfsTree::new();
        tree.device(HUB, true);
        security_key(&tree, "1-2", "event5", FIDO_DESCRIPTOR);
        assert!(tree.excluded("event5"));

        // A plain keyboard, whose interface carries a symlink to the key's
        // FIDO HID device: links lead out of a device and are not followed.
        tree.device(&format!("{HUB}/1-3"), true);
        let plain = format!("{HUB}/1-3/1-3:1.0");
        tree.hid(&plain, "0003:046d:c31c.0003", KEYBOARD_DESCRIPTOR);
        tree.event(
            "event6",
            &format!("{plain}/0003:046d:c31c.0003/input/input6"),
        );
        std::os::unix::fs::symlink(
            tree.0
                .join("devices")
                .join(HUB)
                .join("1-2/1-2:1.1/0003:1050:0407.0002"),
            tree.0
                .join("devices")
                .join(&plain)
                .join("0003:1050:0407.0099"),
        )
        .unwrap();
        assert!(!tree.excluded("event6"));

        // Behind a hub, a keyboard beside a key is its own USB device; the
        // key at the next port is still found. Each resolves to its own
        // port: a walk that overshot to the hub would lose event8's key.
        tree.device(&format!("{HUB}/1-4"), true);
        tree.device(&format!("{HUB}/1-4/1-4:1.0"), false);
        let behind = format!("{HUB}/1-4/1-4.1");
        tree.device(&behind, true);
        tree.hid(
            &format!("{behind}/1-4.1:1.0"),
            "0003:046d:c31c.0004",
            KEYBOARD_DESCRIPTOR,
        );
        tree.event(
            "event7",
            &format!("{behind}/1-4.1:1.0/0003:046d:c31c.0004/input/input7"),
        );
        security_key(&tree, "1-4/1-4.2", "event8", FIDO_DESCRIPTOR);
        assert!(!tree.excluded("event7"));
        assert!(tree.excluded("event8"));
        let devices = tree.0.join("devices").canonicalize().unwrap().join(HUB);
        for (event, port) in [("event7", "1-4/1-4.1"), ("event8", "1-4/1-4.2")] {
            assert_eq!(
                usb_device(&tree.class().join(event).join("device")),
                Some(devices.join(port)),
                "{event}"
            );
        }

        // No USB device above: PS/2, and a UHID device declaring the page.
        tree.event("event1", "platform/i8042/serio0/input/input1");
        assert!(!tree.excluded("event1"));
        let uhid = tree.hid("virtual/misc/uhid", "0003:1050:0407.0007", FIDO_DESCRIPTOR);
        tree.event(
            "event9",
            "virtual/misc/uhid/0003:1050:0407.0007/input/input9",
        );
        assert!(uhid.join("report_descriptor").is_file());
        assert!(!tree.excluded("event9"));

        // Unreadable ancestry admits: no node, no link, a dangling link.
        assert!(!tree.excluded("event20"));
        std::fs::create_dir_all(tree.class().join("event21")).unwrap();
        assert!(!tree.excluded("event21"));
        std::fs::create_dir_all(tree.class().join("event22")).unwrap();
        std::os::unix::fs::symlink(
            tree.0.join("devices/gone"),
            tree.class().join("event22").join("device"),
        )
        .unwrap();
        assert!(!tree.excluded("event22"));
        assert!(!security_key_keyboard(&tree.class(), Path::new("/")));
    }

    #[test]
    fn a_descriptor_that_is_truncated_or_oversized_declares_nothing() {
        let tree = SysfsTree::new();
        tree.device(HUB, true);
        let bound = usize::try_from(REPORT_DESCRIPTOR_BYTES).unwrap();
        let padded = |length: usize| {
            let mut bytes = FIDO_DESCRIPTOR.to_vec();
            bytes.resize(length, 0xc0);
            bytes
        };
        security_key(&tree, "1-2", "event5", &padded(bound));
        security_key(&tree, "1-3", "event6", &padded(bound + 1));
        security_key(&tree, "1-4", "event7", &[0x06, 0xd0, 0xf1, 0x26, 0xff]);
        security_key(
            &tree,
            "1-5",
            "event8",
            &[0x06, 0xd0, 0xf1, 0xfe, 0x04, 0x00],
        );
        security_key(
            &tree,
            "1-6",
            "event9",
            &[0xfe, 0x01, 0x00, 0x00, 0x07, 0xd0, 0xf1, 0x00, 0x00],
        );
        assert!(tree.excluded("event5"));
        assert!(!tree.excluded("event6"));
        assert!(!tree.excluded("event7"));
        assert!(!tree.excluded("event8"));
        assert!(tree.excluded("event9"));
        // A complete descriptor declaring the page only through extended
        // usage ranges is found; an empty one, as a HID device whose driver
        // has not probed reads, declares nothing.
        security_key(&tree, "1-7", "event10", RANGED_FIDO_DESCRIPTOR);
        security_key(&tree, "1-8", "event11", &[]);
        assert!(tree.excluded("event10"));
        assert!(!tree.excluded("event11"));
        let roster: Vec<(PathBuf, ())> = ["event5", "event6", "event9"]
            .iter()
            .map(|event| (Path::new("/dev/input").join(event), ()))
            .collect();
        assert_eq!(
            attention_exclusions(&tree.class(), &roster),
            BTreeSet::from([0, 2])
        );
    }

    fn press(bindings: &mut KeyBindings, code: u16) -> Option<Command> {
        bindings.feed(key(code, KEY_PRESS)).command
    }

    fn tap(bindings: &mut KeyBindings, code: u16) -> Option<Command> {
        let command = press(bindings, code);
        bindings.feed(key(code, KEY_RELEASE));
        command
    }

    #[derive(Debug, Eq, PartialEq)]
    enum KeyboardCall {
        Key(KeyInput),
        Modifiers(ModifierState),
    }

    /// What an absolute report reached the target as: the time, each axis as
    /// a fraction of the device's own span, and the buttons that came with it.
    type RecordedPlace = (u32, Fraction, Fraction, Vec<PointerButtonInput>);

    #[derive(Default)]
    struct RecordingTarget {
        attention_events: Vec<bool>,
        secret_roles: Vec<crate::secret_client::Selection>,
        notices: Vec<crate::attention::Notice>,
        /// The attention screens fail to paint.
        notice_fails: bool,
        /// The attention screens' paints are owed, not yet on glass.
        notices_owed: bool,
        clock: Arc<crate::runtime::PresentationClock>,
        confirmations: Vec<u128>,
        /// Each approval-key digit offered, with its evdev time.
        approvals: Vec<(u8, u128)>,
        /// The attempt answers that each offered digit ended its request.
        approval_ends: bool,
        attention_cutoff: u128,
        attention_error: Option<String>,
        draining_events: usize,
        draining_error: Option<String>,
        modifiers_error: Option<String>,
        commands: Vec<Command>,
        launcher_actions: Vec<LauncherAction>,
        launcher_visible: bool,
        keys: Vec<KeyInput>,
        key_error: Option<String>,
        modifiers: Vec<ModifierState>,
        keyboard_calls: Vec<KeyboardCall>,
        pointer_frames: Vec<(u32, i32, i32, Vec<PointerButtonInput>)>,
        pointer_places: Vec<RecordedPlace>,
        /// Kept apart from both lists, and only the reports that CARRY a
        /// notch: a wheel is orthogonal to where the pointer is, so a test
        /// about scrolling should not have to say which of the two ways the
        /// report described its position.
        pointer_scrolls: Vec<(u32, PointerScroll)>,
        pointer_error: Option<String>,
        flushes: usize,
        flush_error: Option<String>,
        launched: Vec<LaunchRequest>,
        help_actions: Vec<HelpAction>,
        help: crate::help::Help,
        /// The attempt's own PIN field model, which the tests open.
        field: crate::secret_client::PinField,
        /// The masked length shown after each key that changed the field.
        field_lengths: Vec<usize>,
        /// Every key offered to the field, open or not.
        field_keys: usize,
        field_times: Vec<u128>,
        /// The lock surface is up.
        locked: bool,
        /// How many times the lock surface was put up.
        lock_screens: usize,
    }

    impl RecordingTarget {
        /// Only a report that CARRIES a notch. Every report reaches one of
        /// the two delivery methods, so recording them all would make the
        /// list a count of reports rather than of scrolls.
        fn record_scroll(&mut self, time: u32, scroll: PointerScroll) {
            if !scroll.is_still() {
                self.pointer_scrolls.push((time, scroll));
            }
        }
    }

    impl InputTarget for RecordingTarget {
        fn confirm_install(&mut self, timestamp: u128) -> Result<(), String> {
            self.confirmations.push(timestamp);
            Ok(())
        }

        fn approval_digit(&mut self, digit: u8, timestamp: u128) -> Result<bool, String> {
            self.approvals.push((digit, timestamp));
            Ok(self.approval_ends)
        }

        fn secret_request(&mut self, role: crate::secret_client::Selection) -> Result<(), String> {
            self.secret_roles.push(role);
            Ok(())
        }

        fn session_locked(&mut self) -> bool {
            self.locked
        }

        fn lock_screen(&mut self) -> Result<crate::runtime::NoticePresentation, String> {
            self.lock_screens += 1;
            self.locked = true;
            self.launcher_visible = false;
            self.help.set(false);
            let epoch = u64::try_from(self.lock_screens).unwrap();
            Ok(crate::runtime::NoticePresentation::for_test(
                &self.clock,
                epoch,
            ))
        }

        fn pin_key(
            &mut self,
            key: crate::secret_client::FieldKey,
            timestamp: u128,
        ) -> Result<(), String> {
            self.field_keys += 1;
            self.field_times.push(timestamp);
            if let Some(length) = self.field.key(key) {
                self.field_lengths.push(length);
            }
            Ok(())
        }

        fn attention_notice(
            &mut self,
            notice: crate::attention::Notice,
        ) -> Option<crate::runtime::NoticePresentation> {
            self.notices.push(notice);
            if self.notice_fails {
                return None;
            }
            let epoch = u64::try_from(self.notices.len()).unwrap();
            if !self.notices_owed {
                self.clock.publish_for_test(epoch);
            }
            Some(crate::runtime::NoticePresentation::for_test(
                &self.clock,
                epoch,
            ))
        }

        fn drain_attention(&mut self) -> Result<(), String> {
            self.draining_events += 1;
            // As the live target's cancellation of its attempt does.
            self.field.close();
            match self.draining_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn attention(&mut self, visible: bool) -> Result<u128, String> {
            self.attention_events.push(visible);
            match self.attention_error.take() {
                Some(error) if visible => Err(error),
                error => {
                    self.attention_error = error;
                    Ok(self.attention_cutoff)
                }
            }
        }

        fn command(&mut self, command: Command) -> Result<(), String> {
            self.commands.push(command);
            Ok(())
        }

        fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
            self.launched.push(request);
            Ok(())
        }

        fn help(&mut self, action: HelpAction) -> Result<bool, String> {
            self.help_actions.push(action);
            // The real model, not a second copy of its rule: a fake that
            // drifted would let the adapter test agree with a sheet that no
            // longer behaves this way.
            self.help.set(action.target(self.help.visible()));
            Ok(self.help.visible())
        }

        fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
            self.launcher_actions.push(action);
            match action {
                LauncherAction::Open => self.launcher_visible = true,
                LauncherAction::Close | LauncherAction::Activate | LauncherAction::Choose(_) => {
                    self.launcher_visible = false
                }
                LauncherAction::Next
                | LauncherAction::Previous
                | LauncherAction::Insert(_)
                | LauncherAction::Backspace => {}
            }
            Ok(self.launcher_visible)
        }

        fn key(&mut self, input: KeyInput) -> Result<(), String> {
            self.keys.push(input);
            self.keyboard_calls.push(KeyboardCall::Key(input));
            match self.key_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
            self.modifiers.push(modifiers);
            self.keyboard_calls.push(KeyboardCall::Modifiers(modifiers));
            match self.modifiers_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn pointer_frame(
            &mut self,
            time: u32,
            dx: i32,
            dy: i32,
            buttons: &[PointerButtonInput],
            scroll: PointerScroll,
        ) -> Result<(), String> {
            self.pointer_frames.push((time, dx, dy, buttons.to_vec()));
            self.record_scroll(time, scroll);
            match self.pointer_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn pointer_frame_at(
            &mut self,
            time: u32,
            x: Fraction,
            y: Fraction,
            buttons: &[PointerButtonInput],
            scroll: PointerScroll,
        ) -> Result<(), String> {
            // Kept apart from the relative list on purpose: a test that
            // asserted a tablet report as a delta would be reading the wrong
            // question answered the wrong way.
            self.pointer_places.push((time, x, y, buttons.to_vec()));
            self.record_scroll(time, scroll);
            match self.pointer_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn flush(&mut self) -> Result<(), String> {
            self.flushes = self.flushes.saturating_add(1);
            match self.flush_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    struct LauncherModelTarget {
        launcher: crate::launcher::Launcher,
        recording: RecordingTarget,
        /// What `pointer_launcher` answers, in order, and what it was asked.
        pointer_answers: std::collections::VecDeque<Option<LauncherAction>>,
        pointer_asks: Vec<bool>,
    }

    impl LauncherModelTarget {
        fn new() -> Self {
            Self {
                launcher: crate::launcher::Launcher::new(),
                recording: RecordingTarget::default(),
                pointer_answers: std::collections::VecDeque::new(),
                pointer_asks: Vec::new(),
            }
        }
    }

    impl InputTarget for LauncherModelTarget {
        fn drain_attention(&mut self) -> Result<(), String> {
            self.recording.drain_attention()
        }

        fn attention(&mut self, visible: bool) -> Result<u128, String> {
            self.recording.attention(visible)
        }

        fn command(&mut self, command: Command) -> Result<(), String> {
            self.recording.command(command)
        }

        fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
            self.recording.launch(request)
        }

        fn help(&mut self, action: HelpAction) -> Result<bool, String> {
            self.recording.help(action)
        }

        fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
            self.recording.launcher_actions.push(action);
            self.launcher.apply(action);
            Ok(self.launcher.visible())
        }

        fn pointer_launcher(&mut self, pressed: bool) -> Result<Option<LauncherAction>, String> {
            self.pointer_asks.push(pressed);
            Ok(self.pointer_answers.pop_front().flatten())
        }

        fn key(&mut self, input: KeyInput) -> Result<(), String> {
            self.recording.key(input)
        }

        fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
            self.recording.modifiers(modifiers)
        }

        fn pointer_frame(
            &mut self,
            time: u32,
            dx: i32,
            dy: i32,
            buttons: &[PointerButtonInput],
            scroll: PointerScroll,
        ) -> Result<(), String> {
            self.recording.pointer_frame(time, dx, dy, buttons, scroll)
        }

        fn pointer_frame_at(
            &mut self,
            time: u32,
            x: Fraction,
            y: Fraction,
            buttons: &[PointerButtonInput],
            scroll: PointerScroll,
        ) -> Result<(), String> {
            self.recording.pointer_frame_at(time, x, y, buttons, scroll)
        }

        fn flush(&mut self) -> Result<(), String> {
            self.recording.flush()
        }
    }

    fn encode(event: Event) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(EVENT_SIZE);
        bytes.extend_from_slice(
            &i64::try_from(event.timestamp / 1_000_000_000)
                .unwrap()
                .to_ne_bytes(),
        );
        bytes.extend_from_slice(
            &i64::try_from((event.timestamp % 1_000_000_000) / 1_000)
                .unwrap()
                .to_ne_bytes(),
        );
        bytes.extend_from_slice(&event.kind.to_ne_bytes());
        bytes.extend_from_slice(&event.code.to_ne_bytes());
        bytes.extend_from_slice(&event.value.to_ne_bytes());
        bytes
    }

    #[test]
    fn a_tablet_reaches_the_target_as_a_place_and_a_mouse_as_a_distance() {
        // End to end through the reader, which is the join this feature is:
        // the range comes from the device at open, the events come off the
        // wire, and what the compositor is told has to be a POSITION.
        let mut data = Vec::new();
        for event in [abs(1, ABS_X, 32767), abs(1, ABS_Y, 0), syn(1)] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_device(data.clone(), Vec::new(), Some(tablet()));
        assert_eq!(result, Ok(()));
        assert!(
            target.pointer_frames.is_empty(),
            "a tablet was reported as a delta"
        );
        assert_eq!(
            target.pointer_places,
            [(1, over(32767, 32767), over(0, 32767), Vec::new())]
        );

        // The same bytes from a device that answered no range reach nobody:
        // there is nothing to scale against, and a raw value taken as a
        // delta would fling the pointer across the screen.
        let (target, result) = drain_device(data, Vec::new(), None);
        assert_eq!(result, Ok(()));
        assert!(target.pointer_places.is_empty());
        assert!(target.pointer_frames.is_empty());

        // A dropped batch re-asks the device, and the answer replaces the
        // position rather than being merged with it. Only the device knows
        // where it went while its reports were being discarded: the kernel
        // compares an axis against the value IT last emitted, so one that
        // moved inside the gap is never re-sent.
        let mut data = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            syn(2),
            key(BTN_MOUSE, KEY_PRESS),
            syn(3),
        ] {
            data.extend_from_slice(&encode(event));
        }
        // Deliberately DIFFERENT spans. The recovery composes its own pair of
        // fractions, a second call site for the axis mapping, and one scaled
        // against the other axis is a well-formed position somewhere else —
        // which a fixture sharing one range cannot tell from the right answer.
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };

        // The recovery PUBLISHES the fresh position rather than only caching
        // it. This batch ends at the resynchronising SYN_REPORT, so nothing
        // after it can consume the re-read: a device that moved during the gap
        // and then stopped must still reach the screen.
        let mut quiet = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            syn(2),
        ] {
            quiet.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_resyncing(quiet, Vec::new(), Some(tablet()), Some(moved));
        assert_eq!(result, Ok(()));
        assert_eq!(
            target.pointer_places.last(),
            Some(&(2, over(20000, 32767), over(30000, 40000), Vec::new())),
            "a recovery re-read the device and told nobody"
        );
        let (target, result) =
            drain_resyncing(data.clone(), Vec::new(), Some(tablet()), Some(moved));
        assert_eq!(result, Ok(()));
        let (_, x, y, buttons) = target.pointer_places.last().unwrap();
        assert_eq!(
            (*x, *y),
            (over(20000, 32767), over(30000, 40000)),
            "a recovered device was placed at where it used to be"
        );
        // The press rides the frame that placed it. Asserted because this is
        // the only path that carries one: once a device declares axes, an
        // absolute frame is the ONLY way a button of its reaches the target.
        assert_eq!(
            buttons,
            &vec![PointerButtonInput {
                button: u32::from(BTN_MOUSE),
                state: PointerButtonState::Pressed,
                time: 3,
            }]
        );

        // A device that cannot be asked keeps what it last said, which beats
        // the position it was opened at even though it may be stale. This is
        // also what pins the RESET rather than `reset` itself: a recovery that
        // rebuilt the whole `PointerMotion` would answer 0 here, the value the
        // range was opened at, and no test of `reset` alone can see which of
        // the two a call site made.
        let (target, result) = drain_resyncing(data, Vec::new(), Some(tablet()), None);
        assert_eq!(result, Ok(()));
        let (_, x, y, _) = target.pointer_places.last().unwrap();
        assert_eq!((*x, *y), (over(100, 32767), over(100, 32767)));

        // And the other half of the name: an ORDINARY MOUSE over the same
        // reader is a distance, on the list a tablet never reaches.
        let mut data = Vec::new();
        for event in [
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 7,
            },
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_REL,
                code: REL_Y,
                value: -3,
            },
            syn(2),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_device(data, Vec::new(), None);
        assert_eq!(result, Ok(()));
        assert!(
            target.pointer_places.is_empty(),
            "a mouse was reported as a place"
        );
        assert_eq!(target.pointer_frames, [(2, 7, -3, Vec::new())]);
    }

    #[test]
    fn a_wheel_only_report_is_delivered_rather_than_read_as_a_still_pointer() {
        // The reader keeps a wheel-only frame; the DELIVERY path has a second
        // silence test of its own, and this is the one that catches it asking
        // only about motion and buttons. The whole path — bytes in, a scroll
        // at the runtime — because either guard alone would swallow it.
        let mut data = Vec::new();
        for event in [
            Event {
                timestamp: 3_000_000,
                time: 3,
                kind: EV_REL,
                code: REL_WHEEL,
                value: -2,
            },
            syn(3),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_device(data, Vec::new(), None);
        assert_eq!(result, Ok(()));
        assert_eq!(
            target.pointer_scrolls,
            [(
                3,
                PointerScroll {
                    vertical: -2,
                    horizontal: 0
                }
            )]
        );
        // Delivered as a report that moved nothing, which is what it is: the
        // scroll rides the ordinary frame rather than a path of its own, so
        // the client gets one `wl_pointer.frame` for the one report.
        assert_eq!(target.pointer_frames, [(3, 0, 0, Vec::new())]);
        assert!(target.pointer_places.is_empty());
    }

    fn motion(time: u32, dx: i32) -> Vec<u8> {
        let mut bytes = encode(Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_REL,
            code: REL_X,
            value: dx,
        });
        bytes.extend_from_slice(&encode(Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }));
        bytes
    }

    /// A reader that hands out a scripted sequence of short reads, the way a
    /// character device may but a regular file never does.
    struct ChunkedReader {
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        at: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunks: Vec<std::io::Result<usize>>) -> Self {
            Self {
                data,
                chunks,
                at: 0,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let want = if self.chunks.is_empty() {
                self.data.len() - self.at
            } else {
                self.chunks.remove(0)?
            };
            let count = want.min(self.data.len() - self.at).min(buffer.len());
            buffer[..count].copy_from_slice(&self.data[self.at..self.at + count]);
            self.at += count;
            Ok(count)
        }
    }

    fn drain(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
    ) -> (RecordingTarget, Result<(), String>) {
        drain_device(data, chunks, None)
    }

    fn drain_device(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
    ) -> (RecordingTarget, Result<(), String>) {
        drain_resyncing(data, chunks, axes, None)
    }

    /// `resync` is what a real device answers after a dropped batch. `None`
    /// stands for a device that could not be asked, which is every reader
    /// driven from a byte slice.
    fn drain_resyncing(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
        resync: Option<AbsoluteAxes>,
    ) -> (RecordingTarget, Result<(), String>) {
        let (target, result, _) = drain_counting(data, chunks, axes, resync, None);
        (target, result)
    }

    /// The same, counting how many times the device was asked — the property
    /// `absolute_axes` claims and nothing else could check, since a closure
    /// answering a constant looks the same whether it ran once or every frame.
    fn drain_counting(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
        resync: Option<AbsoluteAxes>,
        pointer_error: Option<String>,
    ) -> (RecordingTarget, Result<(), String>, usize) {
        drain_kind(
            data,
            chunks,
            axes,
            AbsoluteKind::Tablet,
            resync,
            pointer_error,
        )
    }

    fn drain_kind(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
        kind: AbsoluteKind,
        resync: Option<AbsoluteAxes>,
        pointer_error: Option<String>,
    ) -> (RecordingTarget, Result<(), String>, usize) {
        let target = Mutex::new(RecordingTarget::default());
        target
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pointer_error = pointer_error;
        let bindings = Mutex::new(KeyBindings::default());
        let mut reader = ChunkedReader::new(data, chunks);
        let asked = std::cell::Cell::new(0usize);
        let mut resync = || {
            asked.set(asked.get().saturating_add(1));
            resync
        };
        let result = read_device(
            Path::new("event-test"),
            &mut reader,
            0,
            &target,
            &bindings,
            axes.map(|axes| (axes, kind)),
            &mut resync,
        );
        (
            target
                .into_inner()
                .unwrap_or_else(|error| error.into_inner()),
            result,
            asked.get(),
        )
    }

    #[test]
    fn carry_remainder_moves_only_the_tail() {
        let mut buffer = [1u8, 2, 3, 4, 5, 6];
        assert_eq!(carry_remainder(&mut buffer, 4, 6), 2);
        assert_eq!(&buffer[..2], &[5, 6]);
        assert_eq!(carry_remainder(&mut buffer, 2, 2), 0);
        assert_eq!(carry_remainder(&mut buffer, 0, 3), 3);
        assert_eq!(&buffer[..3], &[5, 6, 3]);
        // `read_device` consumes a whole multiple of a record and so cannot ask
        // for more than it filled; out of that domain the bounds are clamped
        // rather than allowed to panic inside `copy_within`.
        assert_eq!(carry_remainder(&mut buffer, 9, 3), 0);
        assert_eq!(carry_remainder(&mut buffer, 1, 99), 5);
    }

    #[test]
    fn a_batch_of_reports_costs_one_flush_not_one_per_report() {
        let mut data = Vec::new();
        for step in 0..32u32 {
            data.extend_from_slice(&motion(step, 1));
        }
        let (target, result) = drain(data, Vec::new());
        assert_eq!(result, Ok(()));
        // Every report still reaches the seat -- clients see the motion path.
        assert_eq!(target.pointer_frames.len(), 32);
        // One paint for the batch, plus the teardown flush after EOF.
        assert_eq!(target.flushes, 2);
    }

    #[test]
    fn a_report_that_arrives_alone_is_painted_without_waiting() {
        let (target, result) = drain(motion(1, 1), vec![Ok(EVENT_SIZE), Ok(EVENT_SIZE)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames.len(), 1);
        // The bare motion read flushes nothing to paint; the report's own read
        // flushes it, and EOF flushes again.
        assert_eq!(target.flushes, 3);
    }

    #[test]
    fn a_record_split_across_reads_is_carried_to_the_next() {
        let data = motion(4, 3);
        let (target, result) = drain(data, vec![Ok(EVENT_SIZE + 7), Ok(EVENT_SIZE - 7)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames, vec![(4, 3, 0, Vec::new())]);
    }

    #[test]
    fn an_interrupted_read_resumes_the_batch() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "signal");
        let (target, result) = drain(motion(6, 5), vec![Err(interrupted)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames, vec![(6, 5, 0, Vec::new())]);
    }

    #[test]
    fn a_flush_failure_closes_the_device_after_releasing_its_pressed_buttons() {
        let target = Mutex::new(RecordingTarget::default());
        target.lock().unwrap().flush_error = Some("paint refused".to_string());
        let bindings = Mutex::new(KeyBindings::default());
        let mut data = encode(Event {
            timestamp: 1_000_000,
            time: 1,
            kind: EV_KEY,
            code: BTN_MOUSE,
            value: KEY_PRESS,
        });
        data.extend_from_slice(&motion(1, 2));
        let mut reader = ChunkedReader::new(data, Vec::new());
        let result = read_device(
            Path::new("event-test"),
            &mut reader,
            0,
            &target,
            &bindings,
            None,
            &mut || None,
        );
        assert_eq!(result, Err("paint refused".to_string()));
        assert!(bindings.lock().unwrap().pointer_pressed.is_empty());
        let target = target.into_inner().unwrap();
        let released = target
            .pointer_frames
            .last()
            .map(|(_, _, _, buttons)| buttons.clone())
            .unwrap_or_default();
        assert_eq!(
            released
                .iter()
                .map(|button| (button.button, button.state))
                .collect::<Vec<_>>(),
            vec![(u32::from(BTN_MOUSE), PointerButtonState::Released)]
        );
    }

    #[test]
    fn parses_x86_64_input_event_tail() {
        let mut bytes = [0u8; EVENT_SIZE];
        bytes
            .get_mut(..8)
            .unwrap()
            .copy_from_slice(&12i64.to_ne_bytes());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&345_000i64.to_ne_bytes());
        bytes
            .get_mut(16..18)
            .unwrap()
            .copy_from_slice(&EV_KEY.to_ne_bytes());
        bytes
            .get_mut(18..20)
            .unwrap()
            .copy_from_slice(&KEY_RIGHT.to_ne_bytes());
        bytes
            .get_mut(20..24)
            .unwrap()
            .copy_from_slice(&KEY_PRESS.to_ne_bytes());
        assert_eq!(
            parse(&bytes).unwrap(),
            Event {
                timestamp: 12_345_000_000,
                time: 12_345,
                kind: EV_KEY,
                code: KEY_RIGHT,
                value: KEY_PRESS
            }
        );
        assert!(parse(bytes.get(..23).unwrap()).is_err());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&1_000_000i64.to_ne_bytes());
        assert!(parse(&bytes).is_err());
        bytes
            .get_mut(..8)
            .unwrap()
            .copy_from_slice(&(-1i64).to_ne_bytes());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&(-1i64).to_ne_bytes());
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn input_node_names_are_narrow() {
        assert!(event_name("event0"));
        assert!(!event_name("event"));
        assert!(!event_name("event0-old"));
    }

    #[test]
    fn arrow_keys_focus_and_shift_moves_in_every_direction() {
        for (code, direction) in [
            (KEY_LEFT, Direction::Left),
            (KEY_RIGHT, Direction::Right),
            (KEY_UP, Direction::Up),
            (KEY_DOWN, Direction::Down),
        ] {
            let mut bindings = KeyBindings::default();
            assert_eq!(tap(&mut bindings, code), None);
            assert_eq!(press(&mut bindings, KEY_LEFTMETA), None);
            assert_eq!(tap(&mut bindings, code), Some(Command::Focus(direction)));
            assert_eq!(press(&mut bindings, KEY_RIGHTSHIFT), None);
            assert_eq!(tap(&mut bindings, code), Some(Command::Move(direction)));
            assert_eq!(
                bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE)).command,
                None
            );
            assert_eq!(tap(&mut bindings, code), Some(Command::Focus(direction)));
        }
    }

    #[test]
    fn either_meta_and_shift_are_tracked_until_both_sides_release() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        press(&mut bindings, KEY_RIGHTMETA);
        press(&mut bindings, KEY_LEFTSHIFT);
        press(&mut bindings, KEY_RIGHTSHIFT);
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_RIGHT),
            Some(Command::Move(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_RIGHT),
            Some(Command::Focus(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(tap(&mut bindings, KEY_RIGHT), None);
    }

    #[test]
    fn all_nine_workspace_keys_switch_or_move_the_focused_tile() {
        for (code, number) in [
            (KEY_1, 1),
            (KEY_2, 2),
            (KEY_3, 3),
            (KEY_4, 4),
            (KEY_5, 5),
            (KEY_6, 6),
            (KEY_7, 7),
            (KEY_8, 8),
            (KEY_9, 9),
        ] {
            let mut bindings = KeyBindings::default();
            press(&mut bindings, KEY_RIGHTMETA);
            assert_eq!(
                tap(&mut bindings, code),
                Some(Command::SwitchWorkspace(number))
            );
            press(&mut bindings, KEY_LEFTSHIFT);
            assert_eq!(
                tap(&mut bindings, code),
                Some(Command::MoveToWorkspace(number))
            );
        }
    }

    #[test]
    fn super_chords_select_fullscreen_and_both_split_axes() {
        for (code, expected) in [
            (KEY_F, Command::ToggleFullscreen),
            (KEY_V, Command::SetPresentation(Presentation::Stacked)),
            (KEY_H, Command::SetPresentation(Presentation::Tabbed)),
        ] {
            let mut bindings = KeyBindings::default();
            // Bare, the key is the client's text: only the modifier makes it
            // a command.
            let bare = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(bare.command, None);
            assert!(bare.forward.is_some());
            bindings.feed(key(code, KEY_RELEASE));
            press(&mut bindings, KEY_LEFTMETA);
            assert_eq!(tap(&mut bindings, code), Some(expected));
        }
    }

    /// What a help row's chord actually produces. The sheet is PAINTED text,
    /// so nothing but a test can stop it describing bindings that no longer
    /// exist — the compiler sees two unrelated string literals.
    #[derive(Debug, Eq, PartialEq)]
    enum Bound {
        Command(Command),
        Launcher(LauncherAction),
        Launch(LaunchRequest),
        Help(HelpAction),
        /// The paired profile's session lock.
        Lock,
        /// Documented but not a key, so this row is exercised elsewhere. The
        /// SPELLING is carried because the mouse has three gestures here and
        /// the sheet names them all; the WORDS come from `action`, so a mouse
        /// row still cannot invent its own.
        ///
        /// What is NOT checked, and cannot be from this table, is that the
        /// gesture named produces the effect claimed: a keyboard row derives
        /// its effect from the dispatch that just ran, and a mouse row has no
        /// dispatch to derive from, so a row and its probe changed TOGETHER
        /// would agree about something untrue. The gestures themselves are
        /// proved where they happen — `runtime.rs`'s hover and click focus
        /// tests, and `dragging_a_title_band_drops_the_window_beside_where_
        /// it_was_released`.
        Pointer(&'static str, Pointing),
    }

    /// What a mouse row claims the pointer does, as an effect rather than a
    /// string — the sheet's words live in exactly one table either way.
    #[derive(Debug, Eq, PartialEq)]
    enum Pointing {
        Focus,
        Move,
        Send,
        Switch,
        Launcher,
    }

    impl Bound {
        /// The words the sheet must use for this effect. Checking the ACTION
        /// column is the half that makes a row honest: without it a row can
        /// name the right chord beside a description of something else.
        fn action(&self) -> &'static str {
            match self {
                Bound::Command(Command::Focus(_)) | Bound::Pointer(_, Pointing::Focus) => {
                    "FOCUS A TILE"
                }
                // Across the grain a tile LEAVES its container, which "MOVE A
                // TILE" did not say and is the only way out of one.
                Bound::Command(Command::Move(_)) | Bound::Pointer(_, Pointing::Move) => {
                    "MOVE A TILE / SPLIT OUT"
                }
                // Same effect from the strip, so the same words: the chord
                // names a NUMBER and the pointer names a cell or a direction,
                // and the card would otherwise carry two vocabularies for one
                // object — the line `Send` already draws below.
                Bound::Command(Command::SwitchWorkspace(_))
                | Bound::Pointer(_, Pointing::Switch) => "SWITCH WORKSPACE",
                // The pointer reaches the same effect by a different route:
                // `Super+Shift+N` names a NUMBER, a drop names wherever it
                // landed. One effect, so one wording — the card would
                // otherwise carry two vocabularies for one object.
                Bound::Command(Command::MoveToWorkspace(_)) | Bound::Pointer(_, Pointing::Send) => {
                    "MOVE TO WORKSPACE"
                }
                Bound::Command(Command::SetPresentation(Presentation::Stacked)) => "STACK A COLUMN",
                Bound::Command(Command::SetPresentation(Presentation::Tabbed)) => "TAB A COLUMN",
                // Not bound to a chord: `Super+s` ungroups, and a second way
                // to say the same thing is a second row on the help sheet
                // nobody can reach.
                Bound::Command(Command::SetPresentation(Presentation::Split)) => "UNGROUP",
                Bound::Command(Command::ToggleFullscreen) => "TOGGLE FULLSCREEN",
                Bound::Command(Command::ToggleGrouped) => "GROUP A COLUMN",
                Bound::Launch(LaunchRequest::Terminal) => "NEW TERMINAL",
                Bound::Launch(LaunchRequest::UiDemo) => "OPEN UI CLIENT",
                Bound::Launch(LaunchRequest::TaskManager) => "TASK MANAGER",
                Bound::Launch(LaunchRequest::Editor) => "TEXT EDITOR",
                Bound::Launch(LaunchRequest::Photo) => "PHOTOS",
                Bound::Launch(LaunchRequest::Review) => "CODE REVIEW",
                Bound::Launch(LaunchRequest::Dua) => "DISK USAGE",
                Bound::Launch(LaunchRequest::Agent) => "CODING AGENT",
                Bound::Launcher(_) | Bound::Pointer(_, Pointing::Launcher) => "OPEN LAUNCHER",
                Bound::Help(_) => "THIS HELP",
                Bound::Lock => "LOCK SCREEN",
            }
        }
    }

    /// How a chord is SPELLED on the sheet, derived from the codes a probe
    /// actually pressed. Key FAMILIES rather than one row each, because four
    /// bindings are a range and printing every member would be a worse cheat
    /// sheet than naming the range.
    fn spelling(modifiers: &[u16], code: u16) -> String {
        let label = match code {
            KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => "ARROWS",
            KEY_1..=KEY_9 => "1..9",
            KEY_V => "V",
            KEY_H => "H",
            KEY_F => "F",
            KEY_S => "S",
            KEY_T => "T",
            KEY_ENTER => "ENTER",
            KEY_L => "L",
            // `?` IS the shifted `/`, so the glyph absorbs the modifier
            // rather than the sheet naming it twice.
            KEY_SLASH => return "SUPER+?".to_string(),
            other => panic!("no help spelling for evdev {other}"),
        };
        let shift = if modifiers.contains(&KEY_LEFTSHIFT) {
            "SHIFT+"
        } else {
            ""
        };
        format!("SUPER+{shift}{label}")
    }

    #[test]
    fn every_painted_help_row_is_the_binding_it_claims() {
        // One entry per row, IN ORDER, so a row added without a probe fails
        // the length check rather than going unchecked.
        let probes: &[(&[u16], u16, Bound)] = &[
            (
                &[KEY_LEFTMETA],
                KEY_LEFT,
                Bound::Command(Command::Focus(Direction::Left)),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_UP,
                Bound::Command(Command::Move(Direction::Up)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_3,
                Bound::Command(Command::SwitchWorkspace(3)),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_9,
                Bound::Command(Command::MoveToWorkspace(9)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_V,
                Bound::Command(Command::SetPresentation(Presentation::Stacked)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_H,
                Bound::Command(Command::SetPresentation(Presentation::Tabbed)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_F,
                Bound::Command(Command::ToggleFullscreen),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_S,
                Bound::Command(Command::ToggleGrouped),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_T,
                Bound::Launch(LaunchRequest::Terminal),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_ENTER,
                Bound::Launcher(LauncherAction::Open),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_SLASH,
                Bound::Help(HelpAction::Toggle),
            ),
            (&[KEY_LEFTMETA], KEY_L, Bound::Lock),
            (&[], 0, Bound::Pointer("HOVER", Pointing::Focus)),
            (&[], 0, Bound::Pointer("CLICK", Pointing::Focus)),
            (&[], 0, Bound::Pointer("DRAG A TITLE", Pointing::Move)),
            (&[], 0, Bound::Pointer("DRAG TO THE BAR", Pointing::Send)),
            (&[], 0, Bound::Pointer("CLICK THE BAR", Pointing::Switch)),
            (&[], 0, Bound::Pointer("SCROLL THE BAR", Pointing::Switch)),
            (
                &[],
                0,
                Bound::Pointer("CLICK THE BAR MENU", Pointing::Launcher),
            ),
        ];
        // The paired profile's sheet, every row; the direct one lists all
        // but `Super+l`, which it does not bind.
        assert_eq!(
            probes.len(),
            crate::help::rows(true).count(),
            "every help row needs a probe"
        );
        for (probe, row) in probes.iter().zip(crate::help::rows(true)) {
            let (modifiers, code, expected) = probe;
            if let Bound::Pointer(keys, _) = expected {
                assert_eq!(row.keys, *keys);
                assert_eq!(row.action, expected.action());
                continue;
            }
            let mut bindings = KeyBindings {
                attention_enabled: true,
                login: login_answer(&ENROLLED),
                ..KeyBindings::default()
            };
            for modifier in *modifiers {
                bindings.feed(key(*modifier, KEY_PRESS));
            }
            let decision = bindings.feed(key(*code, KEY_PRESS));
            let actual = match (
                decision.command,
                decision.launcher,
                decision.launch,
                decision.help,
                decision.lock,
            ) {
                (Some(command), None, None, None, false) => Bound::Command(command),
                (None, Some(action), None, None, false) => Bound::Launcher(action),
                (None, None, Some(request), None, false) => Bound::Launch(request),
                (None, None, None, Some(action), false) => Bound::Help(action),
                (None, None, None, None, true) => Bound::Lock,
                other => panic!("{} produced {other:?}", row.keys),
            };
            assert_eq!(&actual, expected, "{} / {}", row.keys, row.action);
            // Both COLUMNS are derived from what the dispatch just did, so a
            // row cannot drift in either direction: the keys from the chord
            // that was pressed, the action from the effect it produced.
            assert_eq!(row.keys, spelling(modifiers, *code));
            assert_eq!(row.action, actual.action(), "{}", row.keys);
            assert!(
                decision.forward.is_none(),
                "{} reached the client",
                row.keys
            );
        }
    }

    #[test]
    fn super_slash_toggles_the_sheet_with_or_without_shift() {
        for modifiers in [&[KEY_LEFTMETA][..], &[KEY_LEFTMETA, KEY_LEFTSHIFT][..]] {
            let mut bindings = KeyBindings::default();
            // Bare, the key is the client's text.
            let bare = bindings.feed(key(KEY_SLASH, KEY_PRESS));
            assert!(bare.help.is_none());
            assert!(bare.forward.is_some());
            bindings.feed(key(KEY_SLASH, KEY_RELEASE));
            for modifier in modifiers {
                bindings.feed(key(*modifier, KEY_PRESS));
            }
            let opened = bindings.feed(key(KEY_SLASH, KEY_PRESS));
            assert_eq!(opened.help, Some(HelpAction::Toggle));
            assert!(opened.forward.is_none());
            assert!(bindings.feed(key(KEY_SLASH, KEY_RELEASE)).forward.is_none());
        }
    }

    #[test]
    fn any_non_modifier_key_dismisses_the_sheet_and_runs_no_command() {
        for code in [KEY_T, KEY_V, KEY_2, KEY_ESC, KEY_A, KEY_ENTER, KEY_SLASH] {
            let mut bindings = KeyBindings::default();
            press(&mut bindings, KEY_LEFTMETA);
            bindings.feed(key(KEY_SLASH, KEY_PRESS));
            bindings.settle_help(Some(true));
            bindings.feed(key(KEY_SLASH, KEY_RELEASE));
            // Super still held: a chord behind the sheet closes it and does
            // NOT also run, or reading the bindings would launch a terminal.
            let dismissed = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(dismissed.help, Some(HelpAction::Close), "{code}");
            assert!(dismissed.command.is_none(), "{code}");
            assert!(dismissed.launch.is_none(), "{code}");
            assert!(dismissed.launcher.is_none(), "{code}");
            assert!(dismissed.forward.is_none(), "{code}");
            assert!(bindings.feed(key(code, KEY_RELEASE)).forward.is_none());
        }
    }

    #[test]
    fn a_modifier_does_not_dismiss_the_sheet() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_SLASH, KEY_PRESS));
        bindings.settle_help(Some(true));
        bindings.feed(key(KEY_SLASH, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        // Reading the sheet means letting go of the keyboard, and pressing a
        // modifier is how the next chord STARTS: dismissing on it would eat
        // the modifier and leave the chord's key to act on its own.
        for code in [
            KEY_LEFTMETA,
            KEY_RIGHTMETA,
            KEY_LEFTSHIFT,
            KEY_LEFTCTRL,
            KEY_LEFTALT,
        ] {
            let held = bindings.feed(key(code, KEY_PRESS));
            assert!(held.help.is_none(), "{code} dismissed the sheet");
            assert!(bindings.help_open, "{code}");
        }
        // The chord's own key then closes it and does not run.
        let dismissed = bindings.feed(key(KEY_T, KEY_PRESS));
        assert_eq!(dismissed.help, Some(HelpAction::Close));
        assert!(dismissed.launch.is_none());
    }

    #[test]
    fn the_launcher_outranks_the_sheet_so_both_are_never_up() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_ENTER, KEY_PRESS));
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        // The launcher branch runs first, and `/` is not a character it
        // accepts, so the chord neither opens the sheet nor types.
        let slash = bindings.feed(key(KEY_SLASH, KEY_PRESS));
        assert!(slash.help.is_none());
        assert!(slash.launcher.is_none());
        assert!(slash.forward.is_none());
        assert!(!bindings.help_open);
    }

    #[test]
    fn the_pointer_opens_and_answers_the_launcher_through_the_keyboards_door() {
        let target = Mutex::new(LauncherModelTarget::new());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let feed = |pointer: &mut PointerMotion, events: &[Event]| {
            for event in events {
                apply(&target, *event, 0, &bindings, pointer, None).unwrap();
            }
        };

        // A press the runtime says landed on the bar's button opens the
        // overlay, and the capture follows it as `Super+Enter`'s does.
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Open));
        feed(&mut pointer, &[key(BTN_LEFT, KEY_PRESS), syn(1)]);
        assert!(bindings.lock().unwrap().launcher_open);
        {
            let target = target.lock().unwrap();
            assert_eq!(target.pointer_asks, [false], "closed, so not the card's");
            assert_eq!(target.recording.launcher_actions, [LauncherAction::Open]);
            assert!(target.launcher.visible());
        }
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(2)]);
        // Now the keyboard types into it, as after the chord.
        feed(
            &mut pointer,
            &[key(KEY_A, KEY_PRESS), key(KEY_A, KEY_RELEASE)],
        );
        assert_eq!(
            target.lock().unwrap().recording.launcher_actions.last(),
            Some(&LauncherAction::Insert('a'))
        );

        // While it is up a left press is the card's: withheld from the
        // runtime's report and asked about instead.
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Close));
        let frames_before = target.lock().unwrap().recording.pointer_frames.len();
        feed(&mut pointer, &[key(BTN_LEFT, KEY_PRESS), syn(3)]);
        assert!(!bindings.lock().unwrap().launcher_open);
        {
            let target = target.lock().unwrap();
            assert_eq!(target.pointer_asks, [false, true], "a release asked");
            assert_eq!(
                target.recording.launcher_actions.last(),
                Some(&LauncherAction::Close)
            );
            assert!(target.recording.pointer_frames[frames_before..]
                .iter()
                .all(|(_, _, _, buttons)| buttons.is_empty()));
        }
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(4)]);

        // Another button on the open card is not a choice.
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Open));
        feed(&mut pointer, &[key(BTN_LEFT, KEY_PRESS), syn(5)]);
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(6)]);
        feed(&mut pointer, &[key(BTN_MOUSE + 1, KEY_PRESS), syn(7)]);
        assert_eq!(target.lock().unwrap().pointer_asks.last(), Some(&false));
        assert!(bindings.lock().unwrap().launcher_open);
        feed(&mut pointer, &[key(BTN_MOUSE + 1, KEY_RELEASE), syn(8)]);

        // A row's answer goes through the same door and drops the capture.
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Choose(1)));
        feed(&mut pointer, &[key(BTN_LEFT, KEY_PRESS), syn(9)]);
        assert!(!bindings.lock().unwrap().launcher_open);
        assert_eq!(
            target.lock().unwrap().recording.launcher_actions.last(),
            Some(&LauncherAction::Choose(1))
        );
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(10)]);
        // A report that pressed nothing asks nothing.
        let asked = target.lock().unwrap().pointer_asks.len();
        feed(
            &mut pointer,
            &[key(KEY_A, KEY_PRESS), key(KEY_A, KEY_RELEASE)],
        );
        let motion = Event {
            timestamp: 0,
            time: 0,
            kind: EV_REL,
            code: REL_X,
            value: 5,
        };
        feed(&mut pointer, &[motion, syn(11)]);
        assert_eq!(target.lock().unwrap().pointer_asks.len(), asked);
    }

    #[test]
    fn another_devices_release_is_not_a_press_on_the_open_launcher() {
        let target = Mutex::new(LauncherModelTarget::new());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointers = [PointerMotion::default(), PointerMotion::default()];
        let mut feed = |device: usize, events: &[Event]| {
            for event in events {
                apply(
                    &target,
                    *event,
                    device,
                    &bindings,
                    &mut pointers[device],
                    None,
                )
                .unwrap();
            }
        };
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Open));
        feed(0, &[key(BTN_LEFT, KEY_PRESS), syn(1)]);
        feed(0, &[key(BTN_LEFT, KEY_RELEASE), syn(2)]);
        assert!(bindings.lock().unwrap().launcher_open);
        // Both mice hold left on the card's title, which answers nothing.
        feed(0, &[key(BTN_LEFT, KEY_PRESS), syn(3)]);
        feed(1, &[key(BTN_LEFT, KEY_PRESS), syn(4)]);
        assert_eq!(target.lock().unwrap().pointer_asks, [false, true, true]);
        // The second lets go over a row: that is no press, so nothing asks.
        target
            .lock()
            .unwrap()
            .pointer_answers
            .push_back(Some(LauncherAction::Choose(0)));
        feed(1, &[key(BTN_LEFT, KEY_RELEASE), syn(5)]);
        assert_eq!(target.lock().unwrap().pointer_asks, [false, true, true]);
        assert!(bindings.lock().unwrap().launcher_open);
        assert_eq!(
            target.lock().unwrap().recording.launcher_actions,
            [LauncherAction::Open]
        );
        // The row's answer was never asked for; this press is on the title.
        target.lock().unwrap().pointer_answers.clear();
        // Both hold left again; the second lets go of left and presses its
        // right button in one report. That report pressed something, so it
        // is asked about, but its only LEFT transition is a release: not a
        // press on the card, whatever the seat's changes make of it.
        feed(1, &[key(BTN_LEFT, KEY_PRESS), syn(6)]);
        feed(
            1,
            &[
                key(BTN_LEFT, KEY_RELEASE),
                key(BTN_MOUSE + 1, KEY_PRESS),
                syn(7),
            ],
        );
        assert_eq!(
            target.lock().unwrap().pointer_asks,
            [false, true, true, true, false]
        );
        assert!(bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn the_adapter_routes_the_sheet_and_settles_its_capture() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_SLASH, KEY_PRESS),
            key(KEY_SLASH, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(bindings.lock().unwrap().help_open);
        assert_eq!(target.lock().unwrap().help_actions, [HelpAction::Toggle]);

        for event in [key(KEY_T, KEY_PRESS), key(KEY_T, KEY_RELEASE)] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(!bindings.lock().unwrap().help_open);
        let target = target.lock().unwrap();
        assert_eq!(target.help_actions, [HelpAction::Toggle, HelpAction::Close]);
        assert_eq!(target.launched, []);
        assert_eq!(target.commands, []);
    }

    #[test]
    fn super_t_starts_a_terminal_without_raising_the_overlay() {
        let mut bindings = KeyBindings::default();
        // Bare, the key is the client's text, not a launch.
        let bare = bindings.feed(key(KEY_T, KEY_PRESS));
        assert!(bare.launch.is_none());
        assert!(bare.forward.is_some());
        bindings.feed(key(KEY_T, KEY_RELEASE));
        press(&mut bindings, KEY_RIGHTMETA);
        let chord = bindings.feed(key(KEY_T, KEY_PRESS));
        assert_eq!(chord.launch, Some(LaunchRequest::Terminal));
        assert!(chord.launcher.is_none());
        assert!(chord.command.is_none());
        assert!(chord.forward.is_none());
        // Consumed, so the release does not reach the client either.
        assert!(bindings.feed(key(KEY_T, KEY_RELEASE)).forward.is_none());
    }

    #[test]
    fn an_open_overlay_swallows_every_super_chord() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        // Super is still DOWN. The overlay owns every non-modifier key, so a
        // chord neither runs behind it nor types into its query: `Super+t`
        // must not start a second terminal, `Super+v` must not split.
        for code in [KEY_T, KEY_V, KEY_H, KEY_F, KEY_2, KEY_RIGHT] {
            let held = bindings.feed(key(code, KEY_PRESS));
            assert!(held.launch.is_none(), "{code}");
            assert!(held.command.is_none(), "{code}");
            assert!(held.launcher.is_none(), "{code}");
            assert!(held.forward.is_none(), "{code}");
            bindings.feed(key(code, KEY_RELEASE));
        }
    }

    #[test]
    fn an_unbound_key_under_super_reaches_the_client() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        // Only the bound chords are stolen; everything else is the client's,
        // which is what the terminal's own untranslated-chord rule turns on.
        let unbound = bindings.feed(key(KEY_Q, KEY_PRESS));
        assert!(unbound.command.is_none());
        assert!(unbound.launch.is_none());
        assert!(unbound.launcher.is_none());
        assert!(unbound.forward.is_some());
        assert!(bindings.feed(key(KEY_Q, KEY_RELEASE)).forward.is_some());
    }

    #[test]
    fn the_adapter_hands_a_terminal_chord_to_the_target_without_a_launcher_action() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_T, KEY_PRESS),
            key(KEY_T, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        let target = target.lock().unwrap();
        assert_eq!(target.launched, [LaunchRequest::Terminal]);
        assert_eq!(target.launcher_actions, []);
        assert_eq!(target.commands, []);
    }

    #[test]
    fn launcher_navigation_activation_and_cancel_are_consumed() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        let opened = bindings.feed(key(KEY_ENTER, KEY_PRESS));
        assert_eq!(opened.launcher, Some(LauncherAction::Open));
        assert!(opened.forward.is_none());
        bindings.settle_launcher(Some(true));
        assert!(bindings.feed(key(KEY_ENTER, KEY_RELEASE)).forward.is_none());
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));

        // A key that WOULD be a workspace command becomes text while the
        // overlay is up: the overlay owns every non-modifier key.
        let blocked_command = bindings.feed(key(KEY_2, KEY_PRESS));
        assert!(blocked_command.command.is_none());
        assert!(blocked_command.forward.is_none());
        assert_eq!(blocked_command.launcher, Some(LauncherAction::Insert('2')));
        bindings.feed(key(KEY_2, KEY_RELEASE));

        press(&mut bindings, KEY_LEFTCTRL);
        let next = bindings.feed(key(KEY_N, KEY_PRESS));
        assert_eq!(next.launcher, Some(LauncherAction::Next));
        assert!(next.forward.is_none());
        bindings.feed(key(KEY_N, KEY_RELEASE));
        let previous = bindings.feed(key(KEY_P, KEY_PRESS));
        assert_eq!(previous.launcher, Some(LauncherAction::Previous));
        bindings.feed(key(KEY_P, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTCTRL, KEY_RELEASE));

        let activated = bindings.feed(key(KEY_ENTER, KEY_PRESS));
        assert_eq!(activated.launcher, Some(LauncherAction::Activate));
        bindings.settle_launcher(Some(false));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        assert!(bindings.feed(key(KEY_N, KEY_PRESS)).forward.is_some());

        let mut keypad = KeyBindings {
            launcher_open: true,
            ..KeyBindings::default()
        };
        let activated = keypad.feed(key(KEY_KPENTER, KEY_PRESS));
        assert_eq!(activated.launcher, Some(LauncherAction::Activate));
        assert!(activated.forward.is_none());
        // Both Enters OPEN as well, or the keypad activates something it
        // cannot raise.
        let mut keypad = KeyBindings::default();
        press(&mut keypad, KEY_LEFTMETA);
        let opened = keypad.feed(key(KEY_KPENTER, KEY_PRESS));
        assert_eq!(opened.launcher, Some(LauncherAction::Open));
        assert!(opened.forward.is_none());

        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_RIGHTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_DOWN, KEY_PRESS)).launcher,
            Some(LauncherAction::Next)
        );
        bindings.feed(key(KEY_DOWN, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_UP, KEY_PRESS)).launcher,
            Some(LauncherAction::Previous)
        );
        bindings.feed(key(KEY_UP, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_ESC, KEY_PRESS)).launcher,
            Some(LauncherAction::Close)
        );

        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_ENTER, KEY_PRESS));
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        press(&mut bindings, KEY_LEFTCTRL);
        assert_eq!(
            bindings.feed(key(KEY_G, KEY_PRESS)).launcher,
            Some(LauncherAction::Close)
        );
        assert!(bindings.feed(key(KEY_G, KEY_RELEASE)).forward.is_none());
    }

    #[test]
    fn launcher_text_backspace_and_modified_keys_are_consumed() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));

        for (code, expected) in [
            (KEY_T, 't'),
            (KEY_E, 'e'),
            (KEY_R, 'r'),
            (KEY_M, 'm'),
            (KEY_SPACE, ' '),
            (KEY_1, '1'),
            (KEY_MINUS, '-'),
        ] {
            let decision = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(decision.launcher, Some(LauncherAction::Insert(expected)));
            assert!(decision.forward.is_none());
            assert!(bindings.feed(key(code, KEY_RELEASE)).forward.is_none());
        }
        assert_eq!(
            bindings.feed(key(KEY_BACKSPACE, KEY_PRESS)).launcher,
            Some(LauncherAction::Backspace)
        );
        bindings.feed(key(KEY_BACKSPACE, KEY_RELEASE));

        press(&mut bindings, KEY_LEFTCTRL);
        let modified = bindings.feed(key(KEY_A, KEY_PRESS));
        assert!(modified.launcher.is_none());
        assert!(modified.forward.is_none());
        bindings.feed(key(KEY_A, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTCTRL, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_A, KEY_PRESS)).launcher,
            Some(LauncherAction::Insert('a'))
        );
    }

    #[test]
    fn launcher_character_map_covers_ascii_registry_input() {
        let letters = [
            KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L,
            KEY_M, KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X,
            KEY_Y, KEY_Z,
        ];
        let mapped: String = letters.into_iter().filter_map(launcher_character).collect();
        assert_eq!(mapped, "abcdefghijklmnopqrstuvwxyz");
        let digits: String = [
            KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0,
        ]
        .into_iter()
        .filter_map(launcher_character)
        .collect();
        assert_eq!(digits, "1234567890");
        assert_eq!(launcher_character(KEY_SPACE), Some(' '));
        assert_eq!(launcher_character(KEY_MINUS), Some('-'));
        assert_eq!(launcher_character(KEY_ENTER), None);
    }

    #[test]
    fn a_chord_is_read_from_the_modifier_held_now_not_from_one_released_earlier() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            tap(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        // With Super up the same key is the client's to type: nothing
        // outlives the modifier.
        assert_eq!(tap(&mut bindings, KEY_V), None);
        assert_eq!(tap(&mut bindings, KEY_2), None);
        press(&mut bindings, KEY_RIGHTMETA);
        assert_eq!(tap(&mut bindings, KEY_2), Some(Command::SwitchWorkspace(2)));
    }

    #[test]
    fn autorepeat_never_runs_a_command() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        // A held chord repeats at the driver, and a compositor acting on that
        // would split the layout once per repeat interval.
        assert_eq!(bindings.feed(key(KEY_RIGHT, KEY_REPEAT)).command, None);
        assert_eq!(bindings.feed(key(KEY_V, KEY_REPEAT)).command, None);
        assert_eq!(
            press(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
        assert_eq!(bindings.feed(key(KEY_V, KEY_REPEAT)).command, None);
        bindings.feed(key(KEY_V, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
    }

    #[test]
    fn the_wheel_codes_are_the_kernels_and_not_each_others() {
        // Every other reference to these is BY NAME, so swapping the two
        // values leaves the whole suite green while a vertical wheel scrolls
        // the surface sideways — the same failure `PointerAxis::wire` is
        // pinned against on the Wayland side, and the same argument: a wrong
        // axis is a well-formed scroll and nothing observable says so.
        assert_eq!(REL_HWHEEL, 6);
        assert_eq!(REL_WHEEL, 8);
        // And no hi-res code is DECLARED, rather than merely unmatched: a
        // device sends both resolutions, so reading 11 or 12 as well would
        // scroll twice. Read out of the module's own text, as `main.rs`'s
        // scans are, since what is asserted is that no arm exists to drive.
        // Spelled in halves because that text includes this test: written
        // whole, the needle would be its own first occurrence.
        let declaration = ["_HI_RES", ": u16"].concat();
        assert!(
            !include_str!("input.rs").contains(&declaration),
            "a high-resolution wheel code was declared; it would scroll twice"
        );
    }

    #[test]
    fn a_report_that_only_turns_the_wheel_is_still_a_frame() {
        // The case the silence test exists to let through: a notch moves the
        // pointer nowhere and presses nothing, so a reader asking only about
        // motion and buttons would drop every scroll that arrived alone —
        // which is what an ordinary scroll IS.
        let mut pointer = PointerMotion::default();
        let rel = |code, value| Event {
            timestamp: 0,
            time: 0,
            kind: EV_REL,
            code,
            value,
        };
        assert_eq!(pointer.feed_relative(rel(REL_WHEEL, -1)), None);
        let frame = pointer
            .feed_relative(Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            })
            .expect("a wheel-only report produced no frame");
        assert_eq!(frame.place, PointerPlace::By { dx: 0, dy: 0 });
        assert!(frame.buttons.is_empty());
        assert_eq!(
            frame.scroll,
            PointerScroll {
                vertical: -1,
                horizontal: 0
            }
        );

        // Notches SUM within a report as the deltas do, and the two axes are
        // carried separately: a tilting wheel reports both, and a frame that
        // added them would scroll diagonally by their difference.
        pointer.feed_relative(rel(REL_WHEEL, 2));
        pointer.feed_relative(rel(REL_WHEEL, 1));
        pointer.feed_relative(rel(REL_HWHEEL, -4));
        let frame = pointer
            .feed_relative(Event {
                timestamp: 5_000_000,
                time: 5,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            })
            .unwrap();
        assert_eq!(
            frame.scroll,
            PointerScroll {
                vertical: 3,
                horizontal: -4
            }
        );

        // And the accumulator is emptied by the frame that took it, or the
        // next report would scroll again by a wheel nobody turned.
        assert_eq!(
            pointer.feed_relative(Event {
                timestamp: 6_000_000,
                time: 6,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }),
            None
        );
    }

    #[test]
    fn pointer_motion_is_coalesced_at_syn_report_and_saturates() {
        let mut pointer = PointerMotion::default();
        assert_eq!(
            pointer.feed_relative(Event {
                timestamp: 0,
                time: 0,
                kind: EV_REL,
                code: REL_X,
                value: i32::MAX
            }),
            None
        );
        pointer.feed_relative(Event {
            timestamp: 0,
            time: 0,
            kind: EV_REL,
            code: REL_X,
            value: 2,
        });
        pointer.feed_relative(Event {
            timestamp: 0,
            time: 0,
            kind: EV_REL,
            code: REL_Y,
            value: -7,
        });
        assert_eq!(
            pointer.feed_relative(Event {
                timestamp: 9_000_000,
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            Some(PointerFrame {
                time: 9,
                place: PointerPlace::By {
                    dx: i32::MAX,
                    dy: -7
                },
                buttons: Vec::new(),
                scroll: PointerScroll::default(),
            })
        );
        assert_eq!(
            pointer.feed_relative(Event {
                timestamp: 0,
                time: 0,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            None
        );
    }

    #[test]
    fn an_absolute_overflow_recovers_where_the_device_is_rather_than_where_it_was() {
        // An overflow reaches the same recovery a dropped batch does, and for
        // the same reason: the report it abandons took that report's own
        // EV_ABS values with it, so the held position is stale in a way no
        // later record announces. The sibling test below drives a RELATIVE
        // device, so nothing else covers the absolute arm of that path.
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };
        let asked = std::cell::Cell::new(0usize);
        let mut resync = || {
            asked.set(asked.get().saturating_add(1));
            Some(moved)
        };
        let mut state = DeviceState::new(Some(tablet()), AbsoluteKind::Tablet, &mut resync, false);
        // A place first, so the recovery has a stale position to replace
        // rather than an absent one.
        for event in [abs(1, ABS_X, 100), abs(1, ABS_Y, 100), syn(1)] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        for index in 0..=MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
            let value = if index % 2 == 0 {
                KEY_PRESS
            } else {
                KEY_RELEASE
            };
            apply_device_event(&target, key(BTN_MOUSE, value), 0, &bindings, &mut state).unwrap();
        }
        assert!(state.dropped);
        assert_eq!(
            asked.get(),
            0,
            "the device was asked before the window ended"
        );

        apply_device_event(&target, syn(9), 0, &bindings, &mut state).unwrap();
        assert!(!state.dropped);
        assert_eq!(
            asked.get(),
            1,
            "an overflow recovery asked {} times",
            asked.get()
        );
        assert_eq!(
            target.lock().unwrap().pointer_places.last(),
            Some(&(9, over(20000, 32767), over(30000, 40000), Vec::new())),
            "an overflow left the cursor where the device used to be"
        );
    }

    #[test]
    fn oversized_pointer_report_is_dropped_and_recovers_at_next_sync() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, false);
        for index in 0..=MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
            apply_device_event(
                &target,
                key(
                    BTN_MOUSE,
                    if index % 2 == 0 {
                        KEY_PRESS
                    } else {
                        KEY_RELEASE
                    },
                ),
                0,
                &bindings,
                &mut state,
            )
            .unwrap();
        }
        assert!(state.dropped);
        assert!(state.pointer.buttons.is_empty());
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        for event in [
            Event {
                timestamp: 7_000_000,
                time: 7,
                kind: EV_REL,
                code: REL_X,
                value: 99,
            },
            Event {
                timestamp: 8_000_000,
                time: 8,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            Event {
                timestamp: 9_000_000,
                time: 9,
                kind: EV_REL,
                code: REL_X,
                value: 3,
            },
            Event {
                timestamp: 10_000_000,
                time: 10,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(10, 3, 0, Vec::new())]
        );
    }

    #[test]
    fn pointer_buttons_are_logical_across_devices_and_flush_with_syn_report() {
        let target = Arc::new(Mutex::new(RecordingTarget::default()));
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        for (device, event) in [
            (0, key(BTN_MOUSE, KEY_PRESS)),
            (1, key(BTN_MOUSE, KEY_PRESS)),
            (
                0,
                Event {
                    timestamp: 5_000_000,
                    time: 5,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (
                1,
                Event {
                    timestamp: 5_000_000,
                    time: 5,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (0, key(BTN_MOUSE, KEY_RELEASE)),
            (
                0,
                Event {
                    timestamp: 6_000_000,
                    time: 6,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (1, key(BTN_MOUSE, KEY_RELEASE)),
            (
                1,
                Event {
                    timestamp: 7_000_000,
                    time: 7,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
        ] {
            let pointer = if device == 0 { &mut first } else { &mut second };
            apply(target.as_ref(), event, device, &bindings, pointer, None).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    5,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 5,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    7,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 7,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn launcher_capture_drops_new_pointer_buttons_but_keeps_motion_local() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let apply_event = |event, pointer: &mut PointerMotion| {
            apply(&target, event, 0, &bindings, pointer, None).unwrap();
        };
        apply_event(key(KEY_LEFTMETA, KEY_PRESS), &mut pointer);
        apply_event(key(KEY_ENTER, KEY_PRESS), &mut pointer);
        apply_event(key(KEY_ENTER, KEY_PRESS), &mut pointer);
        assert!(bindings.lock().unwrap().launcher_open);

        apply_event(key(BTN_MOUSE, KEY_PRESS), &mut pointer);
        apply_event(
            Event {
                timestamp: 1_000_000,
                time: 1,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        apply_event(
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 5,
            },
            &mut pointer,
        );
        apply_event(
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(2, 5, 0, Vec::new())]
        );

        apply_event(key(BTN_MOUSE, KEY_RELEASE), &mut pointer);
        apply_event(
            Event {
                timestamp: 3_000_000,
                time: 3,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert_eq!(target.lock().unwrap().pointer_frames.len(), 1);
    }

    #[test]
    fn replacement_button_press_survives_cross_device_syn_reordering() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        let syn = |time| Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };

        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(&target, syn(1), 0, &bindings, &mut first, None).unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            1,
            &bindings,
            &mut second,
            None,
        )
        .unwrap();
        apply(&target, syn(2), 1, &bindings, &mut second, None).unwrap();
        apply(&target, syn(3), 0, &bindings, &mut first, None).unwrap();
        assert_eq!(target.lock().unwrap().pointer_frames.len(), 1);

        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            1,
            &bindings,
            &mut second,
            None,
        )
        .unwrap();
        apply(&target, syn(4), 1, &bindings, &mut second, None).unwrap();
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    1,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 1,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    4,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 4,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn a_button_edge_waits_for_its_own_device_syn_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        let syn = |time| Event {
            timestamp: u128::from(time) * 1_000_000,
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };

        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(&target, syn(1), 1, &bindings, &mut second, None).unwrap();
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        apply(&target, syn(2), 0, &bindings, &mut first, None).unwrap();
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(
                2,
                0,
                0,
                vec![PointerButtonInput {
                    time: 2,
                    button: u32::from(BTN_MOUSE),
                    state: PointerButtonState::Pressed,
                }],
            )]
        );
    }

    #[test]
    fn adapter_dispatches_parsed_commands_and_complete_pointer_frames() {
        let target = Arc::new(Mutex::new(RecordingTarget::default()));
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_LEFT, KEY_PRESS),
            Event {
                timestamp: 0,
                time: 0,
                kind: EV_REL,
                code: REL_X,
                value: 3,
            },
            Event {
                timestamp: 0,
                time: 0,
                kind: EV_REL,
                code: REL_Y,
                value: -2,
            },
            Event {
                timestamp: 17_000_000,
                time: 17,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply(target.as_ref(), event, 0, &bindings, &mut pointer, None).unwrap();
        }
        let target = target.lock().unwrap();
        assert_eq!(target.commands, [Command::Focus(Direction::Left)]);
        assert_eq!(
            target.keys,
            [KeyInput {
                time: 0,
                key: u32::from(KEY_LEFTMETA),
                state: KeyState::Pressed,
            }]
        );
        assert_eq!(
            target.modifiers,
            [ModifierState {
                depressed: MOD_LOGO,
                ..ModifierState::default()
            }]
        );
        assert_eq!(
            target.keyboard_calls,
            [
                KeyboardCall::Key(KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Pressed,
                }),
                KeyboardCall::Modifiers(ModifierState {
                    depressed: MOD_LOGO,
                    ..ModifierState::default()
                }),
            ]
        );
        assert_eq!(target.pointer_frames, [(17, 3, -2, Vec::new())]);
    }

    #[test]
    fn adapter_delivers_launcher_actions_in_input_order() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
            key(KEY_DOWN, KEY_PRESS),
            key(KEY_DOWN, KEY_RELEASE),
            key(KEY_ENTER, KEY_PRESS),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().launcher_actions,
            [
                LauncherAction::Open,
                LauncherAction::Next,
                LauncherAction::Activate,
            ]
        );
    }

    #[test]
    fn empty_activation_keeps_input_captured_until_backspace_recovers() {
        let target = Mutex::new(LauncherModelTarget::new());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
            key(KEY_Z, KEY_PRESS),
            key(KEY_Z, KEY_RELEASE),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_BACKSPACE, KEY_PRESS),
            key(KEY_BACKSPACE, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        {
            let target = target.lock().unwrap();
            assert!(target.launcher.visible());
            assert_eq!(target.launcher.query(), "");
            assert_eq!(
                target.recording.launcher_actions,
                [
                    LauncherAction::Open,
                    LauncherAction::Insert('z'),
                    LauncherAction::Activate,
                    LauncherAction::Backspace,
                ]
            );
            assert!(target.recording.keys.iter().all(|input| {
                !matches!(
                    input.key,
                    key if key == u32::from(KEY_Z)
                        || key == u32::from(KEY_ENTER)
                        || key == u32::from(KEY_BACKSPACE)
                )
            }));
        }
        assert!(bindings.lock().unwrap().launcher_open);

        for event in [key(KEY_ENTER, KEY_PRESS), key(KEY_ENTER, KEY_RELEASE)] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(!target.lock().unwrap().launcher.visible());
        assert!(!bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn failed_open_repaint_never_enables_launcher_capture() {
        let path = std::env::temp_dir().join(format!(
            "td-input-launcher-open-failure-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 640, 240, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        })
        .unwrap();
        let target = Mutex::new(LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches: LaunchBackend::Direct(launches),
            secret_attempt: None,
            seat: Seat::default(),
        });
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(KEY_LEFTMETA, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        runtime.lock().unwrap().fail_next_repaint();
        assert!(apply(
            &target,
            key(KEY_ENTER, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None
        )
        .is_err());
        assert!(!runtime.lock().unwrap().launcher_visible());
        assert!(!bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn a_click_on_the_bars_button_opens_the_live_launcher_and_one_off_the_card_closes_it() {
        let path = std::env::temp_dir().join(format!(
            "td-input-launcher-button-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 640, 480, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        })
        .unwrap();
        let target = Mutex::new(LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches: LaunchBackend::Direct(launches),
            secret_attempt: None,
            seat: Seat::default(),
        });
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let feed = |pointer: &mut PointerMotion, events: &[Event]| {
            for event in events {
                apply(&target, *event, 0, &bindings, pointer, None).unwrap();
            }
        };
        let rel = |code: u16, value: i32| Event {
            timestamp: 0,
            time: 0,
            kind: EV_REL,
            code,
            value,
        };

        // The pointer starts in the output's corner, which is the button.
        feed(&mut pointer, &[key(BTN_LEFT, KEY_PRESS), syn(1)]);
        assert!(runtime.lock().unwrap().launcher_visible());
        assert!(bindings.lock().unwrap().launcher_open);
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(2)]);
        assert!(runtime.lock().unwrap().launcher_visible());
        // The keyboard is the overlay's, as after `Super+Enter`.
        feed(
            &mut pointer,
            &[key(KEY_A, KEY_PRESS), key(KEY_A, KEY_RELEASE)],
        );
        assert_eq!(runtime.lock().unwrap().launcher_query(), "a");

        // Down the left edge, off the centred card: the press closes it.
        feed(
            &mut pointer,
            &[rel(REL_Y, 200), key(BTN_LEFT, KEY_PRESS), syn(3)],
        );
        assert!(!runtime.lock().unwrap().launcher_visible());
        assert!(!bindings.lock().unwrap().launcher_open);
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(4)]);
        assert!(!runtime.lock().unwrap().launcher_visible());

        // Back to the button, and then a click on a row: the development
        // registry's third is the close entry, which launches nothing.
        feed(
            &mut pointer,
            &[rel(REL_Y, -1000), key(BTN_LEFT, KEY_PRESS), syn(5)],
        );
        feed(&mut pointer, &[key(BTN_LEFT, KEY_RELEASE), syn(6)]);
        assert!(runtime.lock().unwrap().launcher_visible());
        let (x, y) = crate::launcher::row_centre(640, 480, 2);
        feed(
            &mut pointer,
            &[
                rel(REL_X, i32::try_from(x).unwrap()),
                rel(REL_Y, i32::try_from(y).unwrap()),
                key(BTN_LEFT, KEY_PRESS),
                syn(7),
            ],
        );
        assert!(!runtime.lock().unwrap().launcher_visible());
        assert!(!bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn failed_pointer_repaint_retains_state_for_device_cleanup() {
        let path = std::env::temp_dir().join(format!(
            "td-input-pointer-repaint-failure-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        })
        .unwrap();
        let target = Mutex::new(LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches: LaunchBackend::Direct(launches),
            secret_attempt: None,
            seat: Seat::default(),
        });
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        // Along the bar to its status line, where a press acts on nothing:
        // on the launcher's button or a workspace it would paint at once
        // rather than owe the paint.
        apply(
            &target,
            Event {
                timestamp: 3_000_000,
                time: 3,
                kind: EV_REL,
                code: REL_X,
                value: 100,
            },
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        runtime.lock().unwrap().fail_next_repaint();
        // The report itself now only owes the paint, so the failure surfaces at
        // the batch flush -- and must still leave the press for cleanup.
        apply(
            &target,
            Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        assert!(flush_target(&target).is_err());
        {
            let bindings = bindings.lock().unwrap();
            assert!(bindings.pointer_pressed.contains(&(0, BTN_MOUSE)));
            assert!(bindings.pointer_forwarded.contains(&BTN_MOUSE));
        }

        release_device(&target, 0, &bindings, 5).unwrap();

        let bindings = bindings.lock().unwrap();
        assert!(bindings.pointer_pressed.is_empty());
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn ordinary_keys_and_modifiers_forward_but_shortcut_pairs_do_not() {
        let mut bindings = KeyBindings::default();
        let ordinary = bindings.feed(Event {
            timestamp: 44_000_000,
            time: 44,
            kind: EV_KEY,
            code: 30,
            value: KEY_PRESS,
        });
        assert_eq!(
            ordinary.forward,
            Some(KeyInput {
                time: 44,
                key: 30,
                state: KeyState::Pressed,
            })
        );
        assert_eq!(ordinary.command, None);

        let meta = bindings.feed(key(KEY_LEFTMETA, KEY_PRESS));
        assert!(meta.forward.is_some());
        assert_eq!(
            meta.modifiers,
            Some(ModifierState {
                depressed: MOD_LOGO,
                ..ModifierState::default()
            })
        );
        let shortcut = bindings.feed(key(KEY_F, KEY_PRESS));
        assert_eq!(shortcut.command, Some(Command::ToggleFullscreen));
        assert_eq!(shortcut.forward, None);
        assert_eq!(bindings.feed(key(KEY_F, KEY_RELEASE)).forward, None);
        let released = bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        assert!(released.forward.is_some());
        assert_eq!(released.modifiers, Some(ModifierState::default()));
    }

    #[test]
    fn mouse_buttons_and_keys_outside_the_xkb_range_are_not_keyboard_events() {
        let mut bindings = KeyBindings::default();
        for code in [MAX_XKB_EVDEV_KEY + 1, 0x100, 0x110] {
            let decision = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(decision.command, None);
            assert_eq!(decision.forward, None);
            assert_eq!(decision.modifiers, None);
        }
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_PRESS)), None);
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_REPEAT)), None);
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_RELEASE)), None);
        assert_eq!(
            pointer.feed_relative(Event {
                timestamp: 9_000_000,
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }),
            Some(PointerFrame {
                time: 9,
                place: PointerPlace::By { dx: 0, dy: 0 },
                buttons: vec![
                    PointerButtonTransition {
                        code: BTN_MOUSE,
                        pressed: true,
                    },
                    PointerButtonTransition {
                        code: BTN_MOUSE,
                        pressed: false,
                    },
                ],
                scroll: PointerScroll::default(),
            })
        );
    }

    #[test]
    fn special_evdev_codes_match_the_bundled_xkb_keycodes() {
        let maximum = format!("maximum = {};", u32::from(MAX_XKB_EVDEV_KEY) + 8);
        assert!(crate::keyboard::XKB_KEYMAP.contains(&maximum), "{maximum}");
        for (name, code) in [
            ("AE01", KEY_1),
            ("AE09", KEY_9),
            ("AD05", KEY_T),
            ("AD10", KEY_P),
            ("AC04", KEY_F),
            ("AC06", KEY_H),
            ("AB02", KEY_X),
            ("AB04", KEY_V),
            ("AB05", KEY_B),
            ("AB06", KEY_N),
            ("RTRN", KEY_ENTER),
            ("UP", KEY_UP),
            ("LEFT", KEY_LEFT),
            ("RGHT", KEY_RIGHT),
            ("DOWN", KEY_DOWN),
            ("LCTL", KEY_LEFTCTRL),
            ("RCTL", KEY_RIGHTCTRL),
            ("LFSH", KEY_LEFTSHIFT),
            ("RTSH", KEY_RIGHTSHIFT),
            ("LALT", KEY_LEFTALT),
            ("RALT", KEY_RIGHTALT),
            ("CAPS", KEY_CAPSLOCK),
            ("NMLK", KEY_NUMLOCK),
            ("KPEN", KEY_KPENTER),
            ("LWIN", KEY_LEFTMETA),
            ("RWIN", KEY_RIGHTMETA),
        ] {
            let declaration = format!("<{name}> = {};", u32::from(code) + 8);
            assert!(
                crate::keyboard::XKB_KEYMAP.contains(&declaration),
                "{declaration}"
            );
        }
    }

    #[test]
    fn both_sides_contribute_to_the_xkb_modifier_mask() {
        let mut bindings = KeyBindings::default();
        assert_eq!(
            bindings.feed(key(KEY_LEFTSHIFT, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTCTRL, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_CONTROL,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTMETA, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_CONTROL | MOD_LOGO,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTCTRL, KEY_RELEASE)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_LOGO,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn event_devices_contribute_to_one_logical_keyboard_state() {
        let mut bindings = KeyBindings::default();
        let first_shift = bindings.feed_device(3, key(KEY_LEFTSHIFT, KEY_PRESS));
        assert!(first_shift.forward.is_some());
        assert_eq!(
            first_shift.modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
        );
        let second_shift = bindings.feed_device(8, key(KEY_LEFTSHIFT, KEY_PRESS));
        assert_eq!(second_shift.forward, None);
        assert_eq!(second_shift.modifiers, None);

        let alt = bindings.feed_device(8, key(KEY_RIGHTALT, KEY_PRESS));
        assert!(alt.forward.is_some());
        assert_eq!(
            alt.modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_ALT,
                ..ModifierState::default()
            })
        );
        let first_release = bindings.feed_device(3, key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert_eq!(first_release.forward, None);
        assert_eq!(first_release.modifiers, None);
        let last_release = bindings.feed_device(8, key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert!(last_release.forward.is_some());
        assert_eq!(
            last_release.modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn duplicate_and_unmatched_device_transitions_are_suppressed() {
        let mut bindings = KeyBindings::default();
        let press = key(30, KEY_PRESS);
        assert!(bindings.feed_device(1, press).forward.is_some());
        assert_eq!(bindings.feed_device(1, press).forward, None);
        assert_eq!(bindings.feed_device(2, press).forward, None);
        assert_eq!(bindings.feed_device(9, key(31, KEY_RELEASE)).forward, None);
        assert_eq!(bindings.feed_device(1, key(30, KEY_RELEASE)).forward, None);
        assert!(bindings
            .feed_device(2, key(30, KEY_RELEASE))
            .forward
            .is_some());
    }

    #[test]
    fn a_second_physical_press_cannot_retrigger_a_logical_chord() {
        let mut bindings = KeyBindings::default();
        bindings.feed_device(1, key(KEY_LEFTMETA, KEY_PRESS));
        assert_eq!(
            bindings.feed_device(1, key(KEY_F, KEY_PRESS)).command,
            Some(Command::ToggleFullscreen)
        );
        let duplicate = bindings.feed_device(2, key(KEY_F, KEY_PRESS));
        assert_eq!(duplicate.command, None);
        assert_eq!(duplicate.forward, None);
        assert_eq!(
            bindings.feed_device(1, key(KEY_F, KEY_RELEASE)).forward,
            None
        );
        assert_eq!(
            bindings.feed_device(2, key(KEY_F, KEY_RELEASE)).forward,
            None
        );

        bindings.feed_device(1, key(KEY_2, KEY_PRESS));
        let duplicate = bindings.feed_device(2, key(KEY_2, KEY_PRESS));
        assert_eq!(duplicate.command, None);
        assert_eq!(duplicate.forward, None);
        assert_eq!(
            bindings.feed_device(2, key(KEY_3, KEY_PRESS)).command,
            Some(Command::SwitchWorkspace(3))
        );
    }

    #[test]
    fn closing_an_event_device_releases_only_its_contribution() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first_pointer = PointerMotion::default();
        let mut second_pointer = PointerMotion::default();
        for (device, event) in [
            (1, key(KEY_LEFTSHIFT, KEY_PRESS)),
            (1, key(30, KEY_PRESS)),
            (2, key(KEY_RIGHTALT, KEY_PRESS)),
        ] {
            let pointer = if device == 1 {
                &mut first_pointer
            } else {
                &mut second_pointer
            };
            apply(&target, event, device, &bindings, pointer, None).unwrap();
        }
        release_device(&target, 1, &bindings, 77).unwrap();

        let target = target.lock().unwrap();
        assert_eq!(
            target.keys,
            [
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTSHIFT),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 0,
                    key: 30,
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_RIGHTALT),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 77,
                    key: 30,
                    state: KeyState::Released,
                },
                KeyInput {
                    time: 77,
                    key: u32::from(KEY_LEFTSHIFT),
                    state: KeyState::Released,
                },
            ]
        );
        assert_eq!(
            target.modifiers.last(),
            Some(&ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn device_teardown_never_forwards_another_devices_press() {
        let target = Mutex::new(RecordingTarget::default());
        let mut state = KeyBindings {
            launcher_open: true,
            ..KeyBindings::default()
        };
        state.pointer_pressed.insert((1, BTN_MOUSE));
        state.pointer_pressed.insert((2, BTN_MOUSE));
        state.settle_launcher(Some(false));
        let bindings = Mutex::new(state);

        release_device(&target, 1, &bindings, 77).unwrap();

        assert!(target.lock().unwrap().pointer_frames.is_empty());
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.pointer_pressed, BTreeSet::from([(2, BTN_MOUSE)]));
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn device_teardown_commits_releases_before_delivery_failure() {
        let target = Mutex::new(RecordingTarget {
            key_error: Some("injected key failure".into()),
            pointer_error: Some("injected pointer failure".into()),
            ..RecordingTarget::default()
        });
        let mut state = KeyBindings::default();
        state.pressed.insert((1, KEY_LEFTSHIFT));
        state.forwarded.insert((1, KEY_LEFTSHIFT));
        state.pointer_pressed.insert((1, BTN_MOUSE));
        state.pointer_forwarded.insert(BTN_MOUSE);
        let bindings = Mutex::new(state);

        let error = release_device(&target, 1, &bindings, 77).unwrap_err();
        assert!(error.contains("injected key failure"));
        assert!(error.contains("injected pointer failure"));

        let target = target.lock().unwrap();
        assert_eq!(
            target.pointer_frames,
            [(
                77,
                0,
                0,
                vec![PointerButtonInput {
                    time: 77,
                    button: u32::from(BTN_MOUSE),
                    state: PointerButtonState::Released,
                }],
            )]
        );
        assert_eq!(
            target.keys.last(),
            Some(&KeyInput {
                time: 77,
                key: u32::from(KEY_LEFTSHIFT),
                state: KeyState::Released,
            })
        );
        assert_eq!(target.modifiers.last(), Some(&ModifierState::default()));
        drop(target);
        let bindings = bindings.lock().unwrap();
        assert!(bindings.pressed.is_empty());
        assert!(bindings.forwarded.is_empty());
        assert!(bindings.pointer_pressed.is_empty());
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn unrelated_device_teardown_does_not_release_a_held_modifier() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(KEY_LEFTMETA, KEY_PRESS),
            1,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        release_device(&target, 2, &bindings, 17).unwrap();
        apply(
            &target,
            key(KEY_2, KEY_PRESS),
            1,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        assert_eq!(
            target.lock().unwrap().commands,
            [Command::SwitchWorkspace(2)]
        );
    }

    #[test]
    fn syn_dropped_releases_state_and_ignores_events_until_the_next_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, false);
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_V, KEY_PRESS),
            Event {
                timestamp: 3_000_000,
                time: 3,
                kind: EV_REL,
                code: REL_X,
                value: 9,
            },
            Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            key(30, KEY_PRESS),
            Event {
                timestamp: 6_000_000,
                time: 6,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            key(KEY_2, KEY_PRESS),
            Event {
                timestamp: 8_000_000,
                time: 8,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 5, &bindings, &mut state).unwrap();
        }

        let target = target.lock().unwrap();
        assert_eq!(
            target.commands,
            [Command::SetPresentation(Presentation::Stacked)]
        );
        assert_eq!(
            target.keys,
            [
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 4,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Released,
                },
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_2),
                    state: KeyState::Pressed,
                },
            ]
        );
        assert_eq!(
            target.modifiers,
            [
                ModifierState {
                    depressed: MOD_LOGO,
                    ..ModifierState::default()
                },
                ModifierState::default(),
            ]
        );
        assert!(target.pointer_frames.is_empty());
    }

    #[test]
    fn syn_dropped_discards_partial_pointer_data_and_releases_forwarded_buttons() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, false);
        for event in [
            key(BTN_MOUSE, KEY_PRESS),
            Event {
                timestamp: 1_000_000,
                time: 1,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            key(BTN_MOUSE, KEY_RELEASE),
            Event {
                timestamp: 2_000_000,
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 20,
            },
            Event {
                timestamp: 3_000_000,
                time: 3,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            Event {
                timestamp: 4_000_000,
                time: 4,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 5, &bindings, &mut state).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    1,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 1,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    3,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 3,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn alt_is_depressed_and_caps_and_num_are_locked_modifiers() {
        let mut bindings = KeyBindings::default();
        assert_eq!(
            bindings.feed(key(KEY_LEFTALT, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_CAPS,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_NUMLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_CAPS | MOD_NUM,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_RELEASE)).modifiers,
            None
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_NUM,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn lock_keys_toggle_once_across_multiple_event_devices() {
        for (code, mask) in [(KEY_CAPSLOCK, MOD_CAPS), (KEY_NUMLOCK, MOD_NUM)] {
            let mut bindings = KeyBindings::default();
            let first = bindings.feed_device(1, key(code, KEY_PRESS));
            assert!(first.forward.is_some());
            assert_eq!(first.modifiers.unwrap().locked, mask);
            let second = bindings.feed_device(2, key(code, KEY_PRESS));
            assert_eq!(second.forward, None);
            assert_eq!(second.modifiers, None);
            let first_release = bindings.feed_device(1, key(code, KEY_RELEASE));
            assert_eq!(first_release.forward, None);
            assert_eq!(first_release.modifiers, None);
            let second_release = bindings.feed_device(2, key(code, KEY_RELEASE));
            assert!(second_release.forward.is_some());
            assert_eq!(second_release.modifiers, None);

            bindings.feed_device(1, key(code, KEY_PRESS));
            assert_eq!(bindings.modifiers().locked, 0);
        }
    }
    #[test]
    fn physical_attention_notice_failure_retains_capture_and_accepts_escape() {
        let cleanup = Cleanup(std::env::temp_dir().join(format!(
            "td-input-secret-notice-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )));
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 800, 600, 3200).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().enable_attention(true);
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        })
        .unwrap();
        let mut target = LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches: LaunchBackend::Direct(launches),
            secret_attempt: None,
            seat: Seat::default(),
        };
        let mut bindings = KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        };
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        runtime.lock().unwrap().fail_next_repaint();
        let decision = bindings.feed(key(KEY_U, KEY_PRESS));
        deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        assert!(target.secret_attempt.is_some());
        assert!(bindings.attention != AttentionState::Closed);
        let decision = bindings.feed(key(KEY_U, KEY_RELEASE));
        deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        assert!(bindings.feed(key(KEY_R, KEY_PRESS)).secret.is_none());
        bindings.feed(key(KEY_R, KEY_RELEASE));
        for event in [key(KEY_ESC, KEY_PRESS), key(KEY_ESC, KEY_RELEASE)] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        assert!(bindings.attention == AttentionState::Closed);
        assert!(target.secret_attempt.is_none());
    }

    /// The seat's release closes only the screen its own attempt opened,
    /// waits for held keys as Escape does, and returns the keyboard to the
    /// session; a stale attempt or a closed screen is left alone.
    #[test]
    fn the_seat_releases_attention_only_for_its_own_attempt() {
        let cleanup = Cleanup(std::env::temp_dir().join(format!(
            "td-input-release-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )));
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 800, 600, 3200).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().enable_attention(true);
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: Some(PathBuf::from("/bin/td-ui-demo")),
            terminal: PathBuf::from("/bin/td-term"),
            application: None,
        })
        .unwrap();
        let bindings = Arc::new(Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        }));
        let target = Arc::new_cyclic(|target| {
            Mutex::new(LiveInputTarget {
                runtime: Arc::clone(&runtime),
                launches: LaunchBackend::Direct(launches),
                secret_attempt: None,
                seat: Seat {
                    bindings: Arc::downgrade(&bindings),
                    target: target.clone(),
                },
            })
        });
        let feed = |events: &[Event]| {
            let mut bindings = bindings.lock().unwrap();
            let mut target = target.lock().unwrap();
            for event in events {
                let decision = bindings.feed(*event);
                deliver_key_decision(&mut *target, &mut bindings, decision).unwrap();
            }
        };
        feed(&[
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
            key(KEY_I, KEY_PRESS),
            key(KEY_I, KEY_RELEASE),
        ]);
        let attempt = Arc::clone(target.lock().unwrap().secret_attempt.as_ref().unwrap());
        let release = target.lock().unwrap().seat.clone();
        // Another attempt's release leaves this screen open.
        let stale = crate::secret_client::Attempt::new(
            test_origin(),
            Arc::clone(&runtime),
            crate::secret_client::Selection::Install,
        );
        release.release(&stale).unwrap();
        assert!(bindings.lock().unwrap().attention == AttentionState::Open);
        // Consent's Enter, and another, reach only the screen.
        feed(&[
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
        ]);
        // A drain that fails leaves the screen open as it was, for Escape.
        runtime.lock().unwrap().enable_attention(false);
        assert!(release.release(&attempt).is_err());
        {
            let bindings = bindings.lock().unwrap();
            assert!(bindings.attention == AttentionState::Open);
            assert_eq!(bindings.keyless_close, None);
            assert_eq!(bindings.settled, None);
        }
        runtime.lock().unwrap().enable_attention(true);
        // A held key keeps the drain until it is released.
        feed(&[key(KEY_A, KEY_PRESS)]);
        release.release(&attempt).unwrap();
        assert!(bindings.lock().unwrap().attention == AttentionState::Draining);
        feed(&[key(KEY_A, KEY_RELEASE)]);
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        assert!(target.lock().unwrap().secret_attempt.is_none());
        // The next Return goes to the session.
        let decision = bindings.lock().unwrap().feed(key(KEY_ENTER, KEY_PRESS));
        assert!(decision.forward.is_some(), "{decision:?}");
        bindings.lock().unwrap().feed(key(KEY_ENTER, KEY_RELEASE));
        // The seat's own close settles by time; reopening forgets it, and
        // Escape's close discards through the first report again.
        {
            let bindings = bindings.lock().unwrap();
            let cutoff = bindings.cutoff.unwrap();
            assert_eq!(bindings.settled, Some(cutoff + SELF_CLOSE_SETTLE));
            assert_eq!(bindings.keyless_close, None);
        }
        feed(&[
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
        ]);
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        assert_eq!(bindings.lock().unwrap().settled, None);
        // Closed, a second release changes nothing; a seat gone refuses.
        release.release(&attempt).unwrap();
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        assert!(Seat::default().release(&attempt).is_err());
    }

    #[test]
    fn device_dispatch_delivers_each_secret_selection_once() {
        use crate::authority::consent::{Recovery, Role};
        use crate::secret_client::Selection;
        for (code, selected) in [
            (KEY_U, Selection::Unlock(Role::Primary)),
            (KEY_R, Selection::Unlock(Role::Recovery)),
            (KEY_E, Selection::Enroll(Recovery::SecondToken)),
            (KEY_X, Selection::Enroll(Recovery::Unrecoverable)),
            (KEY_W, Selection::Write),
            (KEY_I, Selection::Install),
        ] {
            let target = Mutex::new(RecordingTarget::default());
            let bindings = Mutex::new(KeyBindings {
                attention_enabled: true,
                ..KeyBindings::default()
            });
            let mut resync = || None;
            let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            for event in [key(code, KEY_PRESS), key(code, KEY_RELEASE)] {
                apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
            }
            assert!(target.lock().unwrap().secret_roles.is_empty());
            for event in [
                key(KEY_LEFTCTRL, KEY_PRESS),
                key(KEY_LEFTALT, KEY_PRESS),
                key(KEY_ESC, KEY_PRESS),
                key(KEY_ESC, KEY_RELEASE),
                key(KEY_LEFTCTRL, KEY_RELEASE),
                key(KEY_LEFTALT, KEY_RELEASE),
                key(code, KEY_PRESS),
                key(code, KEY_REPEAT),
                key(code, KEY_RELEASE),
                key(KEY_U, KEY_PRESS),
                key(KEY_U, KEY_RELEASE),
            ] {
                apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
            }
            let target = target.lock().unwrap();
            assert_eq!(target.attention_events, [true]);
            assert_eq!(target.secret_roles, [selected]);
        }
    }

    #[test]
    fn a_security_keys_keyboard_can_still_cancel_attention() {
        const KEY: usize = 1;
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: BTreeSet::from([KEY]),
            ..KeyBindings::default()
        });
        let mut resync = || None;
        // A lone keyboard carrying a FIDO interface opens attention, as
        // outside it is an ordinary keyboard, and must be able to leave.
        let mut token = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            apply_device_event(&target, event, KEY, &bindings, &mut token).unwrap();
        }
        assert!(bindings.lock().unwrap().attention == AttentionState::Open);
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply_device_event(&target, key(KEY_ESC, KEY_PRESS), KEY, &bindings, &mut token).unwrap();
        assert!(bindings.lock().unwrap().attention == AttentionState::Draining);
        assert_eq!(target.lock().unwrap().draining_events, 1);
        apply_device_event(
            &target,
            key(KEY_ESC, KEY_RELEASE),
            KEY,
            &bindings,
            &mut token,
        )
        .unwrap();
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true, false]);
        assert!(target.secret_roles.is_empty());
    }

    #[test]
    fn a_security_keys_keyboard_never_selects_or_confirms_and_still_drains() {
        use crate::authority::consent::Role;
        use crate::secret_client::Selection;
        const KEY: usize = 1;
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: BTreeSet::from([KEY]),
            ..KeyBindings::default()
        });
        let (mut keyboard_resync, mut key_resync) = (|| None, || None);
        let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut keyboard_resync, true);
        let mut token = DeviceState::new(None, AbsoluteKind::Tablet, &mut key_resync, true);
        // Outside attention it is an ordinary keyboard.
        for code in [KEY_U, KEY_ENTER] {
            for value in [KEY_PRESS, KEY_RELEASE] {
                apply_device_event(&target, key(code, value), KEY, &bindings, &mut token).unwrap();
            }
        }
        {
            let target = target.lock().unwrap();
            let typed: Vec<_> = target.keys.iter().map(|input| input.key).collect();
            let (u, enter) = (u32::from(KEY_U), u32::from(KEY_ENTER));
            assert_eq!(typed, [u, u, enter, enter]);
            assert!(target.secret_roles.is_empty());
        }
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            apply_device_event(&target, event, 0, &bindings, &mut keyboard).unwrap();
        }
        let typed = target.lock().unwrap().keys.len();
        // A touch types modhex and Enter: nothing is selected or confirmed.
        for code in [KEY_U, KEY_R, KEY_E, KEY_X, KEY_W, KEY_I, KEY_ENTER] {
            for value in [KEY_PRESS, KEY_RELEASE] {
                apply_device_event(&target, key(code, value), KEY, &bindings, &mut token).unwrap();
            }
        }
        {
            let target = target.lock().unwrap();
            assert!(target.secret_roles.is_empty());
            assert!(target.confirmations.is_empty());
        }
        // Its Shift is no modifier here; the keyboard's is.
        apply_device_event(
            &target,
            key(KEY_LEFTSHIFT, KEY_PRESS),
            KEY,
            &bindings,
            &mut token,
        )
        .unwrap();
        assert_eq!(
            bindings.lock().unwrap().modifiers().depressed & MOD_SHIFT,
            0
        );
        apply_device_event(
            &target,
            key(KEY_RIGHTSHIFT, KEY_PRESS),
            0,
            &bindings,
            &mut keyboard,
        )
        .unwrap();
        assert_eq!(
            bindings.lock().unwrap().modifiers().depressed & MOD_SHIFT,
            MOD_SHIFT
        );
        for (device, state) in [(KEY, &mut token), (0, &mut keyboard)] {
            let shift = if device == KEY {
                KEY_LEFTSHIFT
            } else {
                KEY_RIGHTSHIFT
            };
            apply_device_event(&target, key(shift, KEY_RELEASE), device, &bindings, state).unwrap();
        }
        // Its held U does not stand in the keyboard's way.
        apply_device_event(&target, key(KEY_U, KEY_PRESS), KEY, &bindings, &mut token).unwrap();
        for value in [KEY_PRESS, KEY_RELEASE] {
            apply_device_event(&target, key(KEY_U, value), 0, &bindings, &mut keyboard).unwrap();
        }
        apply_device_event(&target, key(KEY_U, KEY_RELEASE), KEY, &bindings, &mut token).unwrap();
        {
            let target = target.lock().unwrap();
            assert_eq!(target.secret_roles, [Selection::Unlock(Role::Primary)]);
            assert!(target.confirmations.is_empty());
        }
        // Its Enter cannot confirm the selection; the keyboard's does.
        for value in [KEY_PRESS, KEY_RELEASE] {
            apply_device_event(&target, key(KEY_ENTER, value), KEY, &bindings, &mut token).unwrap();
        }
        assert!(target.lock().unwrap().confirmations.is_empty());
        let enter = Event {
            timestamp: 7,
            ..key(KEY_ENTER, KEY_PRESS)
        };
        apply_device_event(&target, enter, 0, &bindings, &mut keyboard).unwrap();
        apply_device_event(
            &target,
            key(KEY_ENTER, KEY_RELEASE),
            0,
            &bindings,
            &mut keyboard,
        )
        .unwrap();
        assert_eq!(target.lock().unwrap().confirmations, [7]);
        // Its held key still holds the drain open.
        apply_device_event(&target, key(KEY_E, KEY_PRESS), KEY, &bindings, &mut token).unwrap();
        for value in [KEY_PRESS, KEY_RELEASE] {
            apply_device_event(&target, key(KEY_ESC, value), 0, &bindings, &mut keyboard).unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply_device_event(&target, key(KEY_E, KEY_RELEASE), KEY, &bindings, &mut token).unwrap();
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true, false]);
        assert_eq!(target.keys.len(), typed);
        assert_eq!(target.secret_roles, [Selection::Unlock(Role::Primary)]);
        assert_eq!(target.confirmations, [7]);
    }

    #[test]
    fn physical_attention_requires_a_fresh_explicit_enrollment_choice() {
        use crate::authority::consent::Recovery;
        use crate::secret_client::Selection;
        for (code, selection) in [
            (KEY_E, Selection::Enroll(Recovery::SecondToken)),
            (KEY_X, Selection::Enroll(Recovery::Unrecoverable)),
            (KEY_W, Selection::Write),
            (KEY_I, Selection::Install),
        ] {
            let mut bindings = KeyBindings {
                attention_enabled: true,
                ..KeyBindings::default()
            };
            let mut target = RecordingTarget::default();
            assert!(bindings.feed(key(code, KEY_PRESS)).secret.is_none());
            for event in [
                key(KEY_LEFTCTRL, KEY_PRESS),
                key(KEY_LEFTALT, KEY_PRESS),
                key(KEY_ESC, KEY_PRESS),
                key(KEY_ESC, KEY_RELEASE),
                key(KEY_LEFTCTRL, KEY_RELEASE),
                key(KEY_LEFTALT, KEY_RELEASE),
            ] {
                let decision = bindings.feed(event);
                deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
            }
            assert!(bindings.feed(key(code, KEY_REPEAT)).secret.is_none());
            assert!(bindings
                .feed_device(1, key(code, KEY_PRESS))
                .secret
                .is_none());
            bindings.feed_device(1, key(code, KEY_RELEASE));
            bindings.feed(key(code, KEY_RELEASE));
            for event in [
                key(code, KEY_PRESS),
                key(code, KEY_REPEAT),
                key(code, KEY_RELEASE),
                key(KEY_U, KEY_PRESS),
                key(KEY_U, KEY_RELEASE),
            ] {
                let decision = bindings.feed(event);
                deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
            }
            assert_eq!(target.secret_roles, [selection]);
        }
    }

    #[test]
    fn physical_attention_selects_only_one_token_role_per_open() {
        let mut bindings = KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        };
        let mut target = RecordingTarget::default();
        for event in [key(KEY_U, KEY_PRESS), key(KEY_U, KEY_RELEASE)] {
            assert!(bindings.feed(event).secret.is_none());
        }
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        for event in [
            key(KEY_R, KEY_PRESS),
            key(KEY_R, KEY_REPEAT),
            key(KEY_R, KEY_RELEASE),
            key(KEY_U, KEY_PRESS),
            key(KEY_U, KEY_RELEASE),
        ] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        assert_eq!(
            target.secret_roles,
            [crate::secret_client::Selection::Unlock(
                crate::authority::consent::Role::Recovery
            )]
        );
        assert!(target
            .keys
            .iter()
            .all(|key| key.key != u32::from(KEY_U) && key.key != u32::from(KEY_R)));
        for event in [key(KEY_ESC, KEY_PRESS), key(KEY_ESC, KEY_RELEASE)] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        assert!(bindings.attention == AttentionState::Closed);
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_U, KEY_PRESS),
        ] {
            let decision = bindings.feed(event);
            deliver_key_decision(&mut target, &mut bindings, decision).unwrap();
        }
        assert_eq!(
            target.secret_roles,
            [
                crate::secret_client::Selection::Unlock(crate::authority::consent::Role::Recovery),
                crate::secret_client::Selection::Unlock(crate::authority::consent::Role::Primary)
            ]
        );
    }

    #[test]
    fn secure_attention_reserves_the_chord_and_drains_all_devices() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut keyboard = PointerMotion::default();
        let mut mouse = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        let before = target.lock().unwrap().keyboard_calls.len();
        for event in [
            key(KEY_ESC, KEY_REPEAT),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_T, KEY_PRESS),
            key(KEY_T, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            1,
            &bindings,
            &mut mouse,
            None,
        )
        .unwrap();
        // Cancel before this other device has delivered SYN_REPORT.
        for event in [
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply(&target, syn(1), 1, &bindings, &mut mouse, None).unwrap();
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            1,
            &bindings,
            &mut mouse,
            None,
        )
        .unwrap();
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply(&target, syn(2), 1, &bindings, &mut mouse, None).unwrap();
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true, false]);
        assert!(target.launched.is_empty());
        assert!(target.pointer_frames.is_empty());
        assert_eq!(target.keyboard_calls.len(), before + 1); // restored lock modifiers
        assert!(target
            .keys
            .iter()
            .all(|input| input.key != u32::from(KEY_ESC)));
    }

    #[test]
    fn losing_an_input_device_does_not_cancel_attention() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        release_device(&target, 0, &bindings, 1).unwrap();
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        for event in [key(KEY_ESC, KEY_PRESS), key(KEY_ESC, KEY_RELEASE)] {
            apply(&target, event, 1, &bindings, &mut pointer, None).unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true, false]);
    }

    #[test]
    fn direct_profile_does_not_claim_a_trusted_chord() {
        let mut bindings = KeyBindings::default();
        for event in [key(KEY_LEFTCTRL, KEY_PRESS), key(KEY_LEFTALT, KEY_PRESS)] {
            assert!(bindings.feed(event).attention.is_none());
        }
        let decision = bindings.feed(key(KEY_ESC, KEY_PRESS));
        assert!(decision.attention.is_none());
        assert_eq!(decision.forward.unwrap().key, u32::from(KEY_ESC));
    }

    #[test]
    fn attention_withdraws_focus_suppresses_runtime_input_and_survives_paint_failure() {
        let path = std::env::temp_dir().join(format!(
            "td-attention-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 320, 200, 320 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let origin = EvdevOrigin { _private: () };
        assert!(runtime.attention(&origin, true).is_err());
        runtime.enable_attention(true);
        let surface = crate::scene::SurfaceKey {
            client: 1,
            object: 1,
        };
        runtime
            .commit(
                surface,
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [1, 2, 3, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .unwrap(),
            )
            .unwrap();
        runtime
            .key(key(KEY_LEFTCTRL, KEY_PRESS).key_input())
            .unwrap();
        let before = runtime.keyboard_snapshot();
        assert_eq!(before.focus, Some(surface));
        runtime.attention(&origin, true).unwrap();
        let trusted_pixels = std::fs::read(&cleanup.0).unwrap();
        let captured = runtime.keyboard_snapshot();
        assert_eq!(captured.focus, None);
        assert!(captured.keys.is_empty());
        assert!(captured.revision > before.revision);
        runtime.key(key(KEY_T, KEY_PRESS).key_input()).unwrap();
        runtime
            .modifiers(ModifierState {
                depressed: MOD_CONTROL,
                ..ModifierState::default()
            })
            .unwrap();
        runtime
            .pointer_frame(1, 70, 70, &[], PointerScroll::default())
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot(), captured);
        assert!(runtime.pointer_snapshot().focus.is_none());
        // An application commit cannot cover the display-only sheet or take focus.
        runtime
            .commit(
                surface,
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [6, 7, 8, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(std::fs::read(&cleanup.0).unwrap(), trusted_pixels);
        assert_eq!(runtime.keyboard_snapshot().focus, None);
        runtime.fail_next_repaint();
        assert!(runtime.attention(&origin, false).is_err());
        runtime.key(key(KEY_T, KEY_PRESS).key_input()).unwrap();
        assert!(runtime.keyboard_snapshot().keys.is_empty());
        assert_eq!(runtime.keyboard_snapshot().focus, None);
        runtime.clear_repaint_failure();
        runtime.attention(&origin, false).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, Some(surface));
        assert!(runtime.keyboard_snapshot().keys.is_empty());
        assert_ne!(std::fs::read(&cleanup.0).unwrap(), trusted_pixels);
    }
    #[test]
    fn cancelling_attention_drains_even_a_silent_pointer_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut keyboard = PointerMotion::default();
        let mut mouse = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        // Net-zero motion yields no PointerFrame, but its SYN still drains it.
        for value in [1, -1] {
            apply(
                &target,
                Event {
                    timestamp: 1_000_000,
                    time: 1,
                    kind: EV_REL,
                    code: REL_X,
                    value,
                },
                1,
                &bindings,
                &mut mouse,
                None,
            )
            .unwrap();
        }
        for event in [
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        apply(&target, syn(2), 1, &bindings, &mut mouse, None).unwrap();
        assert_eq!(target.lock().unwrap().attention_events, [true, false]);
        assert!(target.lock().unwrap().pointer_frames.is_empty());
    }
    fn at_millis(mut event: Event, time: u32) -> Event {
        event.time = time;
        event.timestamp = u128::from(time) * 1_000_000;
        event
    }

    #[test]
    fn a_returned_batch_cannot_deliver_trusted_input_after_another_reader_cancels() {
        struct PausedRead {
            bytes: std::io::Cursor<Vec<u8>>,
            read: Arc<std::sync::Barrier>,
            resume: Arc<std::sync::Barrier>,
            paused: bool,
        }
        impl Read for PausedRead {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = self.bytes.read(output)?;
                if !self.paused {
                    self.paused = true;
                    self.read.wait();
                    self.resume.wait();
                }
                Ok(count)
            }
        }
        let target = Arc::new(Mutex::new(RecordingTarget {
            attention_cutoff: 100_000_000,
            ..RecordingTarget::default()
        }));
        let bindings = Arc::new(Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        }));
        let mut keyboard = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
        ] {
            apply(
                target.as_ref(),
                event,
                0,
                bindings.as_ref(),
                &mut keyboard,
                None,
            )
            .unwrap();
        }
        target.lock().unwrap().keys.clear();
        let read = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        let queued = [
            at_millis(key(KEY_T, KEY_PRESS), 50),
            at_millis(key(KEY_T, KEY_RELEASE), 51),
            syn(51),
            at_millis(key(BTN_MOUSE, KEY_PRESS), 52),
            syn(52),
            at_millis(key(BTN_MOUSE, KEY_RELEASE), 53),
            syn(53),
            at_millis(key(KEY_A, KEY_PRESS), 200),
            syn(200),
            at_millis(key(KEY_A, KEY_RELEASE), 201),
            syn(201),
        ]
        .into_iter()
        .flat_map(encode)
        .collect();
        let mut file = PausedRead {
            bytes: std::io::Cursor::new(queued),
            read: Arc::clone(&read),
            resume: Arc::clone(&resume),
            paused: false,
        };
        let reader_target = Arc::clone(&target);
        let reader_bindings = Arc::clone(&bindings);
        let reader = thread::spawn(move || {
            read_device(
                Path::new("delayed-reader"),
                &mut file,
                1,
                reader_target.as_ref(),
                reader_bindings.as_ref(),
                None,
                &mut || None,
            )
        });
        read.wait();
        for event in [
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            apply(
                target.as_ref(),
                event,
                0,
                bindings.as_ref(),
                &mut keyboard,
                None,
            )
            .unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true, false]);
        resume.wait();
        reader.join().unwrap().unwrap();
        let target = target.lock().unwrap();
        assert_eq!(
            target.keys,
            [
                at_millis(key(KEY_A, KEY_PRESS), 200).key_input(),
                at_millis(key(KEY_A, KEY_RELEASE), 201).key_input()
            ]
        );
        assert!(target.pointer_frames.is_empty());
        assert_eq!(target.draining_events, 1);
    }

    #[test]
    fn cutoff_quarantines_straddling_reports_and_does_not_wrap_with_wayland_time() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        let partial = Event {
            kind: 4,
            code: 0,
            value: 0,
            ..syn(1)
        };
        apply_device_event(&target, partial, 0, &bindings, &mut state).unwrap();
        let cutoff = (u128::from(u32::MAX) + 1) * 1_000_000;
        bindings.lock().unwrap().cutoff = Some(cutoff);
        for mut event in [at_millis(key(KEY_T, KEY_PRESS), 1), syn(1)] {
            event.timestamp += cutoff;
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert!(
            target.lock().unwrap().keys.is_empty(),
            "partial report escaped"
        );
        for event in [at_millis(key(KEY_T, KEY_PRESS), 2), syn(2)] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert!(
            target.lock().unwrap().keys.is_empty(),
            "wrapped stale event escaped"
        );
        let mut fresh = at_millis(key(KEY_A, KEY_PRESS), 3);
        fresh.timestamp += cutoff;
        let decoded = parse(&encode(fresh)).unwrap();
        assert_eq!(decoded.time, 3);
        assert_eq!(decoded.timestamp, fresh.timestamp);
        apply_device_event(&target, decoded, 0, &bindings, &mut state).unwrap();
        assert_eq!(target.lock().unwrap().keys, [fresh.key_input()]);
    }

    /// After the seat's own close, an idle device's first report is taken
    /// once it is stamped past the settle window; one inside it, or at or
    /// before the cutoff, is not. A key's close keeps the first-report
    /// discard.
    #[test]
    fn a_self_close_settles_by_time_and_keeps_the_next_key() {
        let cutoff = 5_000_000_000u128;
        let stamped = |mut event: Event, at: u128| {
            event.timestamp = at;
            event
        };
        let first = stamped(key(KEY_A, KEY_PRESS), cutoff + 2 * SELF_CLOSE_SETTLE);
        let second = stamped(key(KEY_B, KEY_PRESS), cutoff + 3 * SELF_CLOSE_SETTLE);
        for (settled, early, taken) in [
            // The seat's own close: reports at the cutoff or inside the
            // window are not taken, the first one after it is.
            (
                Some(cutoff + SELF_CLOSE_SETTLE),
                &[cutoff, cutoff + SELF_CLOSE_SETTLE][..],
                vec![first.key_input(), second.key_input()],
            ),
            // A key's close: the first report after the cutoff is discarded
            // however late, as before.
            (None, &[][..], vec![second.key_input()]),
        ] {
            let target = Mutex::new(RecordingTarget::default());
            let bindings = Mutex::new(KeyBindings {
                attention_enabled: true,
                cutoff: Some(cutoff),
                settled,
                ..KeyBindings::default()
            });
            let mut resync = || None;
            let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            for at in early {
                for event in [stamped(key(KEY_T, KEY_PRESS), *at), stamped(syn(1), *at)] {
                    apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
                }
            }
            for event in [first, second] {
                for event in [event, stamped(syn(1), event.timestamp)] {
                    apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
                }
            }
            assert_eq!(target.lock().unwrap().keys, taken, "settled {settled:?}");
        }
    }

    /// A key pressed inside a self-close's settle window and released past
    /// it: the press is dropped, the release is taken by the seat but not
    /// forwarded, since its press never was, nothing stays held, and the
    /// next key reaches the session. The window's drops are counted and
    /// said once, by count alone, at the first report past it.
    #[test]
    fn a_key_straddling_the_settle_window_is_dropped_whole() {
        let cutoff = 5_000_000_000u128;
        let stamped = |mut event: Event, at: u128| {
            event.timestamp = at;
            event
        };
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            cutoff: Some(cutoff),
            settled: Some(cutoff + SELF_CLOSE_SETTLE),
            settle_dropped: Some(0),
            ..KeyBindings::default()
        });
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        let mut report = |event: Event, at: u128| {
            for event in [stamped(event, at), stamped(syn(1), at)] {
                apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
            }
        };
        report(key(KEY_ENTER, KEY_PRESS), cutoff + SELF_CLOSE_SETTLE / 2);
        assert_eq!(bindings.lock().unwrap().settle_dropped, Some(1));
        report(
            key(KEY_ENTER, KEY_RELEASE),
            cutoff + SELF_CLOSE_SETTLE * 3 / 2,
        );
        {
            let bindings = bindings.lock().unwrap();
            assert_eq!(bindings.settle_dropped, None);
            assert!(bindings.pressed.is_empty());
            assert!(bindings.forwarded.is_empty());
        }
        assert!(target.lock().unwrap().keys.is_empty());
        let next = stamped(key(KEY_B, KEY_PRESS), cutoff + 3 * SELF_CLOSE_SETTLE);
        report(next, next.timestamp);
        report(key(KEY_B, KEY_RELEASE), cutoff + 4 * SELF_CLOSE_SETTLE);
        let keys = target.lock().unwrap().keys.clone();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys.first(), Some(&next.key_input()));
        assert!(bindings.lock().unwrap().pressed.is_empty());
    }

    /// The settle evidence counts only reports after the cutoff that the
    /// window dropped, says them once with no key, and is silent after a
    /// key's close.
    #[test]
    fn the_settle_evidence_counts_reports_and_names_no_key() {
        let cutoff = 7_000u128;
        let at = |mut event: Event, timestamp: u128| {
            event.timestamp = timestamp;
            event
        };
        let mut bindings = KeyBindings {
            cutoff: Some(cutoff),
            settled: Some(cutoff + SELF_CLOSE_SETTLE),
            settle_dropped: Some(0),
            ..KeyBindings::default()
        };
        // Before the cutoff, or not a report boundary: not counted.
        assert_eq!(bindings.settle_evidence(at(syn(1), cutoff), false), None);
        assert_eq!(
            bindings.settle_evidence(at(key(KEY_ENTER, KEY_PRESS), cutoff + 1), false),
            None
        );
        assert_eq!(bindings.settle_dropped, Some(0));
        for offset in [1, 2] {
            assert_eq!(
                bindings.settle_evidence(at(syn(1), cutoff + offset), false),
                None
            );
        }
        let past = cutoff + SELF_CLOSE_SETTLE + 1;
        assert_eq!(
            bindings.settle_evidence(at(syn(1), past), true),
            Some(format!("{SETTLED_MARKER} cutoff={cutoff} dropped=2"))
        );
        assert_eq!(bindings.settle_evidence(at(syn(1), past + 1), true), None);
        bindings.settle_dropped = None;
        assert_eq!(
            bindings.settle_evidence(at(syn(1), cutoff + 1), false),
            None
        );
    }

    #[test]
    fn a_fresh_escape_on_another_keyboard_can_enter_and_cancel_attention() {
        let mut bindings = KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        };
        bindings.feed_device(0, key(KEY_ESC, KEY_PRESS));
        bindings.feed_device(1, key(KEY_LEFTCTRL, KEY_PRESS));
        bindings.feed_device(1, key(KEY_LEFTALT, KEY_PRESS));
        assert_eq!(
            bindings.feed_device(1, key(KEY_ESC, KEY_PRESS)).attention,
            Some(true)
        );
        bindings.feed_device(1, key(KEY_ESC, KEY_RELEASE));
        assert!(bindings.feed_device(1, key(KEY_ESC, KEY_PRESS)).draining);
        assert!(bindings.attention == AttentionState::Draining);
    }

    #[test]
    fn device_cleanup_completes_cancellation_after_keyboard_and_pointer_drain() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(BTN_MOUSE, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        release_device(&target, 0, &bindings, 1).unwrap();
        assert_eq!(target.lock().unwrap().attention_events, [true, false]);
        assert!(bindings.lock().unwrap().pointer_pending.is_empty());
    }
    #[test]
    fn a_report_timestamped_after_close_must_cross_the_first_report_fence() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            cutoff: Some(100_000_000),
            ..KeyBindings::default()
        });
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        // Linux can collect a value before close and timestamp the whole
        // report at a later input_sync. Userspace has seen no partial record.
        for event in [
            at_millis(key(KEY_T, KEY_PRESS), 101),
            syn(101),
            at_millis(key(KEY_T, KEY_RELEASE), 102),
            syn(102),
        ] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert!(target.lock().unwrap().keys.is_empty());
        for event in [
            at_millis(key(KEY_A, KEY_PRESS), 103),
            syn(103),
            at_millis(key(KEY_A, KEY_RELEASE), 104),
            syn(104),
        ] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().keys,
            [
                at_millis(key(KEY_A, KEY_PRESS), 103).key_input(),
                at_millis(key(KEY_A, KEY_RELEASE), 104).key_input()
            ]
        );
    }

    #[test]
    fn cutoff_reports_resync_absolute_state_without_replaying_discarded_buttons() {
        for dropped in [false, true] {
            for reopened in [false, true] {
                let target = Mutex::new(RecordingTarget::default());
                let bindings = Mutex::new(KeyBindings {
                    attention_enabled: true,
                    cutoff: Some(100_000_000),
                    attention: if reopened {
                        AttentionState::Open
                    } else {
                        AttentionState::Closed
                    },
                    ..KeyBindings::default()
                });
                let calls = std::cell::Cell::new(0);
                let moved = AbsoluteAxes {
                    x: axis(20000, 0, 32767),
                    y: axis(30000, 0, 40000),
                };
                let mut resync = || {
                    calls.set(calls.get() + 1);
                    Some(moved)
                };
                let mut state =
                    DeviceState::new(Some(tablet()), AbsoluteKind::Tablet, &mut resync, true);
                state.pointer.hold(100, 100);
                if dropped {
                    apply_device_event(
                        &target,
                        Event {
                            code: SYN_DROPPED,
                            ..syn(101)
                        },
                        0,
                        &bindings,
                        &mut state,
                    )
                    .unwrap();
                }
                for event in [
                    abs(101, ABS_X, 20000),
                    abs(101, ABS_Y, 30000),
                    at_millis(key(BTN_MOUSE, KEY_PRESS), 101),
                ] {
                    apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
                }
                assert_eq!(
                    calls.get(),
                    0,
                    "a partial rejected report queried the device"
                );
                apply_device_event(&target, syn(101), 0, &bindings, &mut state).unwrap();
                assert_eq!(
                    calls.get(),
                    1,
                    "cutoff lost absolute state: dropped={dropped}, reopened={reopened}"
                );
                assert!(!state.dropped);
                assert!(state.pointer.pressed.is_empty());
                assert!(bindings.lock().unwrap().pointer_pressed.is_empty());
                assert!(target.lock().unwrap().keys.is_empty());
                let places = target.lock().unwrap().pointer_places.clone();
                if reopened {
                    assert!(
                        places.is_empty(),
                        "recovery published through active attention"
                    );
                } else {
                    assert_eq!(
                        places.as_slice(),
                        &[(101, over(20000, 32767), over(30000, 40000), Vec::new())]
                    );
                }
                // A later button-only report must use the current position,
                // even when the kernel never reports another changed axis.
                bindings.lock().unwrap().attention = AttentionState::Closed;
                for event in [at_millis(key(BTN_MOUSE, KEY_PRESS), 102), syn(102)] {
                    apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
                }
                let target = target.lock().unwrap();
                let (_, x, y, buttons) = target.pointer_places.last().unwrap();
                assert_eq!((*x, *y), (over(20000, 32767), over(30000, 40000)));
                assert_eq!(
                    buttons.as_slice(),
                    &[PointerButtonInput {
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                        time: 102
                    }]
                );
                assert_eq!(calls.get(), 1);
            }
        }
    }

    #[test]
    fn modifier_failure_restores_attention_and_reports_every_recovery_failure() {
        for screen_failure in [false, true] {
            for banner_failure in [false, true] {
                let mut target = RecordingTarget::default();
                target.attention(true).unwrap();
                target.modifiers_error = Some("modifier publication failed".to_string());
                target.attention_error =
                    screen_failure.then(|| "screen recovery failed".to_string());
                target.draining_error =
                    banner_failure.then(|| "banner recovery failed".to_string());
                let mut bindings = KeyBindings {
                    attention: AttentionState::Draining,
                    ..KeyBindings::default()
                };
                let error = finish_attention(&mut target, &mut bindings).unwrap_err();
                assert_eq!(target.attention_events, [true, false, true]);
                assert_eq!(target.draining_events, 1);
                assert!(bindings.attention == AttentionState::Draining);
                assert_eq!(bindings.cutoff, None);
                assert!(error.contains("modifier publication failed"));
                assert_eq!(error.contains("screen recovery failed"), screen_failure);
                assert_eq!(error.contains("banner recovery failed"), banner_failure);
            }
        }
    }

    #[test]
    fn unplugging_a_pending_pointer_report_alone_completes_attention_drain() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        });
        let mut keyboard = PointerMotion::default();
        let mut mouse = PointerMotion::default();
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        apply(
            &target,
            Event {
                kind: EV_REL,
                code: REL_X,
                value: 5,
                ..syn(1)
            },
            7,
            &bindings,
            &mut mouse,
            None,
        )
        .unwrap();
        for event in [key(KEY_ESC, KEY_PRESS), key(KEY_ESC, KEY_RELEASE)] {
            apply(&target, event, 0, &bindings, &mut keyboard, None).unwrap();
        }
        assert!(bindings.lock().unwrap().pressed.is_empty());
        assert!(bindings.lock().unwrap().pointer_pressed.is_empty());
        assert_eq!(
            bindings.lock().unwrap().pointer_pending,
            BTreeSet::from([7])
        );
        assert_eq!(target.lock().unwrap().attention_events, [true]);
        release_device(&target, 7, &bindings, 1).unwrap();
        assert!(bindings.lock().unwrap().pointer_pending.is_empty());
        assert_eq!(target.lock().unwrap().attention_events, [true, false]);
    }

    /// Each code pressed and released as its own report, from `from`
    /// milliseconds, so no key is held across another.
    fn presses(codes: &[u16], from: u32) -> Vec<Event> {
        codes
            .iter()
            .zip((from..).step_by(2))
            .flat_map(|(code, time)| {
                [
                    at_millis(key(*code, KEY_PRESS), time),
                    syn(time),
                    at_millis(key(*code, KEY_RELEASE), time + 1),
                    syn(time + 1),
                ]
            })
            .collect()
    }

    /// Ctrl+Alt+Esc, each change its own report, opening attention.
    fn chord_reports(from: u32) -> Vec<Event> {
        [
            (KEY_LEFTCTRL, KEY_PRESS),
            (KEY_LEFTALT, KEY_PRESS),
            (KEY_ESC, KEY_PRESS),
            (KEY_ESC, KEY_RELEASE),
            (KEY_LEFTCTRL, KEY_RELEASE),
            (KEY_LEFTALT, KEY_RELEASE),
        ]
        .into_iter()
        .zip(from..)
        .flat_map(|((code, value), time)| [at_millis(key(code, value), time), syn(time)])
        .collect()
    }

    /// `Super+l`, each change its own report.
    fn super_l_reports(from: u32) -> Vec<Event> {
        [
            (KEY_LEFTMETA, KEY_PRESS),
            (KEY_L, KEY_PRESS),
            (KEY_L, KEY_RELEASE),
            (KEY_LEFTMETA, KEY_RELEASE),
        ]
        .into_iter()
        .zip(from..)
        .flat_map(|((code, value), time)| [at_millis(key(code, value), time), syn(time)])
        .collect()
    }

    /// The whole device dispatcher: these reports, read from `device`.
    fn read_reports(
        target: &Mutex<RecordingTarget>,
        bindings: &Mutex<KeyBindings>,
        device: usize,
        events: &[Event],
    ) {
        let data = events.iter().copied().flat_map(encode).collect();
        read_device(
            Path::new("event-test"),
            &mut ChunkedReader::new(data, Vec::new()),
            device,
            target,
            bindings,
            None,
            &mut || None,
        )
        .unwrap();
    }

    fn attention_bindings(enrolled: Option<Vec<[u8; 4]>>) -> Mutex<KeyBindings> {
        Mutex::new(KeyBindings {
            attention_enabled: true,
            login: enrolled
                .map_or_else(crate::authority::Login::default, |keys| login_answer(&keys)),
            ..KeyBindings::default()
        })
    }

    /// The login state of root's `9a` for `keys`, as the worker stores it.
    fn login_answer(keys: &[[u8; 4]]) -> crate::authority::Login {
        let mut answer = vec![0x9a, 1, keys.len() as u8];
        answer.extend(keys.iter().flatten());
        answer.extend_from_slice(b"\x06tester\x09td-laptop\x00");
        let login = crate::authority::Login::default();
        login.answer(&answer).unwrap();
        login
    }

    const ENROLLED: [[u8; 4]; 3] = [[0xa1; 4], [0xa2; 4], [0xa3; 4]];

    #[test]
    fn key_management_selects_one_login_operation_per_lifetime() {
        use crate::attention::Notice;
        use crate::secret_client::{LoginSelection, Selection};
        for (code, selected) in [
            (KEY_1, LoginSelection::Enroll(1)),
            (KEY_2, LoginSelection::Enroll(2)),
            (KEY_A, LoginSelection::Add),
        ] {
            let target = Mutex::new(RecordingTarget::default());
            let bindings = attention_bindings(None);
            // Outside attention these are ordinary keys.
            read_reports(&target, &bindings, 0, &presses(&[KEY_K, code], 1));
            {
                let target = target.lock().unwrap();
                assert!(target.secret_roles.is_empty() && target.notices.is_empty());
                assert_eq!(target.keys.len(), 4);
            }
            read_reports(&target, &bindings, 0, &chord_reports(10));
            let typed = target.lock().unwrap().keys.len();
            // The menu does not take the key-management screen's keys.
            read_reports(&target, &bindings, 0, &presses(&[code], 20));
            assert!(target.lock().unwrap().secret_roles.is_empty());
            read_reports(&target, &bindings, 0, &presses(&[KEY_K, code], 30));
            // One operation: nothing else selects in this lifetime.
            read_reports(
                &target,
                &bindings,
                0,
                &presses(&[KEY_1, KEY_2, KEY_A, KEY_D, KEY_K, KEY_U, KEY_I], 40),
            );
            let target = target.lock().unwrap();
            assert_eq!(target.attention_events, [true]);
            assert_eq!(target.notices, [Notice::LoginKeys]);
            assert_eq!(target.secret_roles, [Selection::Login(selected)]);
            assert_eq!(target.keys.len(), typed);
        }
    }

    #[test]
    fn a_new_lifetime_starts_at_the_menu_again() {
        use crate::attention::Notice;
        use crate::secret_client::{LoginSelection, Selection};
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(None);
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_K, KEY_A], 20));
        read_reports(&target, &bindings, 0, &presses(&[KEY_ESC], 30));
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        // A fresh report after the close's cutoff, then a new lifetime whose
        // `1` is not the key-management screen's until `K` opens it again.
        let mut reopen = vec![syn(40)];
        reopen.extend(chord_reports(41));
        reopen.extend(presses(&[KEY_1, KEY_K, KEY_1], 50));
        read_reports(&target, &bindings, 0, &reopen);
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true, false, true]);
        assert_eq!(target.notices, [Notice::LoginKeys, Notice::LoginKeys]);
        assert_eq!(
            target.secret_roles,
            [
                Selection::Login(LoginSelection::Add),
                Selection::Login(LoginSelection::Enroll(1))
            ]
        );
    }

    #[test]
    fn removal_without_a_key_list_is_refused_locally_and_ends_the_choice() {
        use crate::attention::Notice;
        let unavailable = |cause: &'static str| -> &'static [&'static str] {
            match cause {
                "directory" => &["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"],
                "record" => &["LOGIN KEY STATE UNAVAILABLE:", "RECORD DAMAGED"],
                _ => &["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"],
            }
        };
        for (answer, shown) in [
            // Unenrolled, and each cause of the unavailable state, as root
            // answers them.
            (
                Some(&b"\x9a\x00\x06tester\x09td-laptop\x00"[..]),
                Notice::Login(&["NO LOGIN KEYS ENROLLED"]),
            ),
            (
                Some(&b"\x9a\x02\x0a\x06tester\x00\x00"[..]),
                Notice::Login(unavailable("directory")),
            ),
            (
                Some(&b"\x9a\x02\x0b\x06tester\x00\x00"[..]),
                Notice::Login(unavailable("record")),
            ),
            (
                Some(&b"\x9a\x02\x0c\x06tester\x00\x00"[..]),
                Notice::Login(unavailable("unreadable")),
            ),
            // No answer: the direct profile, which has no authority.
            (None, Notice::NotAvailable),
        ] {
            let target = Mutex::new(RecordingTarget::default());
            let bindings = attention_bindings(None);
            if let Some(answer) = answer {
                bindings.lock().unwrap().login.answer(answer).unwrap();
            }
            read_reports(&target, &bindings, 0, &chord_reports(10));
            read_reports(
                &target,
                &bindings,
                0,
                &presses(&[KEY_K, KEY_D, KEY_1, KEY_ENTER, KEY_A, KEY_2, KEY_U], 20),
            );
            let target = target.lock().unwrap();
            assert_eq!(target.notices, [Notice::LoginKeys, shown]);
            assert!(target.secret_roles.is_empty());
        }
    }

    /// The list `D` numbers is whatever root last answered: an answer that
    /// arrives once the bindings exist, as the worker's do, is the one used.
    #[test]
    fn removal_numbers_the_list_of_roots_latest_answer() {
        use crate::attention::Notice;
        use crate::authority::consent::Slot;
        use crate::secret_client::{LoginSelection, Selection};
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(None);
        let login = bindings.lock().unwrap().login.clone();
        login.answer(b"\x9a\x00\x06tester\x00\x00").unwrap();
        // A later answer, taken by the worker's handle, is the one `D` reads.
        let mut answer = vec![0x9a, 1, 2];
        answer.extend_from_slice(&[0xb1; 4]);
        answer.extend_from_slice(&[0xb2; 4]);
        answer.extend_from_slice(b"\x06tester\x00\x00");
        login.answer(&answer).unwrap();
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_K, KEY_D, KEY_2], 20));
        read_reports(&target, &bindings, 0, &presses(&[KEY_ENTER], 40));
        let target = target.lock().unwrap();
        assert_eq!(
            target.notices,
            [
                Notice::LoginKeys,
                Notice::Removing { keys: 2, chosen: 0 },
                Notice::Removing {
                    keys: 2,
                    chosen: 0b10
                },
            ]
        );
        assert_eq!(
            target.secret_roles,
            [Selection::Login(LoginSelection::Remove(vec![Slot {
                position: 2,
                key: [0xb2; 4],
            }]))]
        );
    }

    #[test]
    fn removal_digits_choose_a_set_that_enter_selects() {
        use crate::attention::Notice;
        use crate::authority::consent::Slot;
        use crate::secret_client::{LoginSelection, Selection};
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(Some(ENROLLED.to_vec()));
        read_reports(&target, &bindings, 0, &chord_reports(10));
        // Enter on an empty set selects nothing; 4, 9 and 0 name no key.
        read_reports(
            &target,
            &bindings,
            0,
            &presses(
                &[
                    KEY_K, KEY_D, KEY_ENTER, KEY_1, KEY_3, KEY_4, KEY_9, KEY_0, KEY_1, KEY_1,
                ],
                20,
            ),
        );
        {
            let target = target.lock().unwrap();
            let removing = |chosen| Notice::Removing { keys: 3, chosen };
            assert_eq!(
                target.notices,
                [
                    Notice::LoginKeys,
                    removing(0),
                    removing(0b001),
                    removing(0b101),
                    removing(0b100),
                    removing(0b101),
                ]
            );
            assert!(target.secret_roles.is_empty());
        }
        read_reports(&target, &bindings, 0, &presses(&[KEY_ENTER], 90));
        // Selected: nothing more selects, and Enter now only confirms.
        read_reports(&target, &bindings, 0, &presses(&[KEY_2, KEY_ENTER], 100));
        let target = target.lock().unwrap();
        assert_eq!(
            target.secret_roles,
            [Selection::Login(LoginSelection::Remove(vec![
                Slot {
                    position: 1,
                    key: ENROLLED[0],
                },
                Slot {
                    position: 3,
                    key: ENROLLED[2],
                },
            ]))]
        );
        assert_eq!(target.notices.len(), 6);
        assert_eq!(target.confirmations, [102_000_000]);
    }

    // Elevation consent: `B` and the approval key.

    /// Bindings on the open session, where `B` asks root for a rollback.
    fn rollback_bindings() -> Mutex<KeyBindings> {
        Mutex::new(KeyBindings {
            attention_enabled: true,
            ..KeyBindings::default()
        })
    }

    /// One event from `device`, decided and delivered.
    fn deliver(
        bindings: &mut KeyBindings,
        target: &mut RecordingTarget,
        device: usize,
        event: Event,
    ) {
        let decision = bindings.feed_device(device, event);
        deliver_key_decision(target, bindings, decision).unwrap();
    }

    /// `B` asks root for a rollback, and that is the lifetime's one
    /// selection: no later letter, `H` among them, selects anything.
    /// Outside attention both are ordinary keys.
    #[test]
    fn b_selects_a_rollback_and_ends_the_lifetimes_choice() {
        use crate::secret_client::{Elevation, Selection};
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(None);
        read_reports(&target, &bindings, 0, &presses(&[KEY_B, KEY_H], 1));
        {
            let target = target.lock().unwrap();
            assert!(target.secret_roles.is_empty() && target.notices.is_empty());
            assert_eq!(target.keys.len(), 4);
        }
        read_reports(&target, &bindings, 0, &chord_reports(10));
        let typed = target.lock().unwrap().keys.len();
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_B, KEY_B, KEY_U, KEY_I, KEY_K, KEY_L, KEY_H], 30),
        );
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true]);
        assert!(target.notices.is_empty());
        assert_eq!(
            target.secret_roles,
            [Selection::Elevation(Elevation::Rollback)]
        );
        assert_eq!(target.lock_screens, 0);
        assert_eq!(target.keys.len(), typed);
    }

    /// `B`'s rollback is never selected by a repeat, a key held from
    /// before the screen opened, or another device's press of a held key.
    #[test]
    fn b_selects_one_rollback_only_from_a_fresh_press() {
        use crate::secret_client::{Elevation, Selection};
        let mut bindings = rollback_bindings().into_inner().unwrap();
        let mut target = RecordingTarget::default();
        // Held from before the chord, on another keyboard.
        deliver(&mut bindings, &mut target, 1, key(KEY_B, KEY_PRESS));
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, event);
        }
        for (device, event) in [
            (1, key(KEY_B, KEY_REPEAT)),
            (0, key(KEY_B, KEY_PRESS)),
            (0, key(KEY_B, KEY_RELEASE)),
            (1, key(KEY_B, KEY_RELEASE)),
        ] {
            deliver(&mut bindings, &mut target, device, event);
        }
        assert!(target.secret_roles.is_empty() && target.notices.is_empty());
        for event in [
            key(KEY_B, KEY_PRESS),
            key(KEY_B, KEY_REPEAT),
            key(KEY_B, KEY_RELEASE),
            key(KEY_B, KEY_PRESS),
            key(KEY_B, KEY_RELEASE),
            key(KEY_U, KEY_PRESS),
            key(KEY_U, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, event);
        }
        assert_eq!(target.attention_events, [true]);
        assert_eq!(
            target.secret_roles,
            [Selection::Elevation(Elevation::Rollback)]
        );
        assert!(target.notices.is_empty());
    }

    /// `H` selects the queued hostname change, root's `1e`, as the
    /// lifetime's one selection, under the menu's rules: never from a
    /// repeat, a key held from before the screen opened or another
    /// device's press of a held key, and no letter selects after it. Root,
    /// not the menu, says when none waits.
    #[test]
    fn h_selects_the_queued_hostname_change_only_from_a_fresh_press() {
        use crate::secret_client::{Elevation, Selection};
        let mut bindings = rollback_bindings().into_inner().unwrap();
        let mut target = RecordingTarget::default();
        deliver(&mut bindings, &mut target, 1, key(KEY_H, KEY_PRESS));
        for event in [
            key(KEY_LEFTCTRL, KEY_PRESS),
            key(KEY_LEFTALT, KEY_PRESS),
            key(KEY_ESC, KEY_PRESS),
            key(KEY_ESC, KEY_RELEASE),
            key(KEY_LEFTCTRL, KEY_RELEASE),
            key(KEY_LEFTALT, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, event);
        }
        for (device, event) in [
            (1, key(KEY_H, KEY_REPEAT)),
            (0, key(KEY_H, KEY_PRESS)),
            (0, key(KEY_H, KEY_RELEASE)),
            (1, key(KEY_H, KEY_RELEASE)),
        ] {
            deliver(&mut bindings, &mut target, device, event);
        }
        assert!(target.secret_roles.is_empty() && target.notices.is_empty());
        for event in [
            key(KEY_H, KEY_PRESS),
            key(KEY_H, KEY_REPEAT),
            key(KEY_H, KEY_RELEASE),
            key(KEY_B, KEY_PRESS),
            key(KEY_B, KEY_RELEASE),
            key(KEY_H, KEY_PRESS),
            key(KEY_H, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, event);
        }
        assert_eq!(target.attention_events, [true]);
        assert_eq!(
            target.secret_roles,
            [Selection::Elevation(Elevation::Hostname)]
        );
        assert!(target.notices.is_empty());
    }

    /// After the lifetime's selection, only a fresh number-row 2 to 9 from
    /// a device secure attention reads, with no Control, Alt or Super held
    /// on such a device, is offered as an approval-key digit, with its
    /// evdev time; Shift is allowed. 0, 1, the keypad, Enter, repeats, a
    /// digit another read device holds and a security key's own keyboard
    /// offer none, nor does a digit before the selection.
    #[test]
    fn only_fresh_number_row_digits_from_a_read_keyboard_are_offered() {
        use crate::secret_client::{Elevation, Selection};
        const KEY: usize = 2;
        let mut bindings = KeyBindings {
            attention_excluded: BTreeSet::from([KEY]),
            ..rollback_bindings().into_inner().unwrap()
        };
        let mut target = RecordingTarget::default();
        fn stroke(
            bindings: &mut KeyBindings,
            target: &mut RecordingTarget,
            device: usize,
            code: u16,
            millis: u32,
        ) {
            for (value, millis) in [(KEY_PRESS, millis), (KEY_RELEASE, millis + 1)] {
                deliver(
                    bindings,
                    target,
                    device,
                    at_millis(key(code, value), millis),
                );
            }
        }
        // Outside attention, and on the menu before the selection.
        stroke(&mut bindings, &mut target, 0, KEY_4, 1);
        for (code, value) in [
            (KEY_LEFTCTRL, KEY_PRESS),
            (KEY_LEFTALT, KEY_PRESS),
            (KEY_ESC, KEY_PRESS),
            (KEY_ESC, KEY_RELEASE),
            (KEY_LEFTCTRL, KEY_RELEASE),
            (KEY_LEFTALT, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, key(code, value));
        }
        stroke(&mut bindings, &mut target, 0, KEY_4, 10);
        stroke(&mut bindings, &mut target, 0, KEY_B, 12);
        // None of these is a digit of the key's alphabet.
        for (code, millis) in [
            (KEY_KP4, 20),
            (KEY_KP7, 22),
            (KEY_KP2, 24),
            (KEY_0, 26),
            (KEY_1, 28),
            (KEY_ENTER, 30),
            (KEY_KPENTER, 32),
        ] {
            stroke(&mut bindings, &mut target, 0, code, millis);
        }
        // Under Control, Alt or Super, from either side.
        for (modifier, millis) in [
            (KEY_LEFTCTRL, 40),
            (KEY_RIGHTCTRL, 44),
            (KEY_LEFTALT, 48),
            (KEY_RIGHTALT, 52),
            (KEY_LEFTMETA, 56),
            (KEY_RIGHTMETA, 60),
        ] {
            deliver(&mut bindings, &mut target, 0, key(modifier, KEY_PRESS));
            stroke(&mut bindings, &mut target, 0, KEY_4, millis);
            deliver(&mut bindings, &mut target, 0, key(modifier, KEY_RELEASE));
        }
        // Under Shift it counts.
        deliver(&mut bindings, &mut target, 0, key(KEY_LEFTSHIFT, KEY_PRESS));
        stroke(&mut bindings, &mut target, 0, KEY_4, 70);
        deliver(
            &mut bindings,
            &mut target,
            0,
            key(KEY_LEFTSHIFT, KEY_RELEASE),
        );
        // A repeat supplies nothing.
        for (value, millis) in [(KEY_PRESS, 80), (KEY_REPEAT, 81), (KEY_RELEASE, 82)] {
            deliver(
                &mut bindings,
                &mut target,
                0,
                at_millis(key(KEY_7, value), millis),
            );
        }
        // Held on one read keyboard, the other's press is no fresh press.
        for (device, value, millis) in [
            (1, KEY_PRESS, 90),
            (0, KEY_PRESS, 91),
            (0, KEY_RELEASE, 92),
            (1, KEY_RELEASE, 93),
        ] {
            deliver(
                &mut bindings,
                &mut target,
                device,
                at_millis(key(KEY_5, value), millis),
            );
        }
        // A security key's own keyboard types none, and its Control held
        // is no modifier here.
        for (code, millis) in (KEY_2..=KEY_9).zip((100..).step_by(2)) {
            stroke(&mut bindings, &mut target, KEY, code, millis);
        }
        deliver(
            &mut bindings,
            &mut target,
            KEY,
            key(KEY_LEFTCTRL, KEY_PRESS),
        );
        stroke(&mut bindings, &mut target, 0, KEY_6, 120);
        deliver(
            &mut bindings,
            &mut target,
            KEY,
            key(KEY_LEFTCTRL, KEY_RELEASE),
        );
        // Each number-row digit from 2 to 9.
        for (code, millis) in (KEY_2..=KEY_9).zip((130..).step_by(2)) {
            stroke(&mut bindings, &mut target, 0, code, millis);
        }
        let ms = |millis: u128| millis * 1_000_000;
        let mut offered = vec![
            (b'4', ms(70)),
            (b'7', ms(80)),
            (b'5', ms(90)),
            (b'6', ms(120)),
        ];
        offered.extend((b'2'..=b'9').zip((130..).step_by(2).map(ms)));
        assert_eq!(target.approvals, offered);
        assert_eq!(
            target.secret_roles,
            [Selection::Elevation(Elevation::Rollback)]
        );
        // Enter went where it always goes, and confirms no elevation.
        assert_eq!(target.confirmations, [ms(30)]);
        assert_eq!(target.draining_events, 0);
    }

    /// A digit the attempt says ended its request drains the screen as
    /// Escape does: capture holds until the digit is released, then
    /// attention closes. A digit that did not end it changes nothing.
    #[test]
    fn a_digit_that_ends_the_request_drains_the_screen_as_escape_does() {
        let mut bindings = rollback_bindings().into_inner().unwrap();
        let mut target = RecordingTarget::default();
        for (code, value) in [
            (KEY_LEFTCTRL, KEY_PRESS),
            (KEY_LEFTALT, KEY_PRESS),
            (KEY_ESC, KEY_PRESS),
            (KEY_ESC, KEY_RELEASE),
            (KEY_LEFTCTRL, KEY_RELEASE),
            (KEY_LEFTALT, KEY_RELEASE),
            (KEY_B, KEY_PRESS),
            (KEY_B, KEY_RELEASE),
            (KEY_4, KEY_PRESS),
            (KEY_4, KEY_RELEASE),
        ] {
            deliver(&mut bindings, &mut target, 0, key(code, value));
        }
        assert!(bindings.attention == AttentionState::Open);
        assert_eq!(target.draining_events, 0);
        target.approval_ends = true;
        deliver(&mut bindings, &mut target, 0, key(KEY_8, KEY_PRESS));
        assert!(bindings.attention == AttentionState::Draining);
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.attention_events, [true]);
        // Draining, nothing more is offered.
        deliver(&mut bindings, &mut target, 0, key(KEY_7, KEY_PRESS));
        deliver(&mut bindings, &mut target, 0, key(KEY_7, KEY_RELEASE));
        assert_eq!(target.attention_events, [true]);
        deliver(&mut bindings, &mut target, 0, key(KEY_8, KEY_RELEASE));
        assert!(bindings.attention == AttentionState::Closed);
        assert_eq!(target.attention_events, [true, false]);
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.approvals.len(), 2);
    }

    /// A security key's own keyboard neither selects `B` or `H` nor types
    /// an approval-key digit; the keyboard's do, and the key's Escape still
    /// cancels.
    #[test]
    fn a_security_keys_keyboard_cannot_select_b_or_type_the_key() {
        use crate::secret_client::{Elevation, Selection};
        const KEY: usize = 1;
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: BTreeSet::from([KEY]),
            ..KeyBindings::default()
        });
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(
            &target,
            &bindings,
            KEY,
            &presses(&[KEY_B, KEY_H, KEY_4, KEY_7], 20),
        );
        {
            let target = target.lock().unwrap();
            assert!(target.notices.is_empty() && target.secret_roles.is_empty());
            assert!(target.approvals.is_empty());
        }
        read_reports(&target, &bindings, 0, &presses(&[KEY_B], 30));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_4, KEY_7], 40));
        assert!(target.lock().unwrap().approvals.is_empty());
        read_reports(&target, &bindings, 0, &presses(&[KEY_4], 50));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_ESC], 60));
        let target = target.lock().unwrap();
        assert_eq!(
            target.secret_roles,
            [Selection::Elevation(Elevation::Rollback)]
        );
        assert!(target.notices.is_empty());
        assert_eq!(target.approvals, [(b'4', 50_000_000)]);
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.attention_events, [true, false]);
    }

    /// On the lock surface the menu's letters select nothing, `B` among
    /// them: the chord's unlock is the lifetime's one selection.
    #[test]
    fn b_selects_nothing_on_the_lock_surface() {
        use crate::secret_client::{LoginSelection, Selection};
        let target = locked_target();
        let bindings = attention_bindings(Some(ENROLLED.to_vec()));
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_B, KEY_H], 20));
        let target = target.lock().unwrap();
        assert_eq!(
            target.secret_roles,
            [Selection::Login(LoginSelection::Unlock)]
        );
        assert!(target.notices.is_empty());
    }

    #[test]
    fn a_security_keys_keyboard_cannot_choose_a_login_operation() {
        use crate::attention::Notice;
        use crate::authority::consent::Slot;
        use crate::secret_client::{LoginSelection, Selection};
        const KEY: usize = 1;
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: BTreeSet::from([KEY]),
            login: login_answer(&ENROLLED),
            ..KeyBindings::default()
        });
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_K, KEY_1], 20));
        assert!(target.lock().unwrap().notices.is_empty());
        read_reports(&target, &bindings, 0, &presses(&[KEY_K], 30));
        read_reports(
            &target,
            &bindings,
            KEY,
            &presses(&[KEY_1, KEY_2, KEY_A, KEY_D], 40),
        );
        assert_eq!(target.lock().unwrap().notices, [Notice::LoginKeys]);
        read_reports(&target, &bindings, 0, &presses(&[KEY_D], 50));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_2, KEY_ENTER], 60));
        read_reports(&target, &bindings, 0, &presses(&[KEY_2], 70));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_ENTER], 80));
        {
            let target = target.lock().unwrap();
            assert!(target.secret_roles.is_empty());
            assert_eq!(
                target.notices,
                [
                    Notice::LoginKeys,
                    Notice::Removing { keys: 3, chosen: 0 },
                    Notice::Removing {
                        keys: 3,
                        chosen: 0b010
                    },
                ]
            );
        }
        read_reports(&target, &bindings, 0, &presses(&[KEY_ENTER], 90));
        assert_eq!(
            target.lock().unwrap().secret_roles,
            [Selection::Login(LoginSelection::Remove(vec![Slot {
                position: 2,
                key: ENROLLED[1],
            }]))]
        );
    }

    #[test]
    fn an_unpainted_key_management_screen_selects_nothing() {
        use crate::attention::Notice;
        let target = Mutex::new(RecordingTarget {
            notice_fails: true,
            ..RecordingTarget::default()
        });
        let bindings = attention_bindings(Some(ENROLLED.to_vec()));
        read_reports(&target, &bindings, 0, &chord_reports(10));
        let typed = target.lock().unwrap().keys.len();
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_K, KEY_1, KEY_A, KEY_D, KEY_U], 20),
        );
        let target = target.lock().unwrap();
        assert_eq!(target.notices, [Notice::LoginKeys]);
        assert!(target.secret_roles.is_empty());
        // Capture stays: the keys reached no client.
        assert_eq!(target.keys.len(), typed);
        assert!(bindings.lock().unwrap().attention == AttentionState::Open);
    }

    /// A key-management screen whose paint is owed, its flip not yet
    /// complete, takes no choice: `K` then `1` before the screen reaches
    /// glass selects nothing, and the same press once it has selects.
    #[test]
    fn a_key_management_screen_not_yet_on_glass_selects_nothing() {
        use crate::attention::Notice;
        use crate::authority::consent::Slot;
        use crate::secret_client::{LoginSelection, Selection};
        let target = Mutex::new(RecordingTarget {
            notices_owed: true,
            ..RecordingTarget::default()
        });
        let bindings = attention_bindings(Some(ENROLLED.to_vec()));
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_K, KEY_1, KEY_D], 20));
        {
            let target = target.lock().unwrap();
            assert_eq!(target.notices, [Notice::LoginKeys]);
            assert!(target.secret_roles.is_empty());
        }
        let clock = Arc::clone(&target.lock().unwrap().clock);
        clock.publish_for_test(1);
        // On glass: `D` opens removal, whose own screen is owed in turn.
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_D, KEY_2, KEY_ENTER], 30),
        );
        {
            let target = target.lock().unwrap();
            assert_eq!(
                target.notices,
                [Notice::LoginKeys, Notice::Removing { keys: 3, chosen: 0 }]
            );
            assert!(target.secret_roles.is_empty());
        }
        clock.publish_for_test(2);
        read_reports(&target, &bindings, 0, &presses(&[KEY_2], 40));
        // Enter waits for the screen showing the choice it would send.
        read_reports(&target, &bindings, 0, &presses(&[KEY_ENTER], 50));
        assert!(target.lock().unwrap().secret_roles.is_empty());
        clock.publish_for_test(3);
        read_reports(&target, &bindings, 0, &presses(&[KEY_ENTER], 60));
        assert_eq!(
            target.lock().unwrap().secret_roles,
            [Selection::Login(LoginSelection::Remove(vec![Slot {
                position: 2,
                key: ENROLLED[1],
            }]))]
        );
        // And for a choice made on the first screen.
        let target = Mutex::new(RecordingTarget {
            notices_owed: true,
            ..RecordingTarget::default()
        });
        let bindings = attention_bindings(None);
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_K, KEY_1], 20));
        assert!(target.lock().unwrap().secret_roles.is_empty());
        let clock = Arc::clone(&target.lock().unwrap().clock);
        clock.publish_for_test(1);
        read_reports(&target, &bindings, 0, &presses(&[KEY_1], 30));
        assert_eq!(
            target.lock().unwrap().secret_roles,
            [Selection::Login(LoginSelection::Enroll(1))]
        );
    }

    // The PIN field.

    /// Attention open and `K` then `1` chosen, with the target's field
    /// open as root's `0c` opens the attempt's; `excluded` are security
    /// keys' own keyboards.
    fn pin_field(excluded: &[usize]) -> (Mutex<RecordingTarget>, Mutex<KeyBindings>) {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: excluded.iter().copied().collect(),
            ..KeyBindings::default()
        });
        read_reports(&target, &bindings, 0, &chord_reports(10));
        read_reports(&target, &bindings, 0, &presses(&[KEY_K, KEY_1], 20));
        {
            let mut target = target.lock().unwrap();
            assert_eq!(target.secret_roles.len(), 1);
            // Neither the menu's choice nor the screen's typed anything.
            assert_eq!(target.field_keys, 0);
            target.field.open();
        }
        (target, bindings)
    }

    fn typed(target: &Mutex<RecordingTarget>) -> Vec<u8> {
        target.lock().unwrap().field.typed().to_vec()
    }

    /// `codes` pressed and released while `modifier` is held.
    fn with_held(modifier: u16, codes: &[u16], from: u32) -> Vec<Event> {
        let mut events = vec![at_millis(key(modifier, KEY_PRESS), from), syn(from)];
        events.extend(presses(codes, from + 1));
        let end = from + 1 + 2 * u32::try_from(codes.len()).unwrap();
        events.extend([at_millis(key(modifier, KEY_RELEASE), end), syn(end)]);
        events
    }

    #[test]
    fn the_pin_field_types_the_us_keymap() {
        let (target, bindings) = pin_field(&[]);
        let forwarded = target.lock().unwrap().keys.len();
        let read = |events: &[Event]| read_reports(&target, &bindings, 0, events);
        read(&presses(&[KEY_A, KEY_1, KEY_SPACE, KEY_SLASH], 30));
        assert_eq!(typed(&target), b"a1 /");
        // Each with its press's evdev time, which the attempt compares with
        // its field's presentation.
        assert_eq!(
            target.lock().unwrap().field_times,
            [30, 32, 34, 36].map(|millis: u128| millis * 1_000_000)
        );
        // Shift's level, from either Shift.
        read(&with_held(
            KEY_LEFTSHIFT,
            &[KEY_A, KEY_1, KEY_SLASH, KEY_APOSTROPHE],
            40,
        ));
        read(&with_held(KEY_RIGHTSHIFT, &[KEY_GRAVE, KEY_EQUAL], 60));
        assert_eq!(typed(&target), b"a1 /A!?\"~+");
        // The keypad's characters whatever Shift and NumLock say.
        read(&presses(&[KEY_KP7, KEY_KPDOT], 80));
        read(&presses(&[KEY_NUMLOCK, KEY_KP0], 90));
        read(&with_held(KEY_LEFTSHIFT, &[KEY_KP3, KEY_KPPLUS], 100));
        assert_eq!(typed(&target), b"a1 /A!?\"~+7.03+");
        // Caps Lock changes no case, and keys outside the map type nothing.
        read(&presses(&[KEY_CAPSLOCK, KEY_B, KEY_UP, KEY_LEFTMETA], 120));
        assert_eq!(typed(&target), b"a1 /A!?\"~+7.03+b");
        // Under Control, Alt or Super nothing types, erases or submits.
        for (modifier, from) in [
            (KEY_LEFTCTRL, 140),
            (KEY_RIGHTALT, 160),
            (KEY_LEFTMETA, 180),
        ] {
            read(&with_held(
                modifier,
                &[KEY_C, KEY_BACKSPACE, KEY_ENTER],
                from,
            ));
        }
        assert_eq!(typed(&target), b"a1 /A!?\"~+7.03+b");
        // Backspace erases one byte a press; Enter submits.
        read(&presses(&[KEY_BACKSPACE, KEY_BACKSPACE, KEY_KPENTER], 200));
        let target = target.lock().unwrap();
        assert_eq!(target.field.typed(), b"a1 /A!?\"~+7.03");
        assert!(target.field.submitted());
        // Each change showed only its masked length.
        assert_eq!(
            target.field_lengths,
            (1..=16).chain([15, 14]).collect::<Vec<_>>()
        );
        // Nothing of it reached a client.
        assert_eq!(target.keys.len(), forwarded);
    }

    #[test]
    fn the_pin_field_takes_four_to_sixty_three_bytes() {
        let (target, bindings) = pin_field(&[]);
        let read = |events: &[Event]| read_reports(&target, &bindings, 0, events);
        // Enter on three bytes does nothing.
        read(&presses(&[KEY_1, KEY_2, KEY_3, KEY_ENTER], 30));
        assert_eq!(typed(&target), b"123");
        assert!(!target.lock().unwrap().field.submitted());
        // A 64th byte refuses.
        read(&presses(&[KEY_9; 61], 40));
        assert_eq!(typed(&target).len(), 63);
        read(&presses(&[KEY_0], 200));
        assert_eq!(typed(&target).len(), 63);
        assert_eq!(
            target.lock().unwrap().field_lengths,
            (1..=63).collect::<Vec<_>>()
        );
        read(&presses(&[KEY_ENTER], 210));
        assert!(target.lock().unwrap().field.submitted());
        // After Enter nothing types or erases.
        read(&presses(&[KEY_BACKSPACE, KEY_5], 220));
        assert_eq!(typed(&target).len(), 63);
        // Four bytes are enough.
        let (target, bindings) = pin_field(&[]);
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_1, KEY_2, KEY_3, KEY_4, KEY_ENTER], 30),
        );
        assert!(target.lock().unwrap().field.submitted());
    }

    /// Only a fresh press types: a key's repeats and a second keyboard's
    /// press of a key the first holds type nothing, Backspace's included.
    #[test]
    fn a_held_key_types_once() {
        let (target, bindings) = pin_field(&[]);
        let held = |code: u16, from: u32| {
            let mut events = vec![at_millis(key(code, KEY_PRESS), from), syn(from)];
            for time in from + 1..from + 10 {
                events.extend([at_millis(key(code, KEY_REPEAT), time), syn(time)]);
            }
            events.extend([at_millis(key(code, KEY_RELEASE), from + 10), syn(from + 10)]);
            events
        };
        read_reports(&target, &bindings, 0, &held(KEY_7, 30));
        read_reports(&target, &bindings, 0, &presses(&[KEY_1, KEY_2], 50));
        read_reports(&target, &bindings, 0, &held(KEY_BACKSPACE, 60));
        assert_eq!(typed(&target), b"71");
        // Two keyboards at once, so neither reader's end releases its keys.
        let (mut first_resync, mut second_resync) = (|| None, || None);
        let mut first = DeviceState::new(None, AbsoluteKind::Tablet, &mut first_resync, true);
        let mut second = DeviceState::new(None, AbsoluteKind::Tablet, &mut second_resync, true);
        apply_device_event(&target, key(KEY_8, KEY_PRESS), 0, &bindings, &mut first).unwrap();
        for value in [KEY_PRESS, KEY_RELEASE] {
            apply_device_event(&target, key(KEY_8, value), 2, &bindings, &mut second).unwrap();
        }
        apply_device_event(&target, key(KEY_8, KEY_RELEASE), 0, &bindings, &mut first).unwrap();
        assert_eq!(typed(&target), b"718");
        assert_eq!(target.lock().unwrap().field_keys, 5);
    }

    /// A security key's own keyboard types nothing into the field, submits
    /// and erases nothing, and its Shift is no Shift; its Escape cancels.
    #[test]
    fn a_security_keys_keyboard_types_no_pin() {
        const KEY: usize = 1;
        let (target, bindings) = pin_field(&[KEY]);
        // A touch: modhex and Enter, and a Shift.
        read_reports(
            &target,
            &bindings,
            KEY,
            &presses(&[KEY_C, KEY_B, KEY_D, KEY_E, KEY_ENTER], 30),
        );
        read_reports(
            &target,
            &bindings,
            KEY,
            &with_held(KEY_LEFTSHIFT, &[KEY_F], 40),
        );
        assert_eq!(target.lock().unwrap().field_keys, 0);
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_1, KEY_2, KEY_3, KEY_4], 50),
        );
        // Its held Shift does not shift the keyboard's key, and the
        // keyboard's held Shift does: both devices at once.
        let (mut keyboard_resync, mut key_resync) = (|| None, || None);
        let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut keyboard_resync, true);
        let mut token = DeviceState::new(None, AbsoluteKind::Tablet, &mut key_resync, true);
        let mut apply = |code, value, device| {
            let state = if device == KEY {
                &mut token
            } else {
                &mut keyboard
            };
            apply_device_event(&target, key(code, value), device, &bindings, state).unwrap();
        };
        apply(KEY_LEFTSHIFT, KEY_PRESS, KEY);
        apply(KEY_A, KEY_PRESS, 0);
        apply(KEY_A, KEY_RELEASE, 0);
        apply(KEY_LEFTSHIFT, KEY_RELEASE, KEY);
        apply(KEY_RIGHTSHIFT, KEY_PRESS, 0);
        apply(KEY_A, KEY_PRESS, 0);
        apply(KEY_A, KEY_RELEASE, 0);
        apply(KEY_RIGHTSHIFT, KEY_RELEASE, 0);
        apply(KEY_BACKSPACE, KEY_PRESS, 0);
        apply(KEY_BACKSPACE, KEY_RELEASE, 0);
        read_reports(
            &target,
            &bindings,
            KEY,
            &presses(&[KEY_BACKSPACE, KEY_ENTER], 70),
        );
        {
            let target = target.lock().unwrap();
            assert_eq!(target.field.typed(), b"1234a");
            assert!(!target.field.submitted());
            assert_eq!(target.field_keys, 7);
        }
        // Its Escape cancels: attention drains and the field is gone.
        read_reports(&target, &bindings, KEY, &presses(&[KEY_ESC], 80));
        let target = target.lock().unwrap();
        assert_eq!(target.draining_events, 1);
        assert!(!target.field.is_open() && target.field.typed().is_empty());
        assert_eq!(target.field_keys, 7);
    }

    /// Escape cancels the whole operation: it drains, offers the field
    /// nothing, and nothing types afterwards.
    #[test]
    fn escape_cancels_the_pin_field() {
        let (target, bindings) = pin_field(&[]);
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_1, KEY_2, KEY_ESC, KEY_3], 30),
        );
        let target = target.lock().unwrap();
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.field_keys, 2);
        assert!(!target.field.is_open() && target.field.typed().is_empty());
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
    }

    /// Keys reach the field only in an attention lifetime whose operation
    /// was chosen: outside attention they are a client's, and on the menu
    /// or the key-management screen they choose.
    #[test]
    fn only_a_chosen_operation_takes_pin_keys() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(None);
        target.lock().unwrap().field.open();
        read_reports(&target, &bindings, 0, &presses(&[KEY_1, KEY_ENTER], 10));
        read_reports(&target, &bindings, 0, &chord_reports(20));
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_Q, KEY_K, KEY_Z, KEY_A], 30),
        );
        let target = target.lock().unwrap();
        assert_eq!(target.secret_roles.len(), 1);
        assert_eq!(target.field_keys, 0);
        assert!(target.field.typed().is_empty());
    }

    // The lock surface.

    fn locked_target() -> Mutex<RecordingTarget> {
        Mutex::new(RecordingTarget {
            locked: true,
            ..RecordingTarget::default()
        })
    }

    /// On the lock surface the chord is the selection: it opens attention
    /// straight into a login unlock, with no menu, and that is the
    /// lifetime's one operation. Escape ends it, and the next chord is a
    /// new lifetime with a new unlock.
    #[test]
    fn the_lock_surfaces_chord_selects_its_unlock_with_no_menu() {
        use crate::secret_client::{LoginSelection, Selection};
        let unlock = Selection::Login(LoginSelection::Unlock);
        let target = locked_target();
        let bindings = attention_bindings(Some(ENROLLED.to_vec()));
        read_reports(&target, &bindings, 0, &chord_reports(10));
        {
            let target = target.lock().unwrap();
            assert_eq!(target.attention_events, [true]);
            assert_eq!(target.secret_roles, std::slice::from_ref(&unlock));
            assert!(target.notices.is_empty());
        }
        // The menu's letters and the key-management screen select nothing.
        read_reports(
            &target,
            &bindings,
            0,
            &presses(&[KEY_K, KEY_1, KEY_A, KEY_D, KEY_U, KEY_I, KEY_W], 20),
        );
        {
            let target = target.lock().unwrap();
            assert_eq!(target.secret_roles, std::slice::from_ref(&unlock));
            assert!(target.notices.is_empty());
        }
        read_reports(&target, &bindings, 0, &presses(&[KEY_ESC], 40));
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        let mut reopen = vec![syn(50)];
        reopen.extend(chord_reports(51));
        read_reports(&target, &bindings, 0, &reopen);
        {
            let target = target.lock().unwrap();
            assert_eq!(target.attention_events, [true, false, true]);
            assert_eq!(target.secret_roles, [unlock.clone(), unlock]);
        }
        // Unlocked, the same chord opens the menu as before.
        let target = Mutex::new(RecordingTarget::default());
        let bindings = attention_bindings(None);
        read_reports(&target, &bindings, 0, &chord_reports(10));
        assert!(target.lock().unwrap().secret_roles.is_empty());
    }

    /// A security key's own keyboard cannot open the lock surface's unlock:
    /// its chord, or its Control and Alt under another keyboard's Escape,
    /// leaves the lock surface as it was. Its Escape still cancels the
    /// unlock another keyboard opened.
    #[test]
    fn a_security_keys_keyboard_cannot_open_the_lock_surfaces_unlock() {
        use crate::secret_client::{LoginSelection, Selection};
        const KEY: usize = 1;
        let target = locked_target();
        let bindings = Mutex::new(KeyBindings {
            attention_enabled: true,
            attention_excluded: BTreeSet::from([KEY]),
            login: login_answer(&ENROLLED),
            ..KeyBindings::default()
        });
        read_reports(&target, &bindings, KEY, &chord_reports(10));
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        // Its Control and Alt held, the keyboard's Escape.
        let (mut key_resync, mut keyboard_resync) = (|| None, || None);
        let mut token = DeviceState::new(None, AbsoluteKind::Tablet, &mut key_resync, true);
        let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut keyboard_resync, true);
        for (code, value, device) in [
            (KEY_LEFTCTRL, KEY_PRESS, KEY),
            (KEY_LEFTALT, KEY_PRESS, KEY),
            (KEY_ESC, KEY_PRESS, 0),
            (KEY_ESC, KEY_RELEASE, 0),
            (KEY_LEFTALT, KEY_RELEASE, KEY),
            (KEY_LEFTCTRL, KEY_RELEASE, KEY),
        ] {
            let state = if device == KEY {
                &mut token
            } else {
                &mut keyboard
            };
            apply_device_event(&target, key(code, value), device, &bindings, state).unwrap();
        }
        assert!(bindings.lock().unwrap().attention == AttentionState::Closed);
        {
            let target = target.lock().unwrap();
            assert!(target.attention_events.is_empty());
            assert!(target.secret_roles.is_empty());
        }
        // Another keyboard's chord opens it, and the key's Escape cancels.
        read_reports(&target, &bindings, 0, &chord_reports(20));
        read_reports(&target, &bindings, KEY, &presses(&[KEY_ESC], 30));
        let target = target.lock().unwrap();
        assert_eq!(
            target.secret_roles,
            [Selection::Login(LoginSelection::Unlock)]
        );
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.attention_events, [true, false]);
    }

    /// The lock surface runs no ordinary binding: no terminal, launcher,
    /// sheet or workspace.
    #[test]
    fn the_lock_surface_runs_no_binding() {
        let target = locked_target();
        let bindings = attention_bindings(None);
        let chords = [KEY_T, KEY_ENTER, KEY_SLASH, KEY_1, KEY_LEFT, KEY_F];
        read_reports(&target, &bindings, 0, &with_held(KEY_LEFTMETA, &chords, 10));
        let target = target.lock().unwrap();
        assert!(target.launched.is_empty());
        assert!(target.launcher_actions.is_empty());
        assert!(target.help_actions.is_empty());
        assert!(target.commands.is_empty());
        assert!(target.secret_roles.is_empty());
        let bindings = bindings.lock().unwrap();
        assert!(!bindings.launcher_open && !bindings.help_open);
    }

    const LOCK_NONCE: [u8; 32] = [9; 32];
    /// Evdev time far past this host's monotonic clock, so that a press is
    /// after every paint the runtime timed.
    const LATER: u128 = 1_000_000_000_000_000_000;
    /// The client window's colour behind the lock.
    const CLIENT: [u8; 4] = [1, 2, 3, 0];

    fn later(events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .map(|mut event| {
                event.timestamp += LATER;
                event
            })
            .collect()
    }

    /// Root as the private client meets it: its answers in order, and the
    /// requests it was sent.
    struct Root {
        replies: std::collections::VecDeque<Vec<u8>>,
        calls: Vec<Vec<u8>>,
    }

    impl crate::authority::Exchange for Root {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.calls.push(bytes.to_vec());
            Ok(())
        }
        fn receive(&mut self) -> Result<Vec<u8>, String> {
            self.replies
                .pop_front()
                .ok_or_else(|| "root has no answer".to_string())
        }
    }

    fn root(replies: Vec<Vec<u8>>) -> Root {
        Root {
            replies: replies.into(),
            calls: Vec::new(),
        }
    }

    impl Root {
        fn sent(&self, tag: u8) -> usize {
            self.calls
                .iter()
                .filter(|call| call.first() == Some(&tag))
                .count()
        }
    }

    /// Polls until root's answers run out.
    fn run(client: &mut crate::secret_client::Client, root: &mut Root) -> Result<(), String> {
        for _ in 0..64 {
            if root.replies.is_empty() {
                break;
            }
            client.tick(root)?;
        }
        Ok(())
    }

    fn unlock_step(
        step: crate::authority::consent::LoginStep,
    ) -> crate::authority::consent::Request {
        crate::authority::consent::Request::new(
            LOCK_NONCE,
            1000,
            crate::authority::consent::Operation::LoginUnlock {
                account: 1000,
                before: 2,
                after: 2,
                step,
            },
        )
        .unwrap()
    }

    fn identify() -> crate::authority::consent::Request {
        unlock_step(crate::authority::consent::LoginStep::Identify)
    }

    fn pin_step() -> crate::authority::consent::Request {
        unlock_step(crate::authority::consent::LoginStep::Unlock {
            key: [0xa2; 4],
            retries: 8,
        })
    }

    fn status(code: u8, request: &crate::authority::consent::Request) -> Vec<u8> {
        [&[0x91, code][..], &request.encode()].concat()
    }

    fn started() -> Vec<u8> {
        [&[0x9b, 1][..], &LOCK_NONCE].concat()
    }

    /// Root's answers from `1b` until the PIN step asks for its PIN: the
    /// identify step presented and worked on, then the PIN step presented.
    fn to_pin() -> Vec<Vec<u8>> {
        let (identify, step) = (identify(), pin_step());
        vec![
            started(),
            vec![0x91, 0x0b],
            status(3, &identify),
            status(4, &identify),
            vec![0x93],
            status(3, &identify),
            status(4, &step),
            vec![0x93],
            status(0x0c, &step),
        ]
    }

    /// A paired seat over a locked 800x600 output with one client window
    /// behind the lock, its attempts queued for the worker the test plays,
    /// and root's last `1a` answer enrolled, watched as the compositor
    /// watches it.
    struct LockedSeat {
        cleanup: Cleanup,
        runtime: Arc<Mutex<Runtime>>,
        bindings: Arc<Mutex<KeyBindings>>,
        target: Arc<Mutex<LiveInputTarget>>,
        queued: crate::authority::Queued,
        surface: crate::scene::SurfaceKey,
        login: crate::authority::Login,
    }

    impl LockedSeat {
        fn new(excluded: &[usize]) -> Self {
            let seat = Self::open(excluded);
            seat.lock().unwrap();
            seat
        }

        fn lock(&self) -> Result<crate::runtime::NoticePresentation, String> {
            let mut bindings = self.bindings.lock().unwrap();
            let mut target = self.target.lock().unwrap();
            lock_session(&mut *target, &mut bindings)
        }

        /// The same seat before it locks.
        fn open(excluded: &[usize]) -> Self {
            let cleanup = Cleanup(std::env::temp_dir().join(format!(
                "td-lock-{}-{}",
                std::process::id(),
                TEST_SEQ.fetch_add(1, Ordering::Relaxed)
            )));
            let framebuffer =
                crate::framebuffer::Framebuffer::test_file(&cleanup.0, 800, 600, 3200).unwrap();
            let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
            let surface = crate::scene::SurfaceKey {
                client: 1,
                object: 1,
            };
            {
                let mut runtime = runtime.lock().unwrap();
                runtime.enable_attention(true);
                runtime
                    .commit(
                        surface,
                        crate::buffer::Surface::from_shm_pixels(
                            100,
                            100,
                            CLIENT.repeat(10_000),
                            crate::scene::SHM_XRGB8888,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                assert_eq!(runtime.keyboard_snapshot().focus, Some(surface));
            }
            let login = login_answer(&ENROLLED);
            crate::runtime::watch_login(&runtime, &login);
            runtime
                .lock()
                .unwrap()
                .follow_login(&login.current().unwrap())
                .unwrap();
            let bindings = Arc::new(Mutex::new(KeyBindings {
                attention_enabled: true,
                attention_excluded: excluded.iter().copied().collect(),
                login: login.clone(),
                ..KeyBindings::default()
            }));
            let (launcher, queued) = crate::authority::Queued::launcher();
            let target = Arc::new_cyclic(|own| {
                Mutex::new(LiveInputTarget {
                    runtime: Arc::clone(&runtime),
                    launches: LaunchBackend::Authority(launcher),
                    secret_attempt: None,
                    seat: Seat {
                        bindings: Arc::downgrade(&bindings),
                        target: own.clone(),
                    },
                })
            });
            let seat = Self {
                cleanup,
                runtime,
                bindings,
                target,
                queued,
                surface,
                login,
            };
            assert!(seat.client_shown());
            seat
        }

        /// `events` read from `device` by the whole device dispatcher,
        /// after a report boundary that any new cutoff discards.
        fn read(&self, device: usize, from: u32, events: Vec<Event>) {
            self.try_read(device, from, events).unwrap();
        }

        /// The same read, answering the dispatcher's own result.
        fn try_read(&self, device: usize, from: u32, events: Vec<Event>) -> Result<(), String> {
            let mut all = vec![syn(from)];
            all.extend(events);
            let data = later(all).into_iter().flat_map(encode).collect();
            read_device(
                Path::new("event-test"),
                &mut ChunkedReader::new(data, Vec::new()),
                device,
                self.target.as_ref(),
                self.bindings.as_ref(),
                None,
                &mut || None,
            )
        }

        fn chord(&self, from: u32) {
            self.read(0, from, chord_reports(from + 1));
        }

        fn press(&self, codes: &[u16], from: u32) {
            self.read(0, from, presses(codes, from + 1));
        }

        /// `Super+l`, each change its own report.
        fn super_l(&self, from: u32) {
            self.read(0, from, super_l_reports(from + 1));
        }

        /// The keys root's client, the window's, was handed since `events`
        /// subscribed.
        fn client_keys(
            events: &std::sync::mpsc::Receiver<crate::runtime::KeyboardDelivery>,
        ) -> Vec<u32> {
            use crate::keyboard::KeyboardEvent;
            use crate::runtime::KeyboardDelivery;
            events
                .try_iter()
                .filter_map(|delivery| match delivery {
                    KeyboardDelivery::Event(event) => match event.event {
                        KeyboardEvent::Key { input, .. } => Some(input.key),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        }

        fn locked(&self) -> bool {
            self.runtime.lock().unwrap().session_locked()
        }

        fn attention_open(&self) -> bool {
            self.bindings.lock().unwrap().attention != AttentionState::Closed
        }

        fn shown(&self) -> Option<crate::attention::Notice> {
            self.runtime.lock().unwrap().attention_shown()
        }

        fn field(&self) -> Option<crate::attention::Field> {
            self.runtime.lock().unwrap().attention_field_shown()
        }

        fn glass(&self) -> Vec<u8> {
            std::fs::read(&self.cleanup.0).unwrap()
        }

        /// The lock surface for root's last answer, drawn independently
        /// of the runtime.
        fn lock_surface(&self) -> Vec<u8> {
            let rows = crate::attention::lock_rows(self.login.current().as_ref());
            let mut frame = vec![0; 800 * 600 * 4];
            crate::attention::paint_lock(&mut frame, 800, 600, 3200, &rows);
            frame
        }

        fn client_shown(&self) -> bool {
            self.glass().as_chunks::<4>().0.contains(&CLIENT)
        }

        /// The chord's unlock attempt, as the worker receives it.
        fn unlock(&self, from: u32) -> Arc<crate::secret_client::Attempt> {
            use crate::secret_client::{LoginSelection, Selection};
            self.chord(from);
            let attempt = self.queued.attempt().unwrap();
            assert_eq!(
                attempt.selection(),
                &Selection::Login(LoginSelection::Unlock)
            );
            attempt
        }

        /// An unlock driven to its committed last step: the chord, the
        /// identify step, the PIN step whose field the person types into
        /// on the keyboard, the PIN, the touch and the commit.
        fn committed(&self) -> (crate::secret_client::Client, Root) {
            let attempt = self.unlock(10);
            let step = pin_step();
            let mut root = root(to_pin());
            let mut client = crate::secret_client::Client::trusting_memory();
            client.start(&mut root, attempt).unwrap();
            run(&mut client, &mut root).unwrap();
            assert_eq!(self.field(), Some(crate::attention::Field::Pin(0)));
            self.press(&[KEY_1, KEY_2, KEY_3, KEY_4, KEY_ENTER], 40);
            assert_eq!(self.field(), Some(crate::attention::Field::Pin(4)));
            root.replies.extend([
                status(0x0c, &step),
                vec![0x9c, 0],
                status(3, &step),
                status(5, &step),
                vec![0x94],
                status(3, &step),
            ]);
            run(&mut client, &mut root).unwrap();
            assert_eq!(root.sent(0x14), 1);
            assert_eq!(self.field(), Some(crate::attention::Field::Touch));
            (client, root)
        }
    }

    /// Locked, the output shows the lock surface and no client pixel, a
    /// client's new frame changes nothing on glass, and no client is
    /// focused or given a key, a modifier or the pointer. The chord's
    /// attention screen shows no menu over it.
    #[test]
    fn the_lock_surface_hides_every_client_and_withholds_input() {
        let seat = LockedSeat::new(&[]);
        assert!(seat.glass() == seat.lock_surface());
        assert!(!seat.client_shown());
        let mut runtime = seat.runtime.lock().unwrap();
        let before = runtime.keyboard_snapshot();
        assert_eq!(before.focus, None);
        runtime.key(key(KEY_A, KEY_PRESS).key_input()).unwrap();
        runtime
            .modifiers(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
            .unwrap();
        runtime
            .pointer_frame(1, 40, 40, &[], PointerScroll::default())
            .unwrap();
        assert_eq!(runtime.keyboard_snapshot(), before);
        assert!(runtime.pointer_snapshot().focus.is_none());
        runtime
            .commit(
                seat.surface,
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [6, 7, 8, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(seat.glass() == seat.lock_surface());
        assert_eq!(runtime.keyboard_snapshot().focus, None);
        // Leaving it needs an open lifetime, whose screen shows no menu;
        // the entry refuses while one is open.
        let origin = EvdevOrigin { _private: () };
        assert!(runtime.unlock_session(&origin).is_err());
        runtime.attention(&origin, true).unwrap();
        assert_eq!(
            runtime.attention_shown(),
            Some(crate::attention::Notice::Pending)
        );
        assert!(runtime.lock_session(&origin).is_err());
        // Ended as Escape ends it, the lifetime admits the lock beneath it.
        runtime.drain_attention(&origin).unwrap();
        runtime.lock_session(&origin).unwrap();
        // Closed again, the lock surface, not a client, is what returns.
        runtime.attention(&origin, false).unwrap();
        assert_eq!(runtime.keyboard_snapshot().focus, None);
        drop(runtime);
        assert!(seat.glass() == seat.lock_surface());
        // Only the paired profile has a lock surface.
        let cleanup = Cleanup(std::env::temp_dir().join(format!(
            "td-lock-direct-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )));
        let mut direct = Runtime::new(
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 320, 200, 1280).unwrap(),
        );
        assert!(direct.lock_session(&test_origin()).is_err());
        assert!(!direct.session_locked());
    }

    /// Ctrl+Alt+Esc on the lock surface opens one attention lifetime, which
    /// carries the whole chained unlock: `1b 07`, the identify step, the
    /// PIN step and its field typed on the keyboard, the touch and the
    /// commit. Root's `06` leaves the lock surface and closes attention:
    /// the client window is on glass and focused again.
    #[test]
    fn the_chord_unlocks_through_one_lifetime_and_its_pin_field() {
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        // Committed, not yet reported: still locked, attention up.
        assert!(seat.locked() && seat.attention_open());
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(!seat.locked());
        assert!(!seat.attention_open());
        assert!(seat.target.lock().unwrap().secret_attempt.is_none());
        assert!(seat.client_shown());
        assert_eq!(
            seat.runtime.lock().unwrap().keyboard_snapshot().focus,
            Some(seat.surface)
        );
        // The operation's requests: one `1b 07`, one PIN, one commit.
        assert_eq!(root.calls.first(), Some(&vec![0x1b, 7]));
        assert_eq!(root.sent(0x1b), 1);
        assert_eq!(root.sent(0x13), 2);
        assert_eq!(root.sent(0x14), 1);
        let encoded = pin_step().encode();
        let pin = [
            &[0x1c, u8::try_from(encoded.len()).unwrap()][..],
            &encoded,
            b"1234",
        ]
        .concat();
        let pins: Vec<&Vec<u8>> = root
            .calls
            .iter()
            .filter(|call| call.first() == Some(&0x1c))
            .collect();
        assert_eq!(pins, [&pin]);
    }

    /// A failure leaves the session locked with its text on the trusted
    /// screen: this build's root answers an unlock NO RECORD, or with the
    /// directory missing DIRECTORY DAMAGED, before any description; a wrong
    /// PIN ends it too. Escape returns to the lock surface, and a second
    /// chord opens a new lifetime with a new unlock.
    #[test]
    fn a_failed_unlock_stays_locked_with_its_text() {
        let step = pin_step();
        let wrong = vec![
            status(0x0c, &step),
            vec![0x9c, 0],
            [&[0x91, 0x0d, 0x01, 7][..], &step.encode()].concat(),
        ];
        for (replies, typed, rows) in [
            (
                vec![started(), vec![0x91, 0x0d, 0x09, 0]],
                None,
                &["NO LOGIN KEYS ENROLLED"][..],
            ),
            (
                vec![started(), vec![0x91, 0x0d, 0x0a, 0]],
                None,
                &["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"][..],
            ),
            (to_pin(), Some(wrong), &["WRONG PIN"][..]),
        ] {
            let seat = LockedSeat::new(&[]);
            let first = seat.unlock(10);
            let mut root = root(replies);
            let mut client = crate::secret_client::Client::trusting_memory();
            client.start(&mut root, Arc::clone(&first)).unwrap();
            run(&mut client, &mut root).unwrap();
            if let Some(then) = typed {
                seat.press(&[KEY_9, KEY_9, KEY_9, KEY_9, KEY_ENTER], 20);
                root.replies.extend(then);
                run(&mut client, &mut root).unwrap();
                assert_eq!(root.sent(0x1c), 1);
            }
            assert!(root.replies.is_empty());
            assert!(seat.locked() && seat.attention_open());
            match seat.shown() {
                Some(crate::attention::Notice::Login(shown)) => assert_eq!(shown, rows),
                other => panic!("{other:?}"),
            }
            // Nothing more is asked of root for that operation.
            let calls = root.calls.len();
            client.tick(&mut root).unwrap();
            assert_eq!(root.calls.len(), calls);
            seat.press(&[KEY_ESC], 30);
            assert!(!seat.attention_open() && seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            let second = seat.unlock(40);
            assert!(!Arc::ptr_eq(&first, &second));
            assert!(seat.attention_open());
            assert_eq!(seat.shown(), Some(crate::attention::Notice::Pending));
        }
    }

    /// Root's `06` with a key still held leaves the lock surface and drains
    /// as Escape does, except that the success notice stays: it shows over
    /// `RELEASE KEYS AND BUTTONS`, with no client shown or focused, until
    /// the release closes attention onto the client.
    #[test]
    fn an_unlock_with_a_key_held_drains_before_it_closes() {
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        // Fed event by event: a read that ends releases its device.
        let mut pointer = PointerMotion::default();
        let feed = |pointer: &mut PointerMotion, value, time| {
            for event in later(vec![at_millis(key(KEY_A, value), time), syn(time)]) {
                apply(
                    seat.target.as_ref(),
                    event,
                    0,
                    seat.bindings.as_ref(),
                    pointer,
                    None,
                )
                .unwrap();
            }
        };
        feed(&mut pointer, KEY_PRESS, 61);
        assert!(seat.locked() && seat.attention_open());
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(!seat.locked());
        assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
        let mut drained = vec![0; 800 * 600 * 4];
        crate::attention::paint(
            &mut drained,
            800,
            600,
            3200,
            true,
            true,
            crate::attention::Notice::Login(&["SESSION UNLOCKED"]),
        );
        assert!(seat.glass() == drained);
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        feed(&mut pointer, KEY_RELEASE, 71);
        assert!(!seat.attention_open() && !seat.locked());
        assert!(seat.client_shown());
        assert_eq!(
            seat.runtime.lock().unwrap().keyboard_snapshot().focus,
            Some(seat.surface)
        );
    }

    /// Root's `06` and the seat that unlocked: the unlock driven to its
    /// success with no key held, attention closed onto the client.
    fn unlocked_seat() -> LockedSeat {
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(!seat.locked() && !seat.attention_open());
        seat
    }

    /// `events` read from `device` by its own reader, which has seen no
    /// report since the last close.
    fn read_events(seat: &LockedSeat, device: usize, state: &mut DeviceState, events: Vec<Event>) {
        for event in events {
            apply_device_event(
                seat.target.as_ref(),
                event,
                device,
                seat.bindings.as_ref(),
                state,
            )
            .unwrap();
        }
    }

    /// `events` as one batch read by the whole device dispatcher, with no
    /// report before them.
    fn read_batch(seat: &LockedSeat, device: usize, events: Vec<Event>) {
        let data = events.into_iter().flat_map(encode).collect();
        read_device(
            Path::new("event-test"),
            &mut ChunkedReader::new(data, Vec::new()),
            device,
            seat.target.as_ref(),
            seat.bindings.as_ref(),
            None,
            &mut || None,
        )
        .unwrap();
    }

    /// Root's `06`, which no key started, closes attention onto a settle
    /// window rather than each device's first-report discard: the
    /// person's first key after the unlock reaches the session, from the
    /// keyboard that typed the PIN and from another alike.
    #[test]
    fn the_first_key_after_an_unlock_reaches_the_session() {
        let seat = unlocked_seat();
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        for (device, code) in [(0, KEY_A), (1, KEY_B)] {
            let mut resync = || None;
            let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            read_events(&seat, device, &mut state, later(presses(&[code], 80)));
        }
        let (a, b) = (u32::from(KEY_A), u32::from(KEY_B));
        assert_eq!(LockedSeat::client_keys(&events), [a, a, b, b]);
        let bindings = seat.bindings.lock().unwrap();
        let cutoff = bindings.cutoff.unwrap();
        assert_eq!(bindings.settled, Some(cutoff + SELF_CLOSE_SETTLE));
        assert_eq!(bindings.keyless_close, None);
        // The first report taken past the window said its count.
        assert_eq!(bindings.settle_dropped, None);
    }

    /// A key pressed inside an unlock's settle window and released past
    /// it is dropped whole: neither its press nor its release reaches the
    /// session, nothing stays held, and the window counts the report it
    /// dropped. The next key reaches the session.
    #[test]
    fn a_key_straddling_an_unlocks_settle_window_is_dropped_whole() {
        let seat = unlocked_seat();
        let cutoff = seat.bindings.lock().unwrap().cutoff.unwrap();
        assert_eq!(seat.bindings.lock().unwrap().settle_dropped, Some(0));
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        let mut resync = || None;
        let mut state = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        let mut report = |code, value, at: u128| {
            let stamped = [key(code, value), syn(1)].map(|mut event| {
                event.timestamp = at;
                event
            });
            read_events(&seat, 0, &mut state, stamped.to_vec());
        };
        report(KEY_ENTER, KEY_PRESS, cutoff + SELF_CLOSE_SETTLE / 2);
        assert_eq!(seat.bindings.lock().unwrap().settle_dropped, Some(1));
        report(KEY_ENTER, KEY_RELEASE, cutoff + SELF_CLOSE_SETTLE * 3 / 2);
        {
            let bindings = seat.bindings.lock().unwrap();
            assert_eq!(bindings.settle_dropped, None);
            assert!(bindings.pressed.is_empty());
            assert!(bindings.forwarded.is_empty());
        }
        assert!(LockedSeat::client_keys(&events).is_empty());
        report(KEY_B, KEY_PRESS, cutoff + 3 * SELF_CLOSE_SETTLE);
        report(KEY_B, KEY_RELEASE, cutoff + 4 * SELF_CLOSE_SETTLE);
        let b = u32::from(KEY_B);
        assert_eq!(LockedSeat::client_keys(&events), [b, b]);
        assert!(seat.bindings.lock().unwrap().pressed.is_empty());
    }

    /// Escape's close keeps the first-report discard: the Escape
    /// release's own report absorbs it on the keyboard that closed, whose
    /// next key reaches the session, while another device's first report
    /// after the close is discarded however late, and its next is taken.
    #[test]
    fn an_escape_close_keeps_the_first_report_discard() {
        let seat = LockedSeat::open(&[]);
        let mut resync = || None;
        let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        read_events(&seat, 0, &mut keyboard, later(chord_reports(10)));
        assert!(seat.attention_open());
        read_events(&seat, 0, &mut keyboard, later(presses(&[KEY_ESC], 20)));
        assert!(!seat.attention_open());
        {
            let bindings = seat.bindings.lock().unwrap();
            assert!(bindings.cutoff.is_some());
            assert_eq!(bindings.settled, None);
            assert_eq!(bindings.settle_dropped, None);
        }
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        read_events(&seat, 0, &mut keyboard, later(presses(&[KEY_A], 30)));
        let mut resync = || None;
        let mut other = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
        read_events(&seat, 1, &mut other, later(presses(&[KEY_B, KEY_C], 40)));
        let (a, c) = (u32::from(KEY_A), u32::from(KEY_C));
        assert_eq!(LockedSeat::client_keys(&events), [a, a, c, c]);
    }

    /// Locking goes through the bindings: the launcher's and the help
    /// sheet's capture close with the overlays the runtime closes, so no
    /// key is routed to an overlay that is gone.
    #[test]
    fn locking_closes_the_launcher_and_help_capture() {
        for help in [false, true] {
            let seat = LockedSeat::open(&[]);
            {
                let mut bindings = seat.bindings.lock().unwrap();
                let mut runtime = seat.runtime.lock().unwrap();
                if help {
                    bindings.settle_help(Some(runtime.help(HelpAction::Toggle).unwrap()));
                } else {
                    runtime.launcher(LauncherAction::Open).unwrap();
                    bindings.settle_launcher(Some(runtime.launcher_visible()));
                }
                assert!(bindings.launcher_open || bindings.help_open);
            }
            seat.lock().unwrap();
            let bindings = seat.bindings.lock().unwrap();
            assert!(!bindings.launcher_open && !bindings.help_open, "{help}");
            let runtime = seat.runtime.lock().unwrap();
            assert!(!runtime.launcher_visible() && !runtime.help_visible());
        }
    }

    /// Escape keeps the session locked: before the commit it cancels, and
    /// after it root's success no longer unlocks, since Escape came first.
    #[test]
    fn escape_keeps_the_session_locked() {
        let seat = LockedSeat::new(&[]);
        let attempt = seat.unlock(10);
        let mut root = root(to_pin());
        let mut client = crate::secret_client::Client::trusting_memory();
        client.start(&mut root, attempt).unwrap();
        run(&mut client, &mut root).unwrap();
        seat.press(&[KEY_1, KEY_ESC], 40);
        assert!(!seat.attention_open() && seat.locked());
        root.replies.extend([
            vec![0x95, 0],
            [&[0x91, 0x0d, 0x80, 0][..], &pin_step().encode()].concat(),
        ]);
        run(&mut client, &mut root).unwrap();
        assert!(root.replies.is_empty());
        assert_eq!(root.sent(0x15), 1);
        assert_eq!(root.sent(0x1c), 0);
        assert!(seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        // Escape after the commit, before root's success.
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        seat.press(&[KEY_ESC], 60);
        assert!(!seat.attention_open());
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(root.replies.is_empty());
        assert!(seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
    }

    /// Only root's `06` for the unlock this client committed, every step of
    /// it admitted, leaves the lock surface. A forged one ends the paired
    /// generation still locked: as the first description, for a step
    /// before the commit, for another step or nonce than the committed
    /// one, at the PIN step before its PIN, and as the start of the
    /// lifetime after a failure.
    #[test]
    fn a_forged_or_out_of_order_06_never_unlocks() {
        let (identify, step) = (identify(), pin_step());
        let other = crate::authority::consent::Request::new(
            [8; 32],
            1000,
            crate::authority::consent::Operation::LoginUnlock {
                account: 1000,
                before: 2,
                after: 2,
                step: crate::authority::consent::LoginStep::Unlock {
                    key: [0xa2; 4],
                    retries: 8,
                },
            },
        )
        .unwrap();
        let mut at_pin = to_pin();
        at_pin.push(status(6, &step));
        let early: Vec<Vec<Vec<u8>>> = vec![
            vec![started(), status(6, &identify)],
            vec![
                started(),
                vec![0x91, 0x0b],
                status(3, &identify),
                status(4, &identify),
                vec![0x93],
                status(6, &identify),
            ],
            at_pin,
        ];
        for replies in early {
            let seat = LockedSeat::new(&[]);
            let attempt = seat.unlock(10);
            let mut root = root(replies);
            let mut client = crate::secret_client::Client::trusting_memory();
            client.start(&mut root, attempt).unwrap();
            assert!(run(&mut client, &mut root).is_err());
            assert!(seat.locked() && seat.attention_open());
            assert_eq!(root.sent(0x14), 0);
        }
        for forged in [status(6, &identify), status(6, &other)] {
            let seat = LockedSeat::new(&[]);
            let (mut client, mut root) = seat.committed();
            root.replies.push_back(forged);
            assert!(run(&mut client, &mut root).is_err());
            assert!(seat.locked() && seat.attention_open());
            assert!(!seat.client_shown());
        }
        // After a failure the next lifetime starts afresh: its first
        // status cannot be a success.
        let seat = LockedSeat::new(&[]);
        let attempt = seat.unlock(10);
        let mut root = root(vec![started(), vec![0x91, 0x0d, 0x09, 0]]);
        let mut client = crate::secret_client::Client::trusting_memory();
        client.start(&mut root, attempt).unwrap();
        run(&mut client, &mut root).unwrap();
        seat.press(&[KEY_ESC], 30);
        let attempt = seat.unlock(40);
        root.replies.extend([started(), status(6, &step)]);
        client.start(&mut root, attempt).unwrap();
        assert!(run(&mut client, &mut root).is_err());
        assert!(seat.locked() && seat.attention_open());
    }

    /// A security key's own keyboard opens no unlock on the paired seat:
    /// nothing reaches the worker and the lock surface stays.
    #[test]
    fn a_security_keys_chord_reaches_no_worker() {
        const KEY: usize = 1;
        let seat = LockedSeat::new(&[KEY]);
        seat.read(KEY, 10, chord_reports(11));
        assert!(seat.queued.attempt().is_none());
        assert!(!seat.attention_open() && seat.locked());
        assert!(seat.glass() == seat.lock_surface());
    }

    /// Root's `9a` for `state`, with the names `login_answer` gives.
    fn root_answer(state: &[u8]) -> Vec<u8> {
        [&[0x9a][..], state, b"\x06tester\x09td-laptop\x00"].concat()
    }

    /// TOKEN-LOGIN.md's D13: on a lock surface whose state is unavailable
    /// the chord opens attention on the cause's rows and sends no `1b`,
    /// for every cause; the lifetime selects nothing more, and Escape
    /// returns to the lock surface. A state that could not be read
    /// resolves through the worker's polling, and once it reads enrolled
    /// the rows and the chord's unlock come back.
    #[test]
    fn an_unavailable_lock_surface_shows_its_cause_and_sends_nothing() {
        use crate::secret_client::{LoginSelection, Selection};
        for cause in [0x0a, 0x0b, 0x0c] {
            let seat = LockedSeat::new(&[]);
            seat.login.answer(&root_answer(&[2, cause])).unwrap();
            let rows = crate::secret_client::login_failure(cause, 0).unwrap();
            // The rows follow the answer on glass.
            assert_eq!(
                crate::attention::lock_rows(seat.login.current().as_ref())[3..],
                *rows
            );
            assert!(seat.glass() == seat.lock_surface());
            seat.chord(10);
            assert!(seat.queued.attempt().is_none());
            assert!(seat.locked() && seat.attention_open());
            assert_eq!(seat.shown(), Some(crate::attention::Notice::Login(rows)));
            // Nothing else is a choice in this lifetime.
            seat.press(&[KEY_K, KEY_1, KEY_U, KEY_I, KEY_ENTER], 20);
            assert!(seat.queued.attempt().is_none());
            assert_eq!(seat.shown(), Some(crate::attention::Notice::Login(rows)));
            seat.press(&[KEY_ESC], 30);
            assert!(!seat.attention_open() && seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            // Resolved to enrolled: the unlock is offered again.
            seat.login
                .answer(&root_answer(&[1, 1, 7, 7, 7, 7]))
                .unwrap();
            assert!(seat.glass() == seat.lock_surface());
            assert_eq!(
                crate::attention::lock_rows(seat.login.current().as_ref()),
                [
                    "TD-LAPTOP",
                    "TESTER",
                    "LOCKED",
                    "PRESS CTRL+ALT+ESC TO UNLOCK"
                ]
            );
            let attempt = seat.unlock(40);
            assert_eq!(
                attempt.selection(),
                &Selection::Login(LoginSelection::Unlock)
            );
        }
    }

    /// TOKEN-LOGIN.md's D14: a locked session whose state becomes
    /// unenrolled stays locked for the rest of its generation. Its rows
    /// say `NO LOGIN KEYS ENROLLED`, and so does the chord, which sends no
    /// `1b`, so nothing on the lock surface can unlock it.
    #[test]
    fn a_lock_surface_whose_keys_are_gone_stays_locked() {
        let seat = LockedSeat::new(&[]);
        seat.login.answer(&root_answer(&[0])).unwrap();
        assert!(seat.locked());
        assert_eq!(
            crate::attention::lock_rows(seat.login.current().as_ref()),
            ["TD-LAPTOP", "TESTER", "LOCKED", "NO LOGIN KEYS ENROLLED"]
        );
        assert!(seat.glass() == seat.lock_surface());
        assert!(!seat.client_shown());
        for from in [10, 40] {
            seat.chord(from);
            assert!(seat.queued.attempt().is_none());
            assert!(seat.locked() && seat.attention_open());
            assert_eq!(
                seat.shown(),
                Some(crate::attention::Notice::Login(&["NO LOGIN KEYS ENROLLED"]))
            );
            seat.press(&[KEY_ESC], from + 10);
            assert!(!seat.attention_open() && seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        }
    }

    /// The rows follow each answer, but an answer never repaints over an
    /// open attention screen, never locks and never unlocks: the lock
    /// surface shows the new rows once attention closes.
    #[test]
    fn the_lock_rows_follow_the_answer_beneath_attention() {
        let seat = LockedSeat::new(&[]);
        let _attempt = seat.unlock(10);
        let open = seat.glass();
        seat.login.answer(&root_answer(&[2, 0x0b])).unwrap();
        assert!(seat.glass() == open);
        assert!(seat.locked() && seat.attention_open());
        seat.press(&[KEY_ESC], 30);
        assert!(seat.glass() == seat.lock_surface());
        // Unlocked, an answer that would lock changes nothing on glass.
        let seat = LockedSeat::open(&[]);
        let before = seat.glass();
        for state in [&[2, 0x0a][..], &[1, 1, 7, 7, 7, 7], &[0]] {
            seat.login.answer(&root_answer(state)).unwrap();
            assert!(!seat.locked());
            assert!(seat.glass() == before);
        }
    }

    /// With no `1a` answer, which the paired profile never has once
    /// connected, the lock surface's chord sends nothing either.
    #[test]
    fn a_lock_surface_with_no_answer_sends_nothing() {
        let target = locked_target();
        let bindings = attention_bindings(None);
        read_reports(&target, &bindings, 0, &chord_reports(10));
        let target = target.lock().unwrap();
        assert_eq!(target.attention_events, [true]);
        assert!(target.secret_roles.is_empty());
        assert_eq!(target.notices, [crate::attention::Notice::NotAvailable]);
    }

    // `Super+l` and the attention menu's `L` (TOKEN-LOGIN.md increment 4's
    // C9).

    /// Root's `1a` states, each with whether it locks: enrolled and every
    /// unavailable cause do, unenrolled does not.
    const STATES: &[(&[u8], bool)] = &[
        (&[1, 1, 7, 7, 7, 7], true),
        (&[2, 0x0a], true),
        (&[2, 0x0b], true),
        (&[2, 0x0c], true),
        (&[0], false),
    ];

    /// In the paired profile `Super+l`, from either Super, is always
    /// consumed, press and release, and asks to lock exactly while root's
    /// last answer is enrolled or unavailable; with no answer it does
    /// nothing either. A bare `l` is still the client's.
    #[test]
    fn super_l_is_consumed_and_locks_only_on_a_locking_state() {
        let answers = STATES
            .iter()
            .map(|(state, locks)| (Some(*state), *locks))
            .chain([(None, false)]);
        for (state, locks) in answers {
            let login = crate::authority::Login::default();
            if let Some(state) = state {
                login.answer(&root_answer(state)).unwrap();
            }
            let mut bindings = KeyBindings {
                attention_enabled: true,
                login,
                ..KeyBindings::default()
            };
            let bare = bindings.feed(key(KEY_L, KEY_PRESS));
            assert!(!bare.lock && bare.forward.is_some());
            assert!(bindings.feed(key(KEY_L, KEY_RELEASE)).forward.is_some());
            for meta in [KEY_LEFTMETA, KEY_RIGHTMETA] {
                bindings.feed(key(meta, KEY_PRESS));
                let press = bindings.feed(key(KEY_L, KEY_PRESS));
                assert_eq!(press.lock, locks, "{state:?}");
                assert!(press.forward.is_none());
                assert!(press.command.is_none() && press.launch.is_none());
                assert!(press.launcher.is_none() && press.help.is_none());
                assert!(press.notice.is_none() && press.attention.is_none());
                let release = bindings.feed(key(KEY_L, KEY_RELEASE));
                assert!(!release.lock && release.forward.is_none());
                bindings.feed(key(meta, KEY_RELEASE));
            }
        }
    }

    /// TOKEN-LOGIN.md's D10: the direct development profile leaves
    /// `Super+l` as it was, the client's, whatever an answer says.
    #[test]
    fn super_l_is_the_clients_in_the_direct_profile() {
        let mut bindings = KeyBindings {
            login: login_answer(&ENROLLED),
            ..KeyBindings::default()
        };
        bindings.feed(key(KEY_LEFTMETA, KEY_PRESS));
        let press = bindings.feed(key(KEY_L, KEY_PRESS));
        assert!(!press.lock);
        assert_eq!(press.forward, Some(key(KEY_L, KEY_PRESS).key_input()));
        let release = bindings.feed(key(KEY_L, KEY_RELEASE));
        assert_eq!(release.forward, Some(key(KEY_L, KEY_RELEASE).key_input()));
        // Through the live seat: nothing locks, and the window has the key.
        let seat = LockedSeat::open(&[]);
        seat.runtime.lock().unwrap().enable_attention(false);
        seat.bindings.lock().unwrap().attention_enabled = false;
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        seat.super_l(10);
        assert!(!seat.locked());
        assert!(seat.client_shown());
        assert!(LockedSeat::client_keys(&events).contains(&u32::from(KEY_L)));
    }

    /// `Super+l` comes before the sheet's and the launcher's capture: with
    /// either up it is not theirs. Locking, it closes both; unenrolled it
    /// does nothing, so the overlay stays. In the direct profile the
    /// overlay's capture keeps it, as before.
    #[test]
    fn super_l_is_read_before_the_overlays_capture() {
        for (help, launcher) in [(true, false), (false, true)] {
            for (state, locks) in STATES {
                let login = crate::authority::Login::default();
                login.answer(&root_answer(state)).unwrap();
                let mut bindings = KeyBindings {
                    attention_enabled: true,
                    login,
                    help_open: help,
                    launcher_open: launcher,
                    ..KeyBindings::default()
                };
                bindings.feed(key(KEY_LEFTMETA, KEY_PRESS));
                let press = bindings.feed(key(KEY_L, KEY_PRESS));
                assert_eq!(press.lock, *locks);
                assert!(press.help.is_none() && press.launcher.is_none());
                assert!(press.forward.is_none());
            }
            let mut direct = KeyBindings {
                help_open: help,
                launcher_open: launcher,
                ..KeyBindings::default()
            };
            direct.feed(key(KEY_LEFTMETA, KEY_PRESS));
            let press = direct.feed(key(KEY_L, KEY_PRESS));
            assert!(!press.lock && press.forward.is_none());
            assert_eq!(press.help, help.then_some(HelpAction::Close));
        }
        // Through the live seat, the overlays opened by their own chords.
        for (state, locks) in STATES {
            for opener in [KEY_SLASH, KEY_ENTER] {
                let seat = LockedSeat::open(&[]);
                seat.login.answer(&root_answer(state)).unwrap();
                let open = [
                    (KEY_LEFTMETA, KEY_PRESS),
                    (opener, KEY_PRESS),
                    (opener, KEY_RELEASE),
                    (KEY_LEFTMETA, KEY_RELEASE),
                ]
                .into_iter()
                .zip(11..)
                .flat_map(|((code, value), time)| [at_millis(key(code, value), time), syn(time)])
                .collect();
                seat.read(0, 10, open);
                let overlay = |seat: &LockedSeat| {
                    let runtime = seat.runtime.lock().unwrap();
                    runtime.help_visible() || runtime.launcher_visible()
                };
                assert!(overlay(&seat), "{opener}");
                seat.super_l(20);
                assert_eq!(seat.locked(), *locks);
                assert_eq!(overlay(&seat), !locks);
                let bindings = seat.bindings.lock().unwrap();
                assert_eq!(bindings.help_open || bindings.launcher_open, !locks);
                drop(bindings);
                if *locks {
                    assert!(seat.glass() == seat.lock_surface());
                }
            }
        }
    }

    /// Through the whole device dispatcher, `Super+l` locks an enrolled or
    /// unavailable session: the lock surface on glass, no window shown or
    /// focused. Unenrolled it does nothing. Neither way does the window get
    /// the `l`.
    #[test]
    fn super_l_locks_a_session_through_the_dispatcher() {
        for (state, locks) in STATES {
            let seat = LockedSeat::open(&[]);
            seat.login.answer(&root_answer(state)).unwrap();
            let (events, _stop) = seat
                .runtime
                .lock()
                .unwrap()
                .subscribe_keyboard(1)
                .unwrap()
                .split();
            seat.super_l(10);
            assert_eq!(seat.locked(), *locks, "{state:?}");
            assert!(!seat.attention_open());
            assert!(!LockedSeat::client_keys(&events).contains(&u32::from(KEY_L)));
            let focus = seat.runtime.lock().unwrap().keyboard_snapshot().focus;
            if *locks {
                assert!(seat.glass() == seat.lock_surface());
                assert!(!seat.client_shown());
                assert_eq!(focus, None);
                // Again on the lock surface: consumed, still locked.
                seat.super_l(20);
                assert!(seat.locked() && !seat.attention_open());
                assert!(seat.glass() == seat.lock_surface());
            } else {
                assert!(seat.client_shown());
                assert_eq!(focus, Some(seat.surface));
            }
        }
    }

    /// The attention menu's `L` is the lifetime's one selection. Enrolled
    /// or unavailable, it ends attention into the lock and asks root for
    /// nothing; unenrolled it shows NO LOGIN KEYS ENROLLED, asks nothing,
    /// selects nothing more, and Escape returns to the session.
    #[test]
    fn the_menus_l_locks_or_says_why_not() {
        use crate::attention::Notice;
        for (state, locks) in STATES {
            let seat = LockedSeat::open(&[]);
            seat.login.answer(&root_answer(state)).unwrap();
            seat.chord(10);
            assert_eq!(seat.shown(), Some(Notice::default()));
            seat.press(&[KEY_L], 20);
            assert!(seat.queued.attempt().is_none());
            if *locks {
                assert!(seat.locked() && !seat.attention_open(), "{state:?}");
                assert!(seat.glass() == seat.lock_surface());
                assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
                continue;
            }
            let why = Some(Notice::Login(&["NO LOGIN KEYS ENROLLED"]));
            assert!(!seat.locked() && seat.attention_open());
            assert_eq!(seat.shown(), why);
            seat.press(&[KEY_U, KEY_K, KEY_L, KEY_I], 30);
            assert!(seat.queued.attempt().is_none());
            assert_eq!(seat.shown(), why);
            seat.press(&[KEY_ESC], 40);
            assert!(!seat.attention_open() && !seat.locked());
            assert!(seat.client_shown());
        }
    }

    /// `L` is the menu's alone: not on the key-management screen, not once
    /// the lifetime chose its operation, not in the lock surface's unlock,
    /// and never from a security key's own keyboard.
    #[test]
    fn l_selects_only_on_the_menu() {
        use crate::attention::Notice;
        // The key-management screen.
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.press(&[KEY_K, KEY_L], 20);
        assert!(!seat.locked() && seat.attention_open());
        assert_eq!(seat.shown(), Some(Notice::LoginKeys));
        // An operation already chosen.
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.press(&[KEY_U], 20);
        let attempt = seat.queued.attempt().unwrap();
        seat.press(&[KEY_L], 30);
        assert!(!seat.locked() && seat.attention_open());
        assert!(seat.queued.attempt().is_none());
        drop(attempt);
        // The lock surface's unlock: its lifetime stays open.
        let seat = LockedSeat::new(&[]);
        let _attempt = seat.unlock(10);
        seat.press(&[KEY_L], 30);
        assert!(seat.locked() && seat.attention_open());
        assert_eq!(seat.shown(), Some(Notice::Pending));
        // A security key's own keyboard.
        const KEY: usize = 1;
        let seat = LockedSeat::open(&[KEY]);
        seat.chord(10);
        seat.read(KEY, 20, presses(&[KEY_L], 21));
        assert!(!seat.locked() && seat.attention_open());
        assert_eq!(seat.shown(), Some(Notice::default()));
    }

    /// While attention is up `Super+l` is the attention screen's: on the
    /// menu it is read as the menu's `L`, under the menu's rules, and once
    /// an operation is chosen it locks nothing.
    #[test]
    fn attention_reads_super_l_under_its_own_rules() {
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.press(&[KEY_U], 20);
        let _attempt = seat.queued.attempt().unwrap();
        seat.super_l(30);
        assert!(!seat.locked() && seat.attention_open());
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.super_l(20);
        assert!(seat.locked() && !seat.attention_open());
        assert!(seat.glass() == seat.lock_surface());
    }

    /// TOKEN-LOGIN.md's D12: a lock while a lifetime is open ends it first,
    /// as Escape does. Before the commit the operation is cancelled; after
    /// it the screen drains and root's result is never shown; in a login
    /// unlock's lifetime even a committed unlock leaves the session locked.
    /// Held input drains under Escape's screen before the lock surface.
    #[test]
    fn a_lock_during_an_open_lifetime_ends_it_as_escape_does() {
        // Before the commit.
        let seat = LockedSeat::new(&[]);
        let attempt = seat.unlock(10);
        let mut root = root(to_pin());
        let mut client = crate::secret_client::Client::trusting_memory();
        client.start(&mut root, attempt).unwrap();
        run(&mut client, &mut root).unwrap();
        assert_eq!(seat.field(), Some(crate::attention::Field::Pin(0)));
        seat.lock().unwrap();
        assert!(!seat.attention_open() && seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        root.replies.extend([
            vec![0x95, 0],
            [&[0x91, 0x0d, 0x80, 0][..], &pin_step().encode()].concat(),
        ]);
        run(&mut client, &mut root).unwrap();
        assert!(root.replies.is_empty());
        assert_eq!(root.sent(0x15), 1);
        assert_eq!(root.sent(0x1c), 0);
        assert!(seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        // After the commit, before root's success, with a key held.
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        let mut pointer = PointerMotion::default();
        let feed = |pointer: &mut PointerMotion, value, time| {
            for event in later(vec![at_millis(key(KEY_A, value), time), syn(time)]) {
                apply(
                    seat.target.as_ref(),
                    event,
                    0,
                    seat.bindings.as_ref(),
                    pointer,
                    None,
                )
                .unwrap();
            }
        };
        feed(&mut pointer, KEY_PRESS, 61);
        seat.lock().unwrap();
        assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(root.replies.is_empty());
        assert!(seat.locked());
        // Escape's drain, not the result.
        let mut drained = vec![0; 800 * 600 * 4];
        crate::attention::paint(
            &mut drained,
            800,
            600,
            3200,
            true,
            false,
            crate::attention::Notice::Login(&["SESSION UNLOCKED"]),
        );
        assert!(seat.glass() == drained);
        feed(&mut pointer, KEY_RELEASE, 71);
        assert!(!seat.attention_open() && seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
    }

    /// The adapter's entry ends an open lifetime through the target's drain,
    /// Escape's, which cancels whatever operation it chose, then locks and
    /// closes attention once nothing is held. The direct profile has no
    /// entry: it refuses and touches nothing.
    #[test]
    fn the_lock_entry_drains_an_open_lifetime_and_needs_the_paired_profile() {
        let mut target = RecordingTarget::default();
        let mut bindings = KeyBindings {
            attention_enabled: true,
            attention: AttentionState::Open,
            secret_selected: true,
            launcher_open: true,
            ..KeyBindings::default()
        };
        lock_session(&mut target, &mut bindings).unwrap();
        assert_eq!(target.draining_events, 1);
        assert_eq!(target.lock_screens, 1);
        assert_eq!(target.attention_events, [false]);
        assert!(bindings.attention == AttentionState::Closed);
        assert!(!bindings.launcher_open && !bindings.help_open);
        let mut target = RecordingTarget::default();
        let mut direct = KeyBindings {
            attention: AttentionState::Open,
            ..KeyBindings::default()
        };
        assert!(lock_session(&mut target, &mut direct).is_err());
        assert_eq!((target.draining_events, target.lock_screens), (0, 0));
        assert!(direct.attention == AttentionState::Open);
        // A target with no lock surface refuses.
        let (_cleanup, mut runtime) = automation_runtime();
        let mut automation = AutomationTarget {
            runtime: &mut runtime,
        };
        assert!(automation.lock_screen().is_err());
    }

    /// The paired profile's help sheet lists `Super+l`; the direct one's
    /// does not.
    #[test]
    fn the_paired_sheet_lists_super_l() {
        let seat = LockedSeat::open(&[]);
        let sheet = |paired: bool| {
            let mut runtime = seat.runtime.lock().unwrap();
            runtime.enable_attention(paired);
            assert!(runtime.help(HelpAction::Toggle).unwrap());
            drop(runtime);
            let glass = seat.glass();
            assert!(!seat
                .runtime
                .lock()
                .unwrap()
                .help(HelpAction::Close)
                .unwrap());
            glass
        };
        let paired = sheet(true);
        assert!(paired != sheet(false));
        assert!(paired == sheet(true));
    }

    /// A lock never depends on the drain's paint: when the paint that
    /// drains the open lifetime fails, the menu's `L` still locks, and the
    /// reader's cleanup, which closes attention, returns to the lock
    /// surface with no client focused, never to the desktop.
    #[test]
    fn a_failed_drain_paint_still_locks_for_the_menus_l() {
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.runtime.lock().unwrap().fail_next_repaint();
        assert!(seat.try_read(0, 20, presses(&[KEY_L], 21)).is_err());
        assert!(seat.locked());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        assert!(!seat.attention_open());
        assert!(!seat.client_shown());
        assert!(seat.queued.attempt().is_none());
    }

    /// The same through the entry with an operation open, as C10's lid and
    /// resume will reach it: the drain's paint fails, the session locks all
    /// the same, and closing attention shows the lock surface.
    #[test]
    fn a_failed_drain_paint_still_locks_an_open_lifetime() {
        let seat = LockedSeat::open(&[]);
        seat.chord(10);
        seat.press(&[KEY_U], 20);
        let _attempt = seat.queued.attempt().unwrap();
        seat.runtime.lock().unwrap().fail_next_repaint();
        assert!(seat.lock().is_err());
        assert!(seat.locked());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
        // The next report closes the drained screen.
        seat.press(&[KEY_A], 30);
        assert!(!seat.attention_open() && seat.locked());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        assert!(seat.glass() == seat.lock_surface());
    }

    /// A lock that comes after root's success left the lock surface, while
    /// held input still drains under `SESSION UNLOCKED`, takes that notice
    /// down: the drain shows Escape's screen, and the release shows the
    /// lock surface, not the desktop.
    #[test]
    fn a_relock_during_an_unlocks_drain_hides_its_success() {
        let seat = LockedSeat::new(&[]);
        let (mut client, mut root) = seat.committed();
        let mut pointer = PointerMotion::default();
        let feed = |pointer: &mut PointerMotion, value, time| {
            for event in later(vec![at_millis(key(KEY_A, value), time), syn(time)]) {
                apply(
                    seat.target.as_ref(),
                    event,
                    0,
                    seat.bindings.as_ref(),
                    pointer,
                    None,
                )
                .unwrap();
            }
        };
        feed(&mut pointer, KEY_PRESS, 61);
        root.replies.push_back(status(6, &pin_step()));
        run(&mut client, &mut root).unwrap();
        assert!(!seat.locked());
        seat.lock().unwrap();
        assert!(seat.locked());
        let mut drained = vec![0; 800 * 600 * 4];
        crate::attention::paint(
            &mut drained,
            800,
            600,
            3200,
            true,
            false,
            crate::attention::Notice::Login(&["SESSION UNLOCKED"]),
        );
        assert!(seat.glass() == drained);
        feed(&mut pointer, KEY_RELEASE, 71);
        assert!(!seat.attention_open() && seat.locked());
        assert!(seat.glass() == seat.lock_surface());
        assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
    }

    // A lid close and a resume (TOKEN-LOGIN.md increment 4's C10).

    fn switch(code: u16, value: i32) -> Event {
        Event {
            timestamp: 0,
            time: 0,
            kind: EV_SW,
            code,
            value,
        }
    }

    /// `SW_TABLET_MODE`, a switch that is not the lid.
    const SW_TABLET_MODE: u16 = 1;

    /// What `TestClocks` reads: the two clocks, how many boot-time reads
    /// remain to straddle a stall, whether those reads fail, and how many
    /// were made.
    #[derive(Default)]
    struct Time {
        monotonic: Duration,
        boot: Duration,
        stalls: usize,
        failing: bool,
        reads: usize,
    }

    #[derive(Clone, Default)]
    struct TestClocks(Arc<Mutex<Time>>);

    impl TestClocks {
        /// Time awake: both clocks advance.
        fn advance(&self, by: Duration) {
            let mut time = self.0.lock().unwrap();
            time.monotonic += by;
            time.boot += by;
        }

        /// A suspend: only the boot-time clock advances.
        fn suspend(&self, by: Duration) {
            self.0.lock().unwrap().boot += by;
        }

        /// The next `count` samples are preempted for 150 ms between their
        /// two monotonic reads.
        fn stall(&self, count: usize) {
            self.0.lock().unwrap().stalls = count;
        }

        fn fail(&self, failing: bool) {
            self.0.lock().unwrap().failing = failing;
        }

        fn reads(&self) -> usize {
            self.0.lock().unwrap().reads
        }
    }

    impl ResumeClocks for TestClocks {
        fn monotonic(&mut self) -> Duration {
            self.0.lock().unwrap().monotonic
        }

        fn boot(&mut self) -> Result<Duration, String> {
            let mut time = self.0.lock().unwrap();
            time.reads += 1;
            if time.failing {
                return Err("unreadable".into());
            }
            if time.stalls > 0 {
                time.stalls -= 1;
                time.monotonic += Duration::from_millis(150);
                time.boot += Duration::from_millis(150);
            }
            Ok(time.boot)
        }
    }

    fn test_resume(login: &crate::authority::Login) -> (Arc<Resume>, TestClocks) {
        let clocks = TestClocks::default();
        let resume = Arc::new(Resume::new(login.clone(), Box::new(clocks.clone())));
        (resume, clocks)
    }

    /// The lock a gate made, as the gate itself records it.
    fn made(resume: &Resume) {
        resume.watch().owed = false;
    }

    /// The two triggers, each through its own production entry: a lid
    /// switch's close read by `read_lid`, and a resume after a three-second
    /// suspend found by `Resume::gate`.
    #[derive(Clone, Copy, Debug)]
    enum Trigger {
        Lid,
        Resume,
    }

    const TRIGGERS: &[Trigger] = &[Trigger::Lid, Trigger::Resume];

    impl LockedSeat {
        /// `events` read from a lid switch by its reader.
        fn lid(&self, events: Vec<Event>) -> Result<(), String> {
            let data = events.into_iter().flat_map(encode).collect();
            read_lid(
                Path::new("event-lid"),
                &mut ChunkedReader::new(data, Vec::new()),
                self.target.as_ref(),
                self.bindings.as_ref(),
            )
        }

        /// Resume detection as the paired profile's start installs it: the
        /// bindings check it before each batch, and the runtime before each
        /// repaint that could show a client.
        fn resume(&self) -> (Arc<Resume>, TestClocks) {
            let (resume, clocks) = test_resume(&self.login);
            self.bindings.lock().unwrap().resume = Some(Arc::clone(&resume));
            self.runtime
                .lock()
                .unwrap()
                .hold_for_resume(Arc::clone(&resume));
            (resume, clocks)
        }

        fn trigger(&self, trigger: Trigger) -> Result<(), String> {
            match trigger {
                Trigger::Lid => self.lid(vec![switch(SW_LID, LID_CLOSED), syn(0)]),
                Trigger::Resume => {
                    let (resume, clocks) = test_resume(&self.login);
                    assert!(!resume.check());
                    clocks.advance(Duration::from_secs(1));
                    clocks.suspend(Duration::from_secs(3));
                    resume.gate(self.target.as_ref(), self.bindings.as_ref())
                }
            }
        }
    }

    /// A lid switch is a switch-only node, `EV_SYN` and `EV_SW` in
    /// `capabilities/ev`, declaring `SW_LID`. A node with keys beside its
    /// switches, a switch-only node without the lid, and anything
    /// unreadable is not; nor is a keyboard.
    #[test]
    fn a_lid_switch_is_a_switch_only_node_declaring_sw_lid() {
        struct Tree(PathBuf);
        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let tree = Tree(std::env::temp_dir().join(format!(
            "td-input-lid-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        )));
        let entry = |name: &str, types: Option<&str>, switches: Option<&str>| {
            let capabilities = tree.0.join(name).join("device").join("capabilities");
            std::fs::create_dir_all(&capabilities).unwrap();
            for (file, text) in [("ev", types), ("sw", switches)] {
                match text {
                    Some(text) => std::fs::write(capabilities.join(file), text).unwrap(),
                    None => std::fs::create_dir(capabilities.join(file)).unwrap(),
                }
            }
        };
        // The ACPI lid: EV_SYN and EV_SW, SW_LID.
        entry("event0", Some("21\n"), Some("1\n"));
        // EV_SW alone, without EV_SYN's bit.
        entry("event1", Some("20\n"), Some("1\n"));
        // A lid beside keys (EV_KEY), as some embedded controllers report.
        entry("event2", Some("23\n"), Some("1\n"));
        // Switch-only, without the lid: tablet mode, a headphone jack.
        entry("event3", Some("21\n"), Some("2\n"));
        entry("event4", Some("21\n"), Some("4\n"));
        // A keyboard, whose `sw` is empty.
        entry("event5", Some("120013\n"), Some("0\n"));
        // Unreadable halves.
        entry("event6", None, Some("1\n"));
        entry("event7", Some("21\n"), None);
        entry("event8", Some("2x\n"), Some("1\n"));
        // A lid with EV_REL or EV_ABS is no switch-only node.
        entry("event9", Some("25\n"), Some("1\n"));
        entry("event10", Some("29\n"), Some("1\n"));
        let lid = |node: &str| lid_switch(&tree.0, Path::new(node));
        assert!(lid("/dev/input/event0"));
        assert!(lid("/dev/input/event1"));
        for node in 2..=10 {
            assert!(!lid(&format!("/dev/input/event{node}")), "event{node}");
        }
        assert!(!lid("/dev/input/event11"));
        assert!(!lid("/"));
        // The roster: the lid is read apart where admitted, and opened by
        // no ordinary reader either way.
        let paths: Vec<PathBuf> = (0..=11)
            .map(|node| PathBuf::from(format!("/dev/input/event{node}")))
            .collect();
        let lids = [paths[0].clone(), paths[1].clone()];
        let ordinary: Vec<PathBuf> = paths[2..].to_vec();
        assert_eq!(
            admit_lids(paths.clone(), &tree.0, true),
            (ordinary.clone(), lids.to_vec())
        );
        assert_eq!(admit_lids(paths, &tree.0, false), (ordinary, Vec::new()));
    }

    /// A lid switch is admitted in the paired profile alone, on root's
    /// answer at connect: enrolled or unavailable admits it, unenrolled and
    /// no answer do not, and the direct profile never does.
    #[test]
    fn a_lid_switch_is_admitted_on_a_locking_answer_at_connect() {
        for (state, locks) in STATES {
            let login = crate::authority::Login::default();
            login.answer(&root_answer(state)).unwrap();
            let connected = login.current();
            assert_eq!(lid_admitted(true, connected.as_ref()), *locks, "{state:?}");
            assert!(!lid_admitted(false, connected.as_ref()));
        }
        assert!(!lid_admitted(true, None));
    }

    /// A lid close locks exactly while root's last answer is enrolled or
    /// unavailable, through the whole live entry: the lock surface on
    /// glass and no window focused. Unenrolled, it does nothing; nor does
    /// opening the lid or another switch closing. The direct profile has
    /// no lock, and a close there does nothing either.
    #[test]
    fn a_lid_close_locks_only_on_a_locking_state() {
        for (state, locks) in STATES {
            let seat = LockedSeat::open(&[]);
            seat.login.answer(&root_answer(state)).unwrap();
            seat.lid(vec![
                switch(SW_LID, 0),
                syn(1),
                switch(SW_TABLET_MODE, LID_CLOSED),
                syn(2),
            ])
            .unwrap();
            assert!(!seat.locked() && seat.client_shown());
            seat.lid(vec![switch(SW_LID, LID_CLOSED), syn(3)]).unwrap();
            assert_eq!(seat.locked(), *locks, "{state:?}");
            assert!(!seat.attention_open());
            let focus = seat.runtime.lock().unwrap().keyboard_snapshot().focus;
            if *locks {
                assert!(seat.glass() == seat.lock_surface());
                assert_eq!(focus, None);
                // Closed again on the lock surface: still locked.
                seat.lid(vec![switch(SW_LID, LID_CLOSED), syn(4)]).unwrap();
                assert!(seat.locked() && seat.glass() == seat.lock_surface());
            } else {
                assert!(seat.client_shown());
                assert_eq!(focus, Some(seat.surface));
            }
        }
        let seat = LockedSeat::open(&[]);
        seat.runtime.lock().unwrap().enable_attention(false);
        seat.bindings.lock().unwrap().attention_enabled = false;
        seat.lid(vec![switch(SW_LID, LID_CLOSED), syn(1)]).unwrap();
        assert!(!seat.locked() && seat.client_shown());
    }

    /// Nothing a lid switch reports reaches the bindings, the pointer or a
    /// client: a key, a button, motion and the switch itself are read by
    /// the lid's reader alone, which acts only on a close.
    #[test]
    fn a_lid_switch_reaches_no_client() {
        let seat = LockedSeat::open(&[]);
        seat.login.answer(&root_answer(&[0])).unwrap();
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        let pointer = seat.runtime.lock().unwrap().pointer_snapshot();
        seat.lid(vec![
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_A, KEY_PRESS),
            syn(1),
            key(KEY_A, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
            syn(2),
            key(BTN_LEFT, KEY_PRESS),
            Event {
                kind: EV_REL,
                code: REL_X,
                value: 40,
                ..key(0, 0)
            },
            syn(3),
            switch(SW_LID, LID_CLOSED),
            syn(4),
            switch(SW_LID, 0),
            syn(5),
        ])
        .unwrap();
        assert!(LockedSeat::client_keys(&events).is_empty());
        assert_eq!(seat.runtime.lock().unwrap().pointer_snapshot(), pointer);
        let bindings = seat.bindings.lock().unwrap();
        assert!(bindings.pressed.is_empty() && bindings.consumed.is_empty());
        assert!(bindings.forwarded.is_empty() && bindings.pointer_pressed.is_empty());
        assert!(bindings.attention == AttentionState::Closed);
        drop(bindings);
        assert!(!seat.locked() && seat.client_shown());
    }

    /// `/proc/uptime`'s first field, in seconds; anything else is refused.
    #[test]
    fn uptime_reads_the_boot_time_field() {
        assert_eq!(
            uptime("350735.47 234388.90\n"),
            Some(Duration::from_millis(350_735_470))
        );
        assert_eq!(uptime("0.01 0.00\n"), Some(Duration::from_millis(10)));
        assert_eq!(
            uptime("1.123456789 0\n"),
            Some(Duration::new(1, 123_456_789))
        );
        for bad in [
            "",
            "\n",
            "12 3.00\n",
            ".5 1.00\n",
            "5. 1.00\n",
            "+5.00 1.00\n",
            "5.0000000001 1\n",
            "-1.00 1.00\n",
            "1.0x 1.00\n",
            "99999999999999999999.00 1.00\n",
        ] {
            assert_eq!(uptime(bad), None, "{bad:?}");
        }
        let long = format!("1.00 {}\n", "0".repeat(64));
        assert_eq!(uptime(&long), None);
        // The running kernel's own answers.
        let mut clocks = SystemClocks {
            origin: Instant::now(),
            uptime: None,
        };
        let first = clocks.boot().unwrap();
        // Kept open and read again from the start: the kernel regenerates
        // the line, so a later read is later.
        assert!(clocks.uptime.is_some());
        thread::sleep(Duration::from_millis(30));
        assert!(clocks.boot().unwrap() > first);
        assert!(clocks.uptime.is_some());
    }

    /// A gap of more than two seconds between how far the boot-time clock
    /// and the monotonic clock advanced since the last check is a suspend;
    /// two seconds or less, or time awake however long, is not.
    #[test]
    fn a_resume_gap_over_two_seconds_is_a_suspend() {
        let login = login_answer(&ENROLLED);
        for (millis, owed) in [
            (0, false),
            (1_500, false),
            (2_000, false),
            (2_001, true),
            (2_500, true),
            (3_600_000, true),
        ] {
            let (resume, clocks) = test_resume(&login);
            // The first check takes the baseline alone.
            clocks.suspend(Duration::from_secs(60));
            assert!(!resume.check());
            clocks.advance(Duration::from_secs(5));
            clocks.suspend(Duration::from_millis(millis));
            assert_eq!(resume.check(), owed, "{millis}");
            // Owed until the lock is made, whatever later checks find.
            clocks.advance(Duration::from_secs(1));
            assert_eq!(resume.check(), owed, "{millis}");
            made(&resume);
            assert!(!resume.check());
        }
        let (resume, clocks) = test_resume(&login);
        assert!(!resume.check());
        clocks.advance(Duration::from_secs(3_600));
        assert!(!resume.check());
        // Each check measures since the last: two short suspends apart.
        clocks.suspend(Duration::from_millis(1_500));
        assert!(!resume.check());
        clocks.suspend(Duration::from_millis(1_500));
        assert!(!resume.check());
    }

    /// A sample whose monotonic reads are more than 100 ms apart is
    /// discarded and retaken, and the retake is what is compared. Sixteen
    /// discards in a row, or an unreadable boot-time clock, are
    /// unverifiable: a suspend once, after which the baseline starts again
    /// from the next accepted sample.
    #[test]
    fn a_discarded_or_unreadable_sample_holds_until_verified() {
        let login = login_answer(&ENROLLED);
        let (resume, clocks) = test_resume(&login);
        assert!(!resume.check());
        clocks.stall(1);
        clocks.suspend(Duration::from_secs(3));
        assert!(resume.check());
        assert_eq!(clocks.reads(), 3);
        made(&resume);
        clocks.stall(SAMPLE_RETAKES - 1);
        assert!(!resume.check());
        clocks.stall(SAMPLE_RETAKES);
        assert!(resume.check());
        made(&resume);
        // No baseline now: unverifiable again is no new lock.
        clocks.stall(SAMPLE_RETAKES);
        assert!(!resume.check());
        // The next accepted sample is a baseline, then gaps count again.
        clocks.suspend(Duration::from_secs(3));
        assert!(!resume.check());
        clocks.suspend(Duration::from_secs(3));
        assert!(resume.check());
        made(&resume);
        // An unreadable boot-time clock: once, then not until it reads.
        clocks.fail(true);
        assert!(resume.check());
        made(&resume);
        assert!(!resume.check());
        clocks.fail(false);
        assert!(!resume.check());
        clocks.suspend(Duration::from_secs(3));
        assert!(resume.check());
    }

    /// Resume detection samples only while root's last answer is enrolled
    /// or unavailable: unenrolled, or with no answer, it reads no clock and
    /// owes nothing, and a suspend then is never counted later.
    #[test]
    fn resume_samples_only_on_a_locking_state() {
        for (state, locks) in STATES {
            let login = crate::authority::Login::default();
            login.answer(&root_answer(state)).unwrap();
            let (resume, clocks) = test_resume(&login);
            assert!(!resume.check());
            clocks.suspend(Duration::from_secs(3));
            assert_eq!(resume.check(), *locks, "{state:?}");
            assert_eq!(clocks.reads() > 0, *locks, "{state:?}");
        }
        let login = crate::authority::Login::default();
        let (resume, clocks) = test_resume(&login);
        clocks.suspend(Duration::from_secs(3));
        assert!(!resume.check() && !resume.check());
        assert_eq!(clocks.reads(), 0);
        // Turned enrolled, a fresh baseline: the earlier suspend is not
        // counted, a later one is.
        login.answer(&root_answer(STATES[0].0)).unwrap();
        assert!(!resume.check());
        clocks.suspend(Duration::from_secs(3));
        assert!(resume.check());
        // Turned unenrolled before the lock was made, nothing is owed.
        login.answer(&root_answer(&[0])).unwrap();
        assert!(!resume.check());
    }

    /// Before a batch is routed: after a suspend of more than two seconds
    /// an enrolled or unavailable session locks first, so the batch's key
    /// reaches the lock surface and not the window. A shorter suspend, or
    /// an unenrolled session, routes it as before.
    #[test]
    fn a_resume_locks_before_the_batch_is_routed() {
        for (millis, (state, locks)) in [(3_000, STATES[0]), (1_500, STATES[0])]
            .into_iter()
            .chain(STATES.iter().map(|state| (3_000, *state)))
        {
            let seat = LockedSeat::open(&[]);
            seat.login.answer(&root_answer(state)).unwrap();
            let (_resume, clocks) = seat.resume();
            let (events, _stop) = seat
                .runtime
                .lock()
                .unwrap()
                .subscribe_keyboard(1)
                .unwrap()
                .split();
            seat.press(&[KEY_A], 10);
            assert!(LockedSeat::client_keys(&events).contains(&u32::from(KEY_A)));
            clocks.advance(Duration::from_secs(1));
            clocks.suspend(Duration::from_millis(millis));
            seat.press(&[KEY_B], 20);
            let locked = locks && millis > 2_000;
            assert_eq!(seat.locked(), locked, "{state:?} {millis}");
            assert_eq!(
                LockedSeat::client_keys(&events).contains(&u32::from(KEY_B)),
                !locked,
                "{state:?} {millis}"
            );
            if locked {
                assert!(seat.glass() == seat.lock_surface());
                assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
            }
        }
    }

    /// Before a repaint that could show a client: after a suspend the
    /// window's new frame is held, the frame from before the suspend stays
    /// on glass, and the monitor is woken; its lock then paints the lock
    /// surface. Unenrolled, the frame is painted as before.
    #[test]
    fn a_resume_holds_a_clients_repaint_until_the_lock() {
        const NEW: [u8; 4] = [9, 8, 7, 0];
        for (state, locks) in STATES {
            let seat = LockedSeat::open(&[]);
            seat.login.answer(&root_answer(state)).unwrap();
            let (resume, clocks) = seat.resume();
            assert!(!resume.check());
            clocks.suspend(Duration::from_secs(3));
            seat.runtime
                .lock()
                .unwrap()
                .commit(
                    seat.surface,
                    crate::buffer::Surface::from_shm_pixels(
                        100,
                        100,
                        NEW.repeat(10_000),
                        crate::scene::SHM_XRGB8888,
                    )
                    .unwrap(),
                )
                .unwrap();
            let glass = seat.glass();
            assert_eq!(glass.as_chunks::<4>().0.contains(&NEW), !locks, "{state:?}");
            assert_eq!(seat.client_shown(), *locks, "{state:?}");
            assert_eq!(resume.watch().woken, *locks);
            resume
                .gate(seat.target.as_ref(), seat.bindings.as_ref())
                .unwrap();
            assert_eq!(seat.locked(), *locks);
            if *locks {
                assert!(seat.glass() == seat.lock_surface());
            }
        }
    }

    /// Polls `done` for up to five seconds, answering how long it took.
    fn waited(done: &dyn Fn() -> bool) -> Duration {
        let start = Instant::now();
        while !done() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        start.elapsed()
    }

    /// The monitor as start runs it, holding the seat for the generation.
    /// It never ends, so a test's monitor outlives its test, checking a
    /// test clock nothing advances.
    fn spawn_monitor(
        resume: &Arc<Resume>,
        target: Arc<Mutex<LiveInputTarget>>,
        bindings: Arc<Mutex<KeyBindings>>,
    ) {
        let resume = Arc::clone(resume);
        thread::spawn(move || resume.monitor(target.as_ref(), bindings.as_ref()));
    }

    /// The monitor checks at once and then at least once a second, so a
    /// resume with no input and no repaint still locks within a second; a
    /// repaint's wake ends its wait at once.
    #[test]
    fn the_monitor_checks_at_least_once_a_second() {
        assert!(RESUME_PERIOD <= Duration::from_secs(1));
        let seat = LockedSeat::open(&[]);
        let (resume, clocks) = seat.resume();
        spawn_monitor(
            &resume,
            Arc::clone(&seat.target),
            Arc::clone(&seat.bindings),
        );
        let first = waited(&|| clocks.reads() > 0);
        assert!(first < Duration::from_millis(750), "{first:?}");
        clocks.suspend(Duration::from_secs(3));
        let took = waited(&|| seat.locked());
        assert!(seat.locked());
        assert!(took <= Duration::from_secs(1), "{took:?}");
        assert!(seat.glass() == seat.lock_surface());
        // Woken, the wait ends at once.
        resume.watch().woken = true;
        let start = Instant::now();
        resume.wait();
        assert!(start.elapsed() < RESUME_PERIOD / 2);
        assert!(!resume.watch().woken);
    }

    /// With every reader gone, as a USB-only seat's may be at a resume, the
    /// monitor holds the seat itself: a client's repaint held for the owed
    /// lock wakes it, and the lock is made, never a repaint withheld for
    /// good.
    #[test]
    fn the_monitor_locks_with_every_reader_gone() {
        let seat = LockedSeat::open(&[]);
        let (resume, clocks) = seat.resume();
        let LockedSeat {
            cleanup,
            runtime,
            target,
            bindings,
            surface,
            login,
            ..
        } = seat;
        spawn_monitor(&resume, target, bindings);
        waited(&|| clocks.reads() > 0);
        clocks.suspend(Duration::from_secs(3));
        runtime
            .lock()
            .unwrap()
            .commit(
                surface,
                crate::buffer::Surface::from_shm_pixels(
                    100,
                    100,
                    [9, 8, 7, 0].repeat(10_000),
                    crate::scene::SHM_XRGB8888,
                )
                .unwrap(),
            )
            .unwrap();
        waited(&|| runtime.lock().unwrap().session_locked());
        assert!(runtime.lock().unwrap().session_locked());
        let rows = crate::attention::lock_rows(login.current().as_ref());
        let mut frame = vec![0; 800 * 600 * 4];
        crate::attention::paint_lock(&mut frame, 800, 600, 3200, &rows);
        assert!(std::fs::read(&cleanup.0).unwrap() == frame);
    }

    /// What `events` delivered since it subscribed, by kind: key and
    /// button transitions with their direction, and every other delivery
    /// by name.
    fn delivered(
        events: &std::sync::mpsc::Receiver<crate::runtime::KeyboardDelivery>,
    ) -> Vec<String> {
        use crate::keyboard::KeyboardEvent;
        use crate::pointer::PointerEvent;
        use crate::runtime::KeyboardDelivery;
        let mut seen = Vec::new();
        for delivery in events.try_iter() {
            match delivery {
                KeyboardDelivery::Event(event) => seen.push(match event.event {
                    KeyboardEvent::Key { input, .. } => {
                        format!("key {} {:?}", input.key, input.state)
                    }
                    KeyboardEvent::Enter { .. } => "keyboard enter".into(),
                    KeyboardEvent::Leave { .. } => "keyboard leave".into(),
                    KeyboardEvent::Modifiers { .. } => "modifiers".into(),
                }),
                KeyboardDelivery::Pointer(frame) => {
                    for event in frame.events {
                        seen.push(match event {
                            PointerEvent::Button { input, .. } => {
                                format!("button {} {:?}", input.button, input.state)
                            }
                            PointerEvent::Enter { .. } => "pointer enter".into(),
                            PointerEvent::Leave { .. } => "pointer leave".into(),
                            PointerEvent::Motion { .. } => "motion".into(),
                            PointerEvent::Axis { .. } => "axis".into(),
                        });
                    }
                }
                _ => seen.push("other".into()),
            }
        }
        seen
    }

    /// `client`'s keyboard and pointer deliveries, both active.
    fn subscribe_input(
        seat: &LockedSeat,
    ) -> impl FnMut(
        u64,
    ) -> (
        std::sync::mpsc::Receiver<crate::runtime::KeyboardDelivery>,
        crate::runtime::KeyboardSubscriptionStop,
    ) + '_ {
        move |client| {
            let active = || Arc::new(std::sync::atomic::AtomicBool::new(true));
            seat.runtime
                .lock()
                .unwrap()
                .subscribe_input_with_activity(client, active(), active())
                .unwrap()
                .split()
        }
    }

    /// Puts the pointer at (60, 90), over the seat's window, so a press is
    /// the window's.
    fn over_window(seat: &LockedSeat) {
        let at = |numerator, denominator| Fraction {
            numerator,
            denominator,
        };
        seat.runtime
            .lock()
            .unwrap()
            .pointer_frame_at(1, at(3, 40), at(3, 20), &[], PointerScroll::default())
            .unwrap();
    }

    /// A reader that hands out `data`, then suspends `clocks` for three
    /// seconds and fails as a device lost at the resume does.
    struct LostAtResume {
        data: Option<Vec<u8>>,
        clocks: TestClocks,
    }

    impl Read for LostAtResume {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.data.take() {
                Some(data) => {
                    buffer[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                None => {
                    self.clocks.suspend(Duration::from_secs(3));
                    Err(std::io::Error::from_raw_os_error(19))
                }
            }
        }
    }

    /// A device lost at a resume releases what it held only once the lock
    /// is made, so neither the held key's nor the held button's release
    /// reaches the window, and the session is locked.
    #[test]
    fn a_device_lost_at_a_resume_releases_nothing_before_the_lock() {
        let seat = LockedSeat::open(&[]);
        let (_resume, clocks) = seat.resume();
        let (events, _stop) = subscribe_input(&seat)(1);
        over_window(&seat);
        let held = later(vec![
            syn(10),
            at_millis(key(KEY_A, KEY_PRESS), 11),
            at_millis(key(BTN_LEFT, KEY_PRESS), 11),
            syn(11),
        ]);
        let mut device = LostAtResume {
            data: Some(held.into_iter().flat_map(encode).collect()),
            clocks: clocks.clone(),
        };
        let lost = read_device(
            Path::new("event-lost"),
            &mut device,
            0,
            seat.target.as_ref(),
            seat.bindings.as_ref(),
            None,
            &mut || None,
        );
        assert!(lost.is_err());
        let seen = delivered(&events);
        let press = |kind: &str, code: u16| {
            seen.iter()
                .filter(|seen| seen.starts_with(&format!("{kind} {code} ")))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(press("key", KEY_A).len(), 1, "{seen:?}");
        assert!(press("key", KEY_A)[0].ends_with("Pressed"), "{seen:?}");
        assert_eq!(press("button", BTN_LEFT).len(), 1, "{seen:?}");
        assert!(
            press("button", BTN_LEFT)[0].ends_with("Pressed"),
            "{seen:?}"
        );
        assert!(seat.locked());
        assert!(seat.glass() == seat.lock_surface());
    }

    /// While a suspend's lock is owed and not yet made, nothing reaches a
    /// client: no new window's keyboard enter, no key, modifier, button or
    /// motion, and no new frame. The lock then withdraws the window.
    #[test]
    fn an_owed_lock_withholds_focus_and_input() {
        let seat = LockedSeat::open(&[]);
        let (resume, clocks) = seat.resume();
        let mut subscribe = subscribe_input(&seat);
        let (first, _first_stop) = subscribe(1);
        let (second, _second_stop) = subscribe(2);
        over_window(&seat);
        assert!(delivered(&first).contains(&"pointer enter".to_string()));
        assert!(!resume.check());
        clocks.suspend(Duration::from_secs(3));
        {
            let mut runtime = seat.runtime.lock().unwrap();
            runtime
                .commit(
                    crate::scene::SurfaceKey {
                        client: 2,
                        object: 1,
                    },
                    crate::buffer::Surface::from_shm_pixels(
                        100,
                        100,
                        [9, 8, 7, 0].repeat(10_000),
                        crate::scene::SHM_XRGB8888,
                    )
                    .unwrap(),
                )
                .unwrap();
            runtime.key(key(KEY_A, KEY_PRESS).key_input()).unwrap();
            runtime
                .modifiers(ModifierState {
                    depressed: MOD_SHIFT,
                    ..ModifierState::default()
                })
                .unwrap();
            let press = [PointerButtonInput {
                button: u32::from(BTN_LEFT),
                state: PointerButtonState::Pressed,
                time: 1,
            }];
            runtime
                .pointer_frame(1, 5, 5, &press, PointerScroll::default())
                .unwrap();
            runtime
                .pointer_frame_at(
                    2,
                    Fraction {
                        numerator: 1,
                        denominator: 2,
                    },
                    Fraction {
                        numerator: 1,
                        denominator: 2,
                    },
                    &[],
                    PointerScroll::default(),
                )
                .unwrap();
        }
        assert_eq!(delivered(&first), Vec::<String>::new());
        assert_eq!(delivered(&second), Vec::<String>::new());
        assert!(!seat.glass().as_chunks::<4>().0.contains(&[9, 8, 7, 0]));
        assert!(resume.watch().owed);
        resume
            .gate(seat.target.as_ref(), seat.bindings.as_ref())
            .unwrap();
        assert!(seat.locked() && !resume.watch().owed);
        assert!(seat.glass() == seat.lock_surface());
        assert!(delivered(&second)
            .iter()
            .all(|seen| !seen.ends_with("Pressed") && seen != "keyboard enter"));
    }

    /// A resume lock refused before the lock state was set stays owed: the
    /// reader reads on rather than ending, the batch's key reaches no
    /// client meanwhile, and the next gate retakes the lock.
    #[test]
    fn a_refused_resume_lock_stays_owed_and_is_retaken() {
        let seat = LockedSeat::open(&[]);
        let (resume, clocks) = seat.resume();
        let (events, _stop) = seat
            .runtime
            .lock()
            .unwrap()
            .subscribe_keyboard(1)
            .unwrap()
            .split();
        assert!(!resume.check());
        clocks.suspend(Duration::from_secs(3));
        // The runtime's entry refuses before `Scene::lock`.
        seat.runtime.lock().unwrap().enable_attention(false);
        seat.try_read(0, 10, presses(&[KEY_A], 11)).unwrap();
        assert!(!seat.locked());
        assert!(resume.watch().owed);
        assert!(!delivered(&events)
            .iter()
            .any(|seen| seen.starts_with("key ")));
        seat.runtime.lock().unwrap().enable_attention(true);
        seat.try_read(0, 20, presses(&[KEY_B], 21)).unwrap();
        assert!(seat.locked() && !resume.watch().owed);
        assert!(!delivered(&events)
            .iter()
            .any(|seen| seen.starts_with("key ")));
        assert!(seat.glass() == seat.lock_surface());
    }

    /// TOKEN-LOGIN.md's D12 through a lid close and a resume: a lock while
    /// a lifetime is open ends it as Escape does. Before an unlock's commit
    /// it is cancelled with `15`; after it, with a key held, the screen
    /// drains, root's `06` unlocks nothing and its result is never shown.
    #[test]
    fn a_lid_close_or_resume_during_an_unlock_ends_it_as_escape_does() {
        for trigger in TRIGGERS {
            // Before the commit.
            let seat = LockedSeat::new(&[]);
            let attempt = seat.unlock(10);
            let mut root = root(to_pin());
            let mut client = crate::secret_client::Client::trusting_memory();
            client.start(&mut root, attempt).unwrap();
            run(&mut client, &mut root).unwrap();
            assert_eq!(seat.field(), Some(crate::attention::Field::Pin(0)));
            seat.trigger(*trigger).unwrap();
            assert!(!seat.attention_open() && seat.locked(), "{trigger:?}");
            assert!(seat.glass() == seat.lock_surface());
            root.replies.extend([
                vec![0x95, 0],
                [&[0x91, 0x0d, 0x80, 0][..], &pin_step().encode()].concat(),
            ]);
            run(&mut client, &mut root).unwrap();
            assert!(root.replies.is_empty());
            assert_eq!(root.sent(0x15), 1, "{trigger:?}");
            assert_eq!(root.sent(0x1c), 0);
            assert!(seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            // After the commit, before root's success, with a key held.
            let seat = LockedSeat::new(&[]);
            let (mut client, mut root) = seat.committed();
            let mut pointer = PointerMotion::default();
            let feed = |pointer: &mut PointerMotion, value, time| {
                for event in later(vec![at_millis(key(KEY_A, value), time), syn(time)]) {
                    apply(
                        seat.target.as_ref(),
                        event,
                        0,
                        seat.bindings.as_ref(),
                        pointer,
                        None,
                    )
                    .unwrap();
                }
            };
            feed(&mut pointer, KEY_PRESS, 61);
            seat.trigger(*trigger).unwrap();
            assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
            root.replies.push_back(status(6, &pin_step()));
            run(&mut client, &mut root).unwrap();
            assert!(root.replies.is_empty());
            assert!(seat.locked(), "{trigger:?}");
            assert_eq!(root.sent(0x15), 0);
            assert!(seat.glass() == drained());
            feed(&mut pointer, KEY_RELEASE, 71);
            assert!(!seat.attention_open() && seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        }
    }

    /// Whether the chord just read opened the lock surface's unlock.
    fn unlock_opened(seat: &LockedSeat) -> bool {
        seat.attention_open()
            && seat.queued.attempt().is_some_and(|attempt| {
                attempt.selection()
                    == &crate::secret_client::Selection::Login(
                        crate::secret_client::LoginSelection::Unlock,
                    )
            })
    }

    /// A lid close's or a resume's lock while attention is open is a
    /// close no key started, so it settles rather than discarding each
    /// device's first report. A key inside the window is dropped and
    /// counted, both its reports; another keyboard's first report after
    /// the lock, the unlock chord's Control press on the lock surface, is
    /// taken, and the chord opens the unlock at once.
    #[test]
    fn a_suspend_lock_settles_and_keeps_the_next_key() {
        for trigger in TRIGGERS {
            let seat = LockedSeat::open(&[]);
            let mut resync = || None;
            let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            read_events(&seat, 0, &mut keyboard, later(chord_reports(10)));
            assert!(seat.attention_open() && !seat.locked(), "{trigger:?}");
            seat.trigger(*trigger).unwrap();
            assert!(!seat.attention_open() && seat.locked(), "{trigger:?}");
            let cutoff = seat.bindings.lock().unwrap().cutoff.unwrap();
            assert_eq!(
                seat.bindings.lock().unwrap().settled,
                Some(cutoff + SELF_CLOSE_SETTLE)
            );
            let inside = [KEY_PRESS, KEY_RELEASE]
                .into_iter()
                .zip(1u128..)
                .flat_map(|(value, n)| {
                    [key(KEY_LEFTCTRL, value), syn(1)].map(|mut event| {
                        event.timestamp = cutoff + n * SELF_CLOSE_SETTLE / 4;
                        event
                    })
                })
                .collect();
            read_events(&seat, 0, &mut keyboard, inside);
            {
                let bindings = seat.bindings.lock().unwrap();
                assert_eq!(bindings.settle_dropped, Some(2), "{trigger:?}");
                assert!(bindings.pressed.is_empty());
            }
            let mut resync = || None;
            let mut other = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            read_events(&seat, 1, &mut other, later(chord_reports(30)));
            assert!(unlock_opened(&seat), "{trigger:?}");
            assert_eq!(seat.bindings.lock().unwrap().settle_dropped, None);
        }
    }

    /// A resume found by a reader's batch (item 7) locks before that
    /// batch is routed. The batch's own reports were stamped before the
    /// lock's cutoff, so they are dropped as stale, the chord in them
    /// too, and not counted: this pins that drop. The lock settles, so
    /// the next batch, from a reader with no report since, is taken whole
    /// and its chord opens the unlock at once.
    #[test]
    fn a_resumes_own_batch_is_dropped_and_the_next_taken_whole() {
        let seat = LockedSeat::open(&[]);
        let (_resume, clocks) = seat.resume();
        seat.chord(10);
        assert!(seat.attention_open() && !seat.locked());
        clocks.advance(Duration::from_secs(1));
        clocks.suspend(Duration::from_secs(3));
        // Stamped early in this host's monotonic time, as reports queued
        // across the suspend are stamped before the lock that reads them.
        let batch = chord_reports(30);
        let stamped = batch.iter().map(|event| event.timestamp).max().unwrap();
        read_batch(&seat, 0, batch);
        assert!(seat.locked() && !seat.attention_open());
        assert!(seat.queued.attempt().is_none());
        {
            let bindings = seat.bindings.lock().unwrap();
            let cutoff = bindings.cutoff.unwrap();
            assert!(stamped < cutoff);
            assert_eq!(bindings.settled, Some(cutoff + SELF_CLOSE_SETTLE));
            assert_eq!(bindings.settle_dropped, Some(0));
        }
        read_batch(&seat, 0, later(chord_reports(50)));
        assert!(unlock_opened(&seat));
    }

    /// A lid or resume lock that arrives while Escape's drain waits on a
    /// held key does not change the close's kind, fixed when Escape
    /// started the drain: the close is a key's and keeps the first-report
    /// discard, so another device's first report after it, a chord's
    /// Control press, is dropped, and that device's next chord unlocks.
    #[test]
    fn a_suspend_lock_during_escapes_held_drain_stays_a_key_close() {
        for trigger in TRIGGERS {
            let seat = LockedSeat::open(&[]);
            let mut resync = || None;
            let mut keyboard = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            read_events(&seat, 0, &mut keyboard, later(chord_reports(10)));
            let held = vec![
                at_millis(key(KEY_LEFTSHIFT, KEY_PRESS), 20),
                syn(20),
                at_millis(key(KEY_ESC, KEY_PRESS), 21),
                syn(21),
                at_millis(key(KEY_ESC, KEY_RELEASE), 22),
                syn(22),
            ];
            read_events(&seat, 0, &mut keyboard, later(held));
            assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
            seat.trigger(*trigger).unwrap();
            assert!(seat.locked(), "{trigger:?}");
            assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
            let release = vec![at_millis(key(KEY_LEFTSHIFT, KEY_RELEASE), 30), syn(30)];
            read_events(&seat, 0, &mut keyboard, later(release));
            assert!(!seat.attention_open(), "{trigger:?}");
            {
                let bindings = seat.bindings.lock().unwrap();
                assert_eq!(bindings.keyless_close, None);
                assert_eq!(bindings.settled, None, "{trigger:?}");
            }
            let mut resync = || None;
            let mut other = DeviceState::new(None, AbsoluteKind::Tablet, &mut resync, true);
            read_events(&seat, 1, &mut other, later(chord_reports(40)));
            assert!(!seat.attention_open(), "{trigger:?}");
            assert!(seat.queued.attempt().is_none());
            read_events(&seat, 1, &mut other, later(chord_reports(50)));
            assert!(unlock_opened(&seat), "{trigger:?}");
        }
    }

    /// Escape's drained screen at 800x600, which shows no result.
    fn drained() -> Vec<u8> {
        let mut drained = vec![0; 800 * 600 * 4];
        crate::attention::paint(
            &mut drained,
            800,
            600,
            3200,
            true,
            false,
            crate::attention::Notice::default(),
        );
        drained
    }

    fn remove_step(
        step: crate::authority::consent::LoginStep,
    ) -> crate::authority::consent::Request {
        crate::authority::consent::Request::new(
            LOCK_NONCE,
            1000,
            crate::authority::consent::Operation::LoginRemove {
                account: 1000,
                before: 3,
                after: 2,
                removed: vec![crate::authority::consent::Slot {
                    position: 1,
                    key: ENROLLED[0],
                }],
                step,
            },
        )
        .unwrap()
    }

    fn authorize_step() -> crate::authority::consent::Request {
        remove_step(crate::authority::consent::LoginStep::Authorize {
            key: ENROLLED[0],
            retries: 8,
        })
    }

    /// A removal of the first key chosen on an unlocked seat's
    /// key-management screen, driven to its PIN step: root's answers from
    /// `1b` until the authorize step asks for its PIN.
    fn removal_to_pin(seat: &LockedSeat) -> (crate::secret_client::Client, Root) {
        use crate::secret_client::{LoginSelection, Selection};
        seat.chord(10);
        seat.press(&[KEY_K, KEY_D, KEY_1, KEY_ENTER], 20);
        let attempt = seat.queued.attempt().unwrap();
        assert!(matches!(
            attempt.selection(),
            Selection::Login(LoginSelection::Remove(_))
        ));
        let (identify, step) = (
            remove_step(crate::authority::consent::LoginStep::Identify),
            authorize_step(),
        );
        let mut root = root(vec![
            started(),
            vec![0x91, 0x0b],
            status(3, &identify),
            status(4, &identify),
            vec![0x93],
            status(3, &identify),
            status(4, &step),
            vec![0x93],
            status(0x0c, &step),
        ]);
        let mut client = crate::secret_client::Client::trusting_memory();
        client.start(&mut root, attempt).unwrap();
        run(&mut client, &mut root).unwrap();
        assert_eq!(seat.field(), Some(crate::attention::Field::Pin(0)));
        (client, root)
    }

    /// D12 for a key-management lifetime, which C9 left to the lid and
    /// resume: a removal open when either comes is cancelled with `15`
    /// before its commit; after its commit the screen drains under
    /// Escape's screen, root's success is never shown, and the session
    /// ends on the lock surface.
    #[test]
    fn a_lid_close_or_resume_during_a_removal_ends_it_as_escape_does() {
        for trigger in TRIGGERS {
            // Before the commit.
            let seat = LockedSeat::open(&[]);
            let (mut client, mut root) = removal_to_pin(&seat);
            seat.trigger(*trigger).unwrap();
            assert!(!seat.attention_open() && seat.locked(), "{trigger:?}");
            assert!(seat.glass() == seat.lock_surface());
            root.replies.extend([
                vec![0x95, 0],
                [&[0x91, 0x0d, 0x80, 0][..], &authorize_step().encode()].concat(),
            ]);
            run(&mut client, &mut root).unwrap();
            assert!(root.replies.is_empty());
            assert_eq!(root.sent(0x15), 1, "{trigger:?}");
            assert_eq!(root.sent(0x1c), 0);
            assert!(seat.locked() && seat.glass() == seat.lock_surface());
            // After the commit, with a key held.
            let seat = LockedSeat::open(&[]);
            let (mut client, mut root) = removal_to_pin(&seat);
            let step = authorize_step();
            seat.press(&[KEY_1, KEY_2, KEY_3, KEY_4, KEY_ENTER], 40);
            root.replies.extend([
                status(0x0c, &step),
                vec![0x9c, 0],
                status(3, &step),
                status(5, &step),
                vec![0x94],
                status(3, &step),
            ]);
            run(&mut client, &mut root).unwrap();
            assert_eq!(root.sent(0x14), 1);
            let mut pointer = PointerMotion::default();
            let feed = |pointer: &mut PointerMotion, value, time| {
                for event in later(vec![at_millis(key(KEY_A, value), time), syn(time)]) {
                    apply(
                        seat.target.as_ref(),
                        event,
                        0,
                        seat.bindings.as_ref(),
                        pointer,
                        None,
                    )
                    .unwrap();
                }
            };
            feed(&mut pointer, KEY_PRESS, 61);
            seat.trigger(*trigger).unwrap();
            assert!(seat.bindings.lock().unwrap().attention == AttentionState::Draining);
            root.replies.push_back(status(6, &step));
            run(&mut client, &mut root).unwrap();
            assert!(root.replies.is_empty());
            assert_eq!(root.sent(0x15), 0, "{trigger:?}");
            assert!(seat.locked());
            assert!(seat.glass() == drained());
            assert_ne!(
                seat.shown(),
                Some(crate::attention::Notice::Login(&["LOGIN KEYS REMOVED"]))
            );
            feed(&mut pointer, KEY_RELEASE, 71);
            assert!(!seat.attention_open() && seat.locked());
            assert!(seat.glass() == seat.lock_surface());
            assert_eq!(seat.runtime.lock().unwrap().keyboard_snapshot().focus, None);
        }
    }
}
