use anyhow::{Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask,
    MapState, PropMode, StackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::paths::APP_ID;

pub const ACCOUNT_ATOM: &str = "_OXIDEZAP_ACCOUNT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

pub struct X11 {
    pub conn: RustConnection,
    pub root: Window,
    account_atom: Atom,
    pid_atom: Atom,
    active_window_atom: Atom,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("could not connect to X11")?;
        let root = conn.setup().roots[screen_num].root;
        let account_atom = conn
            .intern_atom(false, ACCOUNT_ATOM.as_bytes())?
            .reply()?
            .atom;
        let pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        let active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        Ok(Self {
            conn,
            root,
            account_atom,
            pid_atom,
            active_window_atom,
        })
    }

    pub fn find_account_window(&self, account_id: &str) -> Result<Option<Window>> {
        for window in self.children()? {
            if self.account_for(window).as_deref() == Some(account_id) {
                return Ok(Some(window));
            }
        }
        Ok(None)
    }

    pub fn find_untagged_account_window(&self) -> Result<Option<Window>> {
        let pid = std::process::id();
        for window in self.children()? {
            if self.pid_of(window) != Some(pid) {
                continue;
            }
            let Some(class) = self.class_of(window) else {
                continue;
            };
            if class != APP_ID {
                continue;
            }
            if self.account_for(window).is_none() {
                return Ok(Some(window));
            }
        }
        Ok(None)
    }

    pub fn tag_account(&self, window: Window, account_id: &str) -> Result<()> {
        self.set_string(window, self.account_atom, account_id)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn account_for(&self, window: Window) -> Option<String> {
        self.get_string(window, self.account_atom)
    }

    pub fn pid_of(&self, window: Window) -> Option<u32> {
        let reply = self
            .conn
            .get_property(false, window, self.pid_atom, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    pub fn class_of(&self, window: Window) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        let value = String::from_utf8_lossy(&reply.value);
        let mut parts = value.split('\0').filter(|part| !part.is_empty());
        let instance = parts.next()?;
        let class = parts.next().unwrap_or(instance);
        Some(class.to_string())
    }

    pub fn root_position(&self, window: Window) -> Result<(i32, i32)> {
        let reply = self
            .conn
            .translate_coordinates(window, self.root, 0, 0)?
            .reply()?;
        Ok((i32::from(reply.dst_x), i32::from(reply.dst_y)))
    }

    pub fn geometry(&self, window: Window) -> Result<Geometry> {
        let reply = self.conn.get_geometry(window)?.reply()?;
        Ok(Geometry {
            x: reply.x,
            y: reply.y,
            width: reply.width,
            height: reply.height,
        })
    }

    pub fn is_viewable(&self, window: Window) -> Result<bool> {
        let reply = self.conn.get_window_attributes(window)?.reply()?;
        Ok(reply.map_state == MapState::VIEWABLE)
    }

    pub fn transient_for(&self, window: Window) -> Option<Window> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    pub fn idle_ms(&self) -> Option<u32> {
        use x11rb::protocol::screensaver::ConnectionExt as _;
        let _extension = self
            .conn
            .extension_information(x11rb::protocol::screensaver::X11_EXTENSION_NAME)
            .ok()??;
        let reply = self
            .conn
            .screensaver_query_info(self.root)
            .ok()?
            .reply()
            .ok()?;
        Some(reply.ms_since_user_input)
    }

    pub fn set_transient(&self, window: Window, parent: Window) -> Result<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[parent],
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn configure(&self, window: Window, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width)
                .height(height),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn raise(&self, window: Window) -> Result<()> {
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn unmap(&self, window: Window) -> Result<()> {
        self.conn.unmap_window(window)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn map(&self, window: Window) -> Result<()> {
        self.conn.map_window(window)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn activate(&self, window: Window) -> Result<()> {
        let event =
            ClientMessageEvent::new(32, window, self.active_window_atom, [2u32, 0, 0, 0, 0]);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn children(&self) -> Result<Vec<Window>> {
        let tree = self.conn.query_tree(self.root)?.reply()?;
        Ok(tree.children.into_iter().rev().collect())
    }

    fn set_string(&self, window: Window, property: Atom, value: &str) -> Result<()> {
        self.conn.change_property8(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::STRING,
            value.as_bytes(),
        )?;
        Ok(())
    }

    fn get_string(&self, window: Window, property: Atom) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, window, property, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&reply.value).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_plain_data() {
        let geometry = Geometry {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        assert_eq!(geometry.width, 800);
    }
}
