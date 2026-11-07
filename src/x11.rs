//! X11 integration for the GPUI account windows and the GTK/WebKit overlay.
//!
//! Every account window reports the same `oxidezap` `WM_CLASS`, so the account
//! identity is tracked in two ways: the `_OXIDEZAP_ACCOUNT` property (kept as a
//! diagnostic and as the fallback when a cached XID is lost) and a
//! deterministic discovery pass that pairs the window created for an account
//! with its unique title. The overlay caches the GPUI XID and does not consult
//! the X11 tree on every synchronization tick.

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

/// One top-level window seen while discovering a newly opened account window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub window: Window,
    pub title: Option<String>,
    pub account_tag: Option<String>,
}

/// Selects the GPUI window that belongs to an account whose window was just
/// opened.
///
/// `known_windows` is a snapshot of the account windows taken immediately
/// before the new GPUI window was created. A window outside that snapshot is
/// the unique creation token for the window that was just opened, which keeps
/// two accounts opening close together from claiming each other's window.
/// Titles are matched first because they identify the account even when both
/// windows are new; the creation token resolves duplicate account names.
///
/// Windows tagged for another account are never selected, and a window already
/// tagged for this account is the last-resort fallback used to rebind an
/// overlay that lost its cached XID.
pub fn select_account_window(
    known_windows: &[Window],
    candidates: &[WindowInfo],
    account_id: &str,
    expected_title: &str,
) -> Option<Window> {
    let untagged: Vec<&WindowInfo> = candidates
        .iter()
        .filter(|candidate| candidate.account_tag.is_none())
        .collect();
    let is_new = |candidate: &&WindowInfo| !known_windows.contains(&candidate.window);
    let matches_title = |candidate: &&WindowInfo| {
        candidate
            .title
            .as_deref()
            .is_some_and(|title| title_matches_account(title, expected_title))
    };

    untagged
        .iter()
        .copied()
        .filter(|candidate| is_new(candidate) && matches_title(candidate))
        .max_by_key(|candidate| candidate.window)
        .or_else(|| {
            untagged
                .iter()
                .copied()
                .filter(is_new)
                .max_by_key(|candidate| candidate.window)
        })
        .or_else(|| {
            untagged
                .iter()
                .copied()
                .find(|candidate| matches_title(candidate))
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| candidate.account_tag.as_deref() == Some(account_id))
        })
        .or_else(|| untagged.first().copied())
        .map(|candidate| candidate.window)
}

/// Matches an account window title, accepting the unread suffix that the
/// account titlebar appends (`OxideZap — Work (3)`).
fn title_matches_account(title: &str, expected_title: &str) -> bool {
    title == expected_title
        || title
            .strip_prefix(expected_title)
            .is_some_and(|suffix| suffix.starts_with(" ("))
}

