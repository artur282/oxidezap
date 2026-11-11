use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Keycode, ModMask, Window};

use crate::shortcut::Shortcut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    Toggle,
    BossKey,
    CommandPalette,
}

/// The global shortcuts the hotkey thread grabs. A shortcut that duplicates a
/// higher-priority binding is dropped because the actions would be
/// indistinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyBindings {
    pub toggle: Shortcut,
    pub boss_key: Option<Shortcut>,
    pub command_palette: Option<Shortcut>,
}

impl HotkeyBindings {
    pub fn new(
        toggle: Shortcut,
        boss_key: Option<Shortcut>,
        command_palette: Option<Shortcut>,
    ) -> Self {
        let boss_key = boss_key.filter(|boss_key| *boss_key != toggle);
        let command_palette =
            command_palette.filter(|shortcut| *shortcut != toggle && Some(*shortcut) != boss_key);
        Self {
            toggle,
            boss_key,
            command_palette,
        }
    }

    fn entries(self) -> impl Iterator<Item = (Shortcut, HotkeyAction)> {
        std::iter::once((self.toggle, HotkeyAction::Toggle))
            .chain(
                self.boss_key
                    .map(|shortcut| (shortcut, HotkeyAction::BossKey)),
            )
            .chain(
                self.command_palette
                    .map(|shortcut| (shortcut, HotkeyAction::CommandPalette)),
            )
    }
}

enum Command {
    Set(HotkeyBindings),
    Stop,
}

#[derive(Clone, Copy)]
struct Grab {
    keycode: Keycode,
    mask: u16,
    action: HotkeyAction,
}

impl Grab {
    fn variants(self) -> [u16; 4] {
        let lock = u16::from(ModMask::LOCK);
        let num_lock = u16::from(ModMask::M2);
        [
            self.mask,
            self.mask | lock,
            self.mask | num_lock,
            self.mask | lock | num_lock,
        ]
    }
}

pub struct HotkeyHandle {
    tx: Sender<Command>,
    _thread: JoinHandle<()>,
}

impl HotkeyHandle {
    pub fn set(&self, bindings: HotkeyBindings) {
        let _ = self.tx.try_send(Command::Set(bindings));
    }

    pub fn stop(&self) {
        let _ = self.tx.try_send(Command::Stop);
    }
}

pub fn start(shortcut: Shortcut, actions: Sender<HotkeyAction>) -> HotkeyHandle {
    let (tx, rx) = bounded(4);
    let thread = thread::Builder::new()
        .name("oxidezap-hotkey".into())
        .spawn(move || run(rx, actions))
        .expect("could not start the global shortcut thread");
    let handle = HotkeyHandle {
        tx,
        _thread: thread,
    };
    handle.set(HotkeyBindings::new(shortcut, None, None));
    handle
}

fn run(rx: Receiver<Command>, actions: Sender<HotkeyAction>) {
    let (conn, screen_num) = match x11rb::connect(None) {
        Ok(connection) => connection,
        Err(error) => {
            warn!(%error, "global shortcut unavailable: could not connect to X11");
            return;
        }
    };
    let root = conn.setup().roots[screen_num].root;
    let min_keycode = conn.setup().min_keycode;
    let max_keycode = conn.setup().max_keycode;
    let mut grabs: Vec<Grab> = Vec::new();
    let mut current = HotkeyBindings::new(Shortcut::default(), None, None);

    loop {
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Set(bindings) => {
                    ungrab_all(&conn, root, &grabs);
                    current = bindings;
                    grabs = register_all(&conn, root, min_keycode, max_keycode, current);
                }
                Command::Stop => {
                    ungrab_all(&conn, root, &grabs);
                    return;
                }
            }
        }

        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(event))) => {
                    if let Some(action) =
                        action_for_event(&grabs, event.detail, u16::from(event.state))
                    {
                        debug!(keycode = event.detail, ?action, "global shortcut pressed");
                        let _ = actions.try_send(action);
                    }
                }
                Ok(Some(Event::MappingNotify(_))) => {
                    ungrab_all(&conn, root, &grabs);
                    grabs = register_all(&conn, root, min_keycode, max_keycode, current);
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    warn!(%error, "global shortcut event loop stopped");
                    return;
                }
            }
        }

        thread::sleep(Duration::from_millis(40));
    }
}

