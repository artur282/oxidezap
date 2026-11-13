use std::collections::HashSet;
use std::io::Cursor;

use crossbeam_channel::Sender;
use image::{ImageFormat, Rgba, RgbaImage};
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Icon, ToolTip};
use tracing::{info, warn};

use crate::settings::{Account, AccountColor};

const BADGE_CENTER: i32 = 46;
const BADGE_OUTER_RADIUS: i32 = 17;
const BADGE_RING_RADIUS: i32 = 15;
const BADGE_INNER_RADIUS: i32 = 13;

/// Size of the generated per-account menu icon. StatusNotifierItem consumers
/// scale it to their own menu row height.
const ACCOUNT_ICON_SIZE: u32 = 22;

const BADGE_OUTLINE: [u8; 4] = [24, 34, 42, 255];
const BADGE_RING: [u8; 4] = [255, 255, 255, 255];
const BADGE_UNREAD: [u8; 4] = [18, 140, 126, 255];
const BADGE_DND: [u8; 4] = [55, 65, 81, 255];
const GLYPH_COLOR: [u8; 4] = [255, 255, 255, 255];

const GLYPH_SCALE: i32 = 3;
const GLYPH_GAP: i32 = 3;
const GLYPH_WIDTH: i32 = 3 * GLYPH_SCALE;
const GLYPH_HEIGHT: i32 = 5 * GLYPH_SCALE;

const DIGIT_GLYPHS: [[&str; 5]; 10] = [
    ["111", "101", "101", "101", "111"],
    ["010", "110", "010", "010", "111"],
    ["111", "001", "111", "100", "111"],
    ["111", "001", "111", "001", "111"],
    ["101", "101", "111", "001", "001"],
    ["111", "100", "111", "001", "111"],
    ["111", "100", "111", "101", "111"],
    ["111", "001", "010", "010", "010"],
    ["111", "101", "111", "101", "111"],
    ["111", "101", "111", "001", "111"],
];

const PLUS_GLYPH: [&str; 5] = ["000", "010", "111", "010", "000"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ShowHide,
    OpenAccount(String),
    ResumeAll,
    AddAccount,
    Downloads,
    Settings,
    Lock,
    DoNotDisturbOff,
    DoNotDisturbOneHour,
    DoNotDisturbTomorrow,
    DoNotDisturbIndefinite,
    DoNotDisturbSchedule,
    DoNotDisturbSkip,
    ToggleAlwaysOnTop(String),
    ToggleMiniMode(String),
    Quit,
}

/// Window-mode state for the active account, shown as checkable tray entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayWindowState {
    pub account_id: String,
    pub name: String,
    pub always_on_top: bool,
    pub mini_mode: bool,
}

/// Effective Do Not Disturb presentation for the tray. `status` is the compact
/// text from the settings model, for example `Quiet hours until 07:00 (6h 20m
/// left)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayDndState {
    pub active: bool,
    pub status: String,
    pub follows_schedule: bool,
    pub schedule_active: bool,
    pub wake_time: String,
}