pub struct X11 {
    pub conn: RustConnection,
    pub root: Window,
    account_atom: Atom,
    pid_atom: Atom,
    active_window_atom: Atom,
    net_wm_name_atom: Atom,
    utf8_string_atom: Atom,
    window_type_atom: Atom,
    window_type_utility_atom: Atom,
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
        let net_wm_name_atom = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let window_type_atom = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")?
            .reply()?
            .atom;
        let window_type_utility_atom = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_UTILITY")?
            .reply()?
            .atom;
        Ok(Self {
            conn,
            root,
            account_atom,
            pid_atom,
            active_window_atom,
            net_wm_name_atom,
            utf8_string_atom,
            window_type_atom,
            window_type_utility_atom,
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

    /// Lists this process's account windows in front-to-back order. GTK
    /// overlays also share the `oxidezap` class, so windows using the Utility
    /// type hint are excluded: only GPUI account windows remain.
    pub fn account_window_candidates(&self) -> Result<Vec<Window>> {
        let pid = std::process::id();
        let mut windows = Vec::new();
        for window in self.children()? {
            if self.pid_of(window) != Some(pid) {
                continue;
            }
            if self.class_of(window).as_deref() != Some(APP_ID) {
                continue;
            }
            if self.has_window_type(window, self.window_type_utility_atom) {
                continue;
            }
            windows.push(window);
        }
        Ok(windows)
    }

    /// Tags the window for the account and clears the tag from any stale window
    /// that still carries the same account id, so the fallback property always
    /// points at the live window after a recreation.
    pub fn tag_account(&self, window: Window, account_id: &str) -> Result<()> {
        for other in self.children()? {
            if other != window && self.account_for(other).as_deref() == Some(account_id) {
                if let Ok(cookie) = self.conn.delete_property(other, self.account_atom) {
                    let _ = cookie.check();
                }
            }
        }
        self.set_string(window, self.account_atom, account_id)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn account_for(&self, window: Window) -> Option<String> {
        self.get_string(window, self.account_atom, AtomEnum::STRING.into())
    }

    /// Reads the window title, preferring the EWMH `_NET_WM_NAME` property and
    /// falling back to the legacy `WM_NAME` GPUI sets at window creation.
    pub fn title_of(&self, window: Window) -> Option<String> {
        self.get_string(window, self.net_wm_name_atom, self.utf8_string_atom)
            .filter(|title| !title.is_empty())
            .or_else(|| {
                self.get_string(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())
                    .filter(|title| !title.is_empty())
            })
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

    fn has_window_type(&self, window: Window, expected: Atom) -> bool {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, self.window_type_atom, AtomEnum::ATOM, 0, 16)
        else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        reply
            .value32()
            .is_some_and(|mut types| types.any(|window_type| window_type == expected))
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

    fn get_string(&self, window: Window, property: Atom, value_type: Atom) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, window, property, value_type, 0, 256)
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

    fn window(id: Window, title: &str) -> WindowInfo {
        WindowInfo {
            window: id,
            title: Some(title.to_string()),
            account_tag: None,
        }
    }

    fn tagged_window(id: Window, title: &str, account_tag: &str) -> WindowInfo {
        WindowInfo {
            window: id,
            title: Some(title.to_string()),
            account_tag: Some(account_tag.to_string()),
        }
    }

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

    #[test]
    fn title_matching_accepts_the_unread_suffix() {
        assert!(title_matches_account("OxideZap — Work", "OxideZap — Work"));
        assert!(title_matches_account(
            "OxideZap — Work (3)",
            "OxideZap — Work"
        ));
        assert!(!title_matches_account(
            "OxideZap — Work 2",
            "OxideZap — Work"
        ));
        assert!(!title_matches_account(
            "OxideZap content",
            "OxideZap — Work"
        ));
    }

    #[test]
    fn new_window_for_the_account_wins_over_an_older_untagged_window() {
        let first = 10;
        let second = 20;
        let candidates = vec![
            window(first, "OxideZap — Work"),
            window(second, "OxideZap — Home"),
        ];

        let selected = select_account_window(&[first], &candidates, "home", "OxideZap — Home");

        assert_eq!(selected, Some(second));
    }

    #[test]
    fn duplicate_titles_resolve_to_the_newest_created_window() {
        let candidates = vec![
            window(10, "OxideZap — Account"),
            window(20, "OxideZap — Account"),
        ];

        let selected = select_account_window(&[], &candidates, "second", "OxideZap — Account");

        assert_eq!(selected, Some(20));
    }

    #[test]
    fn tagged_windows_are_never_stolen_for_another_account() {
        let candidates = vec![tagged_window(10, "OxideZap — Work", "other")];

        let selected = select_account_window(&[], &candidates, "work", "OxideZap — Work");

        assert_eq!(selected, None);
    }

    #[test]
    fn a_window_tagged_for_the_account_is_the_last_fallback() {
        let candidates = vec![tagged_window(10, "OxideZap — Work", "work")];

        let selected = select_account_window(&[], &candidates, "work", "OxideZap — Work");

        assert_eq!(selected, Some(10));
    }

    #[test]
    fn title_identity_is_used_when_the_snapshot_already_contains_the_window() {
        let candidates = vec![window(10, "OxideZap — Work"), window(20, "OxideZap — Home")];

        let selected = select_account_window(&[10, 20], &candidates, "work", "OxideZap — Work");

        assert_eq!(selected, Some(10));
    }

    #[test]
    fn untagged_fallback_skips_tagged_windows_and_prefers_the_newest() {
        let candidates = vec![
            tagged_window(5, "OxideZap — Other", "other"),
            WindowInfo {
                window: 10,
                title: None,
                account_tag: None,
            },
            WindowInfo {
                window: 20,
                title: None,
                account_tag: None,
            },
        ];

        let selected = select_account_window(&[], &candidates, "work", "OxideZap — Work");

        assert_eq!(selected, Some(20));
    }
}