/// Selects the action for a KeyPress. Only the four shortcuts modifiers are
/// compared, so CapsLock and NumLock variants do not change the action, and an
/// extra modifier never falls back to a less specific binding.
fn action_for_event(grabs: &[Grab], keycode: Keycode, state: u16) -> Option<HotkeyAction> {
    let relevant = state
        & (u16::from(ModMask::CONTROL)
            | u16::from(ModMask::M1)
            | u16::from(ModMask::SHIFT)
            | u16::from(ModMask::M4));
    grabs
        .iter()
        .find(|grab| grab.keycode == keycode && relevant == grab.mask)
        .map(|grab| grab.action)
}

fn register_all(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
    bindings: HotkeyBindings,
) -> Vec<Grab> {
    bindings
        .entries()
        .filter_map(|(shortcut, action)| {
            register(conn, root, min_keycode, max_keycode, shortcut, action)
        })
        .collect()
}

fn ungrab_all(conn: &x11rb::rust_connection::RustConnection, root: Window, grabs: &[Grab]) {
    for grab in grabs {
        ungrab(conn, root, *grab);
    }
}

fn register(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
    shortcut: Shortcut,
    action: HotkeyAction,
) -> Option<Grab> {
    let Some(keysym) = shortcut.keysym() else {
        warn!(%shortcut, "global shortcut has no X11 keysym");
        return None;
    };
    let count = max_keycode - min_keycode + 1;
    let mapping = match conn.get_keyboard_mapping(min_keycode, count) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply,
            Err(error) => {
                warn!(%error, "could not read the keyboard mapping");
                return None;
            }
        },
        Err(error) => {
            warn!(%error, "could not request the keyboard mapping");
            return None;
        }
    };
    let per_keycode = usize::from(mapping.keysyms_per_keycode);
    let Some(keycode) = mapping
        .keysyms
        .chunks(per_keycode.max(1))
        .position(|symbols| symbols.contains(&keysym))
        .map(|index| min_keycode + index as Keycode)
    else {
        warn!(%shortcut, "could not find the keycode for the global shortcut");
        return None;
    };

    let mask = modifier_mask(shortcut);
    let grab = Grab {
        keycode,
        mask,
        action,
    };
    let mut successes = 0;
    for variant in grab.variants() {
        let cookie = conn.grab_key(
            false,
            root,
            ModMask::from(variant),
            keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
        if let Ok(cookie) = cookie {
            if cookie.check().is_ok() {
                successes += 1;
            }
        }
    }
    if successes == 0 {
        warn!(
            shortcut = %shortcut,
            "could not grab the global shortcut; another application may own it"
        );
        return None;
    }
    let _ = conn.flush();
    info!(shortcut = %shortcut, keycode, "global shortcut registered");
    Some(grab)
}

fn ungrab(conn: &x11rb::rust_connection::RustConnection, root: Window, grab: Grab) {
    for variant in grab.variants() {
        let _ = conn.ungrab_key(grab.keycode, root, ModMask::from(variant));
    }
    let _ = conn.flush();
}