impl TrayDndState {
    fn tooltip_line(&self) -> Option<String> {
        if !self.active {
            return None;
        }
        if self.status.is_empty() {
            Some(t!("Do not disturb").to_string())
        } else {
            Some(tr!("Do not disturb: {status}", status = self.status))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayIconState {
    Normal,
    Unread(u32),
    UnreadOverflow,
    DoNotDisturb,
}

impl TrayIconState {
    pub fn for_unread(unread: u32, do_not_disturb: bool) -> Self {
        if do_not_disturb {
            Self::DoNotDisturb
        } else if unread == 0 {
            Self::Normal
        } else if unread <= 9 {
            Self::Unread(unread)
        } else {
            Self::UnreadOverflow
        }
    }
}

#[derive(Clone, Debug)]
struct TrayAccount {
    id: String,
    name: String,
    unread: u32,
    suspended: bool,
    accent: Option<AccountColor>,
}

pub struct OxideTray {
    tx: Sender<TrayAction>,
    accounts: Vec<TrayAccount>,
    unread: u32,
    visible: bool,
    dnd: TrayDndState,
    window: Option<TrayWindowState>,
    icons: IconCache,
}

impl OxideTray {
    fn new(tx: Sender<TrayAction>) -> Self {
        let mut icons = IconCache::load();
        icons.refresh(TrayIconState::Normal);
        Self {
            tx,
            accounts: Vec::new(),
            unread: 0,
            visible: true,
            dnd: TrayDndState::default(),
            window: None,
            icons,
        }
    }
}

impl ksni::Tray for OxideTray {
    fn id(&self) -> String {
        "oxidezap".to_string()
    }

    fn title(&self) -> String {
        if self.unread > 0 {
            tn!(
                self.unread,
                "OxideZap — {count} unread message",
                "OxideZap — {count} unread messages"
            )
        } else {
            "OxideZap".to_string()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.icons()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: self.icons.icons(),
            title: "OxideZap".to_string(),
            description: tooltip_description(
                &self.accounts,
                self.unread,
                self.dnd.tooltip_line().as_deref(),
            ),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowHide);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowHide);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();
        items.push(
            StandardItem {
                label: if self.visible {
                    t!("Hide OxideZap").to_string()
                } else {
                    t!("Show OxideZap").to_string()
                },
                icon_name: "view-restore".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::ShowHide);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: t!("Lock OxideZap").to_string(),
                icon_name: "system-lock-screen".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Lock);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);

        let active_account = self
            .window
            .as_ref()
            .map(|window| window.account_id.as_str());
        let account_items = self
            .accounts
            .iter()
            .map(|account| {
                let id = account.id.clone();
                let label =
                    account_menu_label(account, active_account == Some(account.id.as_str()));
                StandardItem {
                    label,
                    icon_name: if account.accent.is_some() {
                        String::new()
                    } else if account.suspended {
                        "user-away".to_string()
                    } else {
                        "user-available".to_string()
                    },
                    icon_data: account.accent.map(account_icon_data).unwrap_or_default(),
                    activate: Box::new(move |tray: &mut Self| {
                        let _ = tray.tx.try_send(TrayAction::OpenAccount(id.clone()));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        items.push(
            SubMenu {
                label: t!("Accounts").to_string(),
                submenu: account_items,
                ..Default::default()
            }
            .into(),
        );
        if self.accounts.iter().any(|account| account.suspended) {
            items.push(
                StandardItem {
                    label: t!("Resume all suspended accounts").to_string(),
                    icon_name: "view-restore".to_string(),
                    activate: Box::new(|tray: &mut Self| {
                        let _ = tray.tx.try_send(TrayAction::ResumeAll);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if let Some(window) = &self.window {
            let account = window.account_id.clone();
            let top_account = account.clone();
            let mini_account = account;
            items.push(
                SubMenu {
                    label: format!("Window: {}", window.name),
                    submenu: vec![
                        StandardItem {
                            label: if window.always_on_top {
                                "✓ Always on top".to_string()
                            } else {
                                "Always on top".to_string()
                            },
                            icon_name: "view-pin".to_string(),
                            activate: Box::new(move |tray: &mut Self| {
                                let _ = tray
                                    .tx
                                    .try_send(TrayAction::ToggleAlwaysOnTop(top_account.clone()));
                            }),
                            ..Default::default()
                        }
                        .into(),
                        StandardItem {
                            label: if window.mini_mode {
                                "✓ Mini mode".to_string()
                            } else {
                                "Mini mode".to_string()
                            },
                            icon_name: "window-restore".to_string(),
                            activate: Box::new(move |tray: &mut Self| {
                                let _ = tray
                                    .tx
                                    .try_send(TrayAction::ToggleMiniMode(mini_account.clone()));
                            }),
                            ..Default::default()
                        }
                        .into(),
                    ],
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: t!("Add account…").to_string(),
                icon_name: "list-add".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::AddAccount);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: t!("Downloads…").to_string(),
                icon_name: "folder-download".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Downloads);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(self.dnd_menu().into());
        items.push(
            StandardItem {
                label: t!("Settings…").to_string(),
                icon_name: "preferences-system".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Settings);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: t!("Quit OxideZap").to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

impl OxideTray {
    fn dnd_menu(&self) -> SubMenu<Self> {
        let label = if self.dnd.active {
            tr!("Do not disturb: {status}", status = self.dnd.status)
        } else {
            t!("Do not disturb").to_string()
        };
        let wake = if self.dnd.wake_time.is_empty() {
            "07:00"
        } else {
            self.dnd.wake_time.as_str()
        };
        let schedule_label = if self.dnd.follows_schedule {
            format!("✓ {}", t!("Follow quiet hours"))
        } else {
            t!("Follow quiet hours").to_string()
        };
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        items.push(
            StandardItem {
                label: t!("Off").to_string(),
                icon_name: "notifications-disabled".to_string(),
                enabled: self.dnd.active,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbOff);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: t!("On for 1 hour").to_string(),
                icon_name: "notifications-disabled".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbOneHour);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: tr!("Until tomorrow ({wake})", wake = wake),
                icon_name: "notifications-disabled".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbTomorrow);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: t!("Until I turn it off").to_string(),
                icon_name: "notifications-disabled".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbIndefinite);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: schedule_label,
                icon_name: "alarm-symbolic".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbSchedule);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: t!("Skip this quiet period").to_string(),
                icon_name: "alarm-symbolic".to_string(),
                enabled: self.dnd.schedule_active,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::DoNotDisturbSkip);
                }),
                ..Default::default()
            }
            .into(),
        );
        SubMenu {
            label,
            icon_name: "notifications-disabled".to_string(),
            submenu: items,
            ..Default::default()
        }
    }
}

pub struct TrayHandle {
    handle: ksni::blocking::Handle<OxideTray>,
}

impl TrayHandle {
    pub fn start(tx: Sender<TrayAction>) -> Option<Self> {
        use ksni::blocking::TrayMethods;
        let tray = OxideTray::new(tx);
        match tray.spawn() {
            Ok(handle) => {
                info!("system tray started");
                Some(Self { handle })
            }
            Err(error) => {
                warn!(%error, "could not start the system tray");
                None
            }
        }
    }

    pub fn update(
        &self,
        accounts: &[Account],
        unread: u32,
        visible: bool,
        dnd: TrayDndState,
        suspended: &HashSet<String>,
        window: Option<TrayWindowState>,
    ) {
        let accounts = accounts
            .iter()
            .map(|account| TrayAccount {
                id: account.id.clone(),
                name: account.name.clone(),
                unread: account.unread,
                suspended: suspended.contains(&account.id),
                accent: account.accent_color(),
            })
            .collect();
        let active = dnd.active;
        let _ = self.handle.update(move |tray| {
            tray.accounts = accounts;
            tray.unread = unread;
            tray.visible = visible;
            tray.dnd = dnd;
            tray.window = window;
            tray.icons
                .refresh(TrayIconState::for_unread(unread, active));
        });
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

/// Label for an account entry in the tray submenu. The active account gets a
/// leading checkmark, and the unread count and suspended marker keep the same
/// shape as before so the sheet stays easy to scan.
fn account_menu_label(account: &TrayAccount, active: bool) -> String {
    let mut label = if active {
        format!("✓ {}", account.name)
    } else {
        account.name.clone()
    };
    if account.unread > 0 {
        label.push_str(&format!(" ({})", account.unread));
    }
    if account.suspended {
        label.push_str(&format!(" ({})", t!("suspended")));
    }
    label
}

/// Renders a small PNG disc in the account accent for the tray submenu. The
/// DBus menu protocol carries menu icons as PNG bytes, so no file is written
/// and an invalid or missing accent simply keeps the themed user icon.
fn account_icon_data(accent: AccountColor) -> Vec<u8> {
    let mut image = RgbaImage::new(ACCOUNT_ICON_SIZE, ACCOUNT_ICON_SIZE);
    let center = (ACCOUNT_ICON_SIZE as i32 - 1) as f32 / 2.0;
    let radius = ACCOUNT_ICON_SIZE as f32 / 2.0 - 1.0;
    let radius_squared = radius * radius;
    let [red, green, blue] = accent.channels();
    for y in 0..ACCOUNT_ICON_SIZE as i32 {
        for x in 0..ACCOUNT_ICON_SIZE as i32 {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius_squared {
                image.put_pixel(x as u32, y as u32, Rgba([red, green, blue, 255]));
            }
        }
    }
    let mut bytes = Vec::new();
    let mut cursor = Cursor::new(&mut bytes);
    if let Err(error) = image.write_to(&mut cursor, ImageFormat::Png) {
        warn!(%error, "could not encode a tray account icon");
        return Vec::new();
    }
    bytes
}

fn tooltip_description(accounts: &[TrayAccount], unread: u32, dnd_status: Option<&str>) -> String {
    let mut lines = Vec::new();
    if let Some(status) = dnd_status {
        lines.push(status.to_string());
    }
    if unread == 0 {
        lines.push(if dnd_status.is_some() {
            t!("No unread messages").to_string()
        } else {
            t!("WhatsApp Web for Linux").to_string()
        });
    } else {
        lines.push(tn!(
            unread,
            "{count} unread message",
            "{count} unread messages"
        ));
        for account in accounts.iter().filter(|account| account.unread > 0) {
            lines.push(format!("{}: {}", account.name, account.unread));
        }
    }
    let suspended = accounts.iter().filter(|account| account.suspended).count();
    if suspended > 0 {
        lines.push(tn!(
            suspended as u32,
            "{count} suspended account",
            "{count} suspended accounts"
        ));
    }
    lines.join("\n")
}

/// Keeps the generated pixmaps while the visual state is unchanged, so a burst
/// of title updates with the same unread count never re-renders the badge.
struct IconCache {
    base: Option<RgbaImage>,
    state: Option<TrayIconState>,
    icons: Vec<Icon>,
}

impl IconCache {
    fn load() -> Self {
        Self::new(load_base_icon())
    }

    fn new(base: Option<RgbaImage>) -> Self {
        Self {
            base,
            state: None,
            icons: Vec::new(),
        }
    }

    fn refresh(&mut self, state: TrayIconState) -> bool {
        if self.state == Some(state) {
            return false;
        }
        self.state = Some(state);
        self.icons = self
            .base
            .as_ref()
            .map(|base| vec![to_icon(&render_icon_image(base, state))])
            .unwrap_or_default();
        true
    }

    fn icons(&self) -> Vec<Icon> {
        self.icons.clone()
    }
}

fn load_base_icon() -> Option<RgbaImage> {
    let bytes = include_bytes!("../assets/oxidezap-64.png");
    match image::load_from_memory(bytes) {
        Ok(image) => Some(image.to_rgba8()),
        Err(_) => {
            warn!("could not decode the tray icon");
            None
        }
    }
}

fn render_icon_image(base: &RgbaImage, state: TrayIconState) -> RgbaImage {
    let mut image = base.clone();
    match state {
        TrayIconState::Normal => {}
        TrayIconState::Unread(count) => {
            draw_badge(&mut image, BADGE_UNREAD);
            draw_digit(&mut image, count);
        }
        TrayIconState::UnreadOverflow => {
            draw_badge(&mut image, BADGE_UNREAD);
            draw_overflow(&mut image);
        }
        TrayIconState::DoNotDisturb => {
            dim_image(&mut image);
            draw_badge(&mut image, BADGE_DND);
            draw_dnd_bar(&mut image);
        }
    }
    image
}

fn to_icon(image: &RgbaImage) -> Icon {
    let (width, height) = image.dimensions();
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for pixel in image.pixels() {
        // StatusNotifierItem expects ARGB32 in network byte order.
        data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
    }
    Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

fn draw_badge(image: &mut RgbaImage, fill: [u8; 4]) {
    fill_circle(image, BADGE_OUTER_RADIUS, BADGE_OUTLINE);
    fill_circle(image, BADGE_RING_RADIUS, BADGE_RING);
    fill_circle(image, BADGE_INNER_RADIUS, fill);
}

fn fill_circle(image: &mut RgbaImage, radius: i32, color: [u8; 4]) {
    let radius_squared = radius * radius;
    for y in (BADGE_CENTER - radius)..=(BADGE_CENTER + radius) {
        for x in (BADGE_CENTER - radius)..=(BADGE_CENTER + radius) {
            let dx = x - BADGE_CENTER;
            let dy = y - BADGE_CENTER;
            if dx * dx + dy * dy <= radius_squared {
                put_pixel(image, x, y, color);
            }
        }
    }
}

fn draw_digit(image: &mut RgbaImage, digit: u32) {
    let digit = digit.clamp(1, 9) as usize;
    draw_glyph(
        image,
        &DIGIT_GLYPHS[digit],
        BADGE_CENTER - GLYPH_WIDTH / 2,
        BADGE_CENTER - GLYPH_HEIGHT / 2,
    );
}

fn draw_overflow(image: &mut RgbaImage) {
    let total_width = GLYPH_WIDTH * 2 + GLYPH_GAP;
    let x = BADGE_CENTER - total_width / 2;
    let y = BADGE_CENTER - GLYPH_HEIGHT / 2;
    draw_glyph(image, &DIGIT_GLYPHS[9], x, y);
    draw_glyph(image, &PLUS_GLYPH, x + GLYPH_WIDTH + GLYPH_GAP, y);
}

fn draw_glyph(image: &mut RgbaImage, rows: &[&str; 5], origin_x: i32, origin_y: i32) {
    for (row, pattern) in rows.iter().enumerate() {
        for (column, cell) in pattern.chars().enumerate() {
            if cell == '1' {
                fill_rect(
                    image,
                    origin_x + column as i32 * GLYPH_SCALE,
                    origin_y + row as i32 * GLYPH_SCALE,
                    GLYPH_SCALE,
                    GLYPH_SCALE,
                    GLYPH_COLOR,
                );
            }
        }
    }
}

fn draw_dnd_bar(image: &mut RgbaImage) {
    fill_rect(
        image,
        BADGE_CENTER - 8,
        BADGE_CENTER - 2,
        16,
        4,
        GLYPH_COLOR,
    );
}

fn fill_rect(image: &mut RgbaImage, x: i32, y: i32, width: i32, height: i32, color: [u8; 4]) {
    for offset_y in 0..height {
        for offset_x in 0..width {
            put_pixel(image, x + offset_x, y + offset_y, color);
        }
    }
}

fn put_pixel(image: &mut RgbaImage, x: i32, y: i32, color: [u8; 4]) {
    if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
        image.put_pixel(x as u32, y as u32, Rgba(color));
    }
}

fn dim_image(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        if pixel[3] == 0 {
            continue;
        }
        let gray =
            (u32::from(pixel[0]) * 299 + u32::from(pixel[1]) * 587 + u32::from(pixel[2]) * 114)
                / 1000;
        let dimmed = (gray * 55 / 100) as u8;
        *pixel = Rgba([dimmed, dimmed, dimmed, pixel[3]]);
    }
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;

    fn synthetic_base() -> RgbaImage {
        RgbaImage::from_pixel(64, 64, Rgba([200, 50, 80, 255]))
    }

    fn accounts() -> Vec<TrayAccount> {
        vec![
            TrayAccount {
                id: "a".to_string(),
                name: "Work".to_string(),
                unread: 3,
                suspended: false,
                accent: Some(AccountColor::new(0x1d, 0x4e, 0xd8)),
            },
            TrayAccount {
                id: "b".to_string(),
                name: "Personal".to_string(),
                unread: 2,
                suspended: false,
                accent: None,
            },
            TrayAccount {
                id: "c".to_string(),
                name: "Empty".to_string(),
                unread: 0,
                suspended: false,
                accent: Some(AccountColor::new(0xbe, 0x18, 0x5d)),
            },
        ]
    }

    #[test]
    fn icon_state_covers_the_unread_boundaries() {
        assert_eq!(TrayIconState::for_unread(0, false), TrayIconState::Normal);
        assert_eq!(
            TrayIconState::for_unread(1, false),
            TrayIconState::Unread(1)
        );
        assert_eq!(
            TrayIconState::for_unread(9, false),
            TrayIconState::Unread(9)
        );
        assert_eq!(
            TrayIconState::for_unread(10, false),
            TrayIconState::UnreadOverflow
        );
        assert_eq!(
            TrayIconState::for_unread(u32::MAX, false),
            TrayIconState::UnreadOverflow
        );
    }

    #[test]
    fn do_not_disturb_takes_priority_over_unread() {
        assert_eq!(
            TrayIconState::for_unread(0, true),
            TrayIconState::DoNotDisturb
        );
        assert_eq!(
            TrayIconState::for_unread(7, true),
            TrayIconState::DoNotDisturb
        );
    }

    #[test]
    fn unread_and_dnd_icons_differ_from_the_normal_icon() {
        let base = synthetic_base();
        let normal = render_icon_image(&base, TrayIconState::Normal);
        let one = render_icon_image(&base, TrayIconState::Unread(1));
        let nine = render_icon_image(&base, TrayIconState::Unread(9));
        let overflow = render_icon_image(&base, TrayIconState::UnreadOverflow);
        let dnd = render_icon_image(&base, TrayIconState::DoNotDisturb);

        assert_eq!(normal, base);
        assert_ne!(one, normal);
        assert_ne!(nine, one);
        assert_ne!(overflow, nine);
        assert_ne!(dnd, normal);
        assert_ne!(dnd, nine);
    }

    #[test]
    fn icon_cache_regenerates_only_when_the_icon_state_changes() {
        let mut cache = IconCache::new(Some(synthetic_base()));

        assert!(cache.refresh(TrayIconState::Normal));
        assert!(!cache.refresh(TrayIconState::Normal));
        assert!(cache.refresh(TrayIconState::Unread(1)));
        assert!(!cache.refresh(TrayIconState::Unread(1)));
        assert!(cache.refresh(TrayIconState::Unread(2)));
        assert!(cache.refresh(TrayIconState::UnreadOverflow));
        assert!(!cache.refresh(TrayIconState::UnreadOverflow));
        assert!(cache.refresh(TrayIconState::DoNotDisturb));
        assert!(!cache.refresh(TrayIconState::DoNotDisturb));
    }

    #[test]
    fn tooltip_lists_the_unread_breakdown_per_account() {
        let accounts = accounts();

        assert_eq!(
            tooltip_description(&accounts, 5, None),
            "5 unread messages\nWork: 3\nPersonal: 2"
        );
        assert_eq!(
            tooltip_description(&accounts, 1, None),
            "1 unread message\nWork: 3\nPersonal: 2"
        );
        assert_eq!(
            tooltip_description(&accounts, 0, None),
            "WhatsApp Web for Linux"
        );
    }

    #[test]
    fn tooltip_marks_do_not_disturb_and_shows_the_remaining_time() {
        let accounts = accounts();
        let status = TrayDndState {
            active: true,
            status: "Quiet hours until 07:00 (6h 20m left)".to_string(),
            follows_schedule: true,
            schedule_active: true,
            wake_time: "07:00".to_string(),
        };
        let line = status.tooltip_line().expect("an active DND line");

        assert_eq!(
            tooltip_description(&accounts, 5, Some(&line)),
            "Do not disturb: Quiet hours until 07:00 (6h 20m left)\n5 unread messages\nWork: 3\nPersonal: 2"
        );
        assert_eq!(
            tooltip_description(&accounts, 0, Some(&line)),
            "Do not disturb: Quiet hours until 07:00 (6h 20m left)\nNo unread messages"
        );
        assert!(TrayDndState::default().tooltip_line().is_none());
    }

    #[test]
    fn tooltip_reports_suspended_accounts() {
        let mut accounts = accounts();
        accounts[0].suspended = true;
        accounts[2].suspended = true;

        assert_eq!(
            tooltip_description(&accounts, 3, None),
            "3 unread messages\nWork: 3\nPersonal: 2\n2 suspended accounts"
        );
        assert_eq!(
            tooltip_description(&accounts, 0, None),
            "WhatsApp Web for Linux\n2 suspended accounts"
        );
    }

    #[test]
    fn account_menu_labels_mark_the_active_account() {
        let accounts = accounts();

        assert_eq!(account_menu_label(&accounts[0], false), "Work (3)");
        assert_eq!(account_menu_label(&accounts[0], true), "✓ Work (3)");
        assert_eq!(account_menu_label(&accounts[2], false), "Empty");
    }

    #[test]
    fn account_icons_are_png_images_that_differ_by_accent() {
        let blue = AccountColor::new(0x1d, 0x4e, 0xd8);
        let red = AccountColor::new(0xb9, 0x1c, 0x1c);
        let blue_icon = account_icon_data(blue);
        let red_icon = account_icon_data(red);

        assert!(blue_icon.starts_with(&[0x89, b'P', b'N', b'G']));
        assert_ne!(blue_icon, red_icon);
        let decoded = image::load_from_memory(&blue_icon)
            .expect("the tray icon is a valid PNG")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (ACCOUNT_ICON_SIZE, ACCOUNT_ICON_SIZE));
        assert_eq!(
            decoded.get_pixel(ACCOUNT_ICON_SIZE / 2, ACCOUNT_ICON_SIZE / 2)[3],
            255
        );
    }
}
