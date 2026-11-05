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
}

enum Command {
    Set(Shortcut),
    Stop,
}

#[derive(Clone, Copy)]
struct Grab {
    keycode: Keycode,
    mask: u16,
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
    pub fn set(&self, shortcut: Shortcut) {
        let _ = self.tx.try_send(Command::Set(shortcut));
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
    handle.set(shortcut);
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
    let mut grab: Option<Grab> = None;
    let mut current: Option<Shortcut> = None;

    loop {
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::Set(shortcut) => {
                    if let Some(old) = grab.take() {
                        ungrab(&conn, root, old);
                    }
                    current = Some(shortcut);
                    grab = register(&conn, root, min_keycode, max_keycode, shortcut);
                }
                Command::Stop => {
                    if let Some(old) = grab.take() {
                        ungrab(&conn, root, old);
                    }
                    return;
                }
            }
        }

        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(event))) => {
                    if let Some(grab) = grab {
                        if event.detail == grab.keycode
                            && u16::from(event.state) & grab.mask == grab.mask
                        {
                            debug!(keycode = event.detail, "global shortcut pressed");
                            let _ = actions.try_send(HotkeyAction::Toggle);
                        }
                    }
                }
                Ok(Some(Event::MappingNotify(_))) => {
                    if let Some(shortcut) = current {
                        if let Some(old) = grab.take() {
                            ungrab(&conn, root, old);
                        }
                        grab = register(&conn, root, min_keycode, max_keycode, shortcut);
                    }
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

fn register(
    conn: &x11rb::rust_connection::RustConnection,
    root: Window,
    min_keycode: Keycode,
    max_keycode: Keycode,
    shortcut: Shortcut,
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
    let grab = Grab { keycode, mask };
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