fn modifier_mask(shortcut: Shortcut) -> u16 {
    let mut mask = 0u16;
    if shortcut.ctrl {
        mask |= u16::from(ModMask::CONTROL);
    }
    if shortcut.alt {
        mask |= u16::from(ModMask::M1);
    }
    if shortcut.shift {
        mask |= u16::from(ModMask::SHIFT);
    }
    if shortcut.super_key {
        mask |= u16::from(ModMask::M4);
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(input: &str) -> Shortcut {
        Shortcut::parse(input).unwrap()
    }

    fn grab(keycode: Keycode, mask: u16, action: HotkeyAction) -> Grab {
        Grab {
            keycode,
            mask,
            action,
        }
    }

    #[test]
    fn bindings_map_each_shortcut_to_its_action() {
        let bindings = HotkeyBindings::new(
            shortcut("Ctrl+Shift+W"),
            Some(shortcut("Ctrl+Shift+H")),
            Some(shortcut("Ctrl+Shift+K")),
        );
        let entries: Vec<(Shortcut, HotkeyAction)> = bindings.entries().collect();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (shortcut("Ctrl+Shift+W"), HotkeyAction::Toggle));
        assert_eq!(
            entries[1],
            (shortcut("Ctrl+Shift+H"), HotkeyAction::BossKey)
        );
        assert_eq!(
            entries[2],
            (shortcut("Ctrl+Shift+K"), HotkeyAction::CommandPalette)
        );
    }

    #[test]
    fn a_boss_key_equal_to_the_show_hide_shortcut_is_ignored() {
        let toggle = shortcut("Ctrl+Shift+W");
        let bindings = HotkeyBindings::new(toggle, Some(toggle), None);

        assert_eq!(bindings.boss_key, None);
        assert_eq!(bindings.entries().count(), 1);
    }

    #[test]
    fn a_palette_shortcut_that_duplicates_another_binding_is_ignored() {
        let toggle = shortcut("Ctrl+Shift+W");
        let boss = shortcut("Ctrl+Shift+H");

        let bindings = HotkeyBindings::new(toggle, Some(boss), Some(toggle));
        assert_eq!(bindings.command_palette, None);
        assert_eq!(bindings.entries().count(), 2);

        let bindings = HotkeyBindings::new(toggle, Some(boss), Some(boss));
        assert_eq!(bindings.command_palette, None);
        assert_eq!(bindings.entries().count(), 2);
    }

    #[test]
    fn the_global_palette_shortcut_is_optional() {
        let bindings = HotkeyBindings::new(shortcut("Ctrl+Shift+W"), None, None);

        assert_eq!(bindings.command_palette, None);
        assert_eq!(bindings.entries().count(), 1);
    }

    #[test]
    fn keycode_and_mask_select_the_matching_action() {
        let control = u16::from(ModMask::CONTROL);
        let shift = u16::from(ModMask::SHIFT);
        let grabs = vec![
            grab(10, control | shift, HotkeyAction::Toggle),
            grab(20, control | shift, HotkeyAction::BossKey),
        ];

        assert_eq!(
            action_for_event(&grabs, 10, control | shift),
            Some(HotkeyAction::Toggle)
        );
        assert_eq!(
            action_for_event(&grabs, 20, control | shift),
            Some(HotkeyAction::BossKey)
        );
        assert_eq!(action_for_event(&grabs, 10, control), None);
        assert_eq!(action_for_event(&grabs, 30, control | shift), None);
    }

    #[test]
    fn extra_modifiers_do_not_trigger_a_binding() {
        let control = u16::from(ModMask::CONTROL);
        let shift = u16::from(ModMask::SHIFT);
        let alt = u16::from(ModMask::M1);
        let grabs = vec![grab(10, control | shift, HotkeyAction::Toggle)];

        assert_eq!(action_for_event(&grabs, 10, control | shift | alt), None);
        assert_eq!(
            action_for_event(&grabs, 10, control | shift),
            Some(HotkeyAction::Toggle)
        );
    }

    #[test]
    fn caps_lock_and_num_lock_are_ignored() {
        let control = u16::from(ModMask::CONTROL);
        let shift = u16::from(ModMask::SHIFT);
        let lock = u16::from(ModMask::LOCK);
        let num_lock = u16::from(ModMask::M2);
        let grabs = vec![grab(10, control | shift, HotkeyAction::BossKey)];

        assert_eq!(
            action_for_event(&grabs, 10, control | shift | lock | num_lock),
            Some(HotkeyAction::BossKey)
        );
    }
}
