use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossbeam_channel::Receiver;
use gpui::{
    App, Bounds, Context, Entity, SharedString, Timer, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    WindowOptions, point, prelude::*, px, size,
};
use tracing::{debug, error, info, warn};

use crate::autostart;
use crate::badge::Badge;
use crate::hotkey::{HotkeyAction, HotkeyHandle};
use crate::links;
use crate::lock;
use crate::notify;
use crate::paths::{APP_ID, Paths};
use crate::settings::{Account, SavedBounds, Settings};
use crate::shortcut::Shortcut;
use crate::single_instance::{InstanceListener, InstanceMessage};
use crate::theme::Theme;
use crate::tray::{TrayAction, TrayHandle};
use crate::ui::account_window::AccountWindow;
use crate::ui::lock_window::LockWindow;
use crate::ui::settings_window::SettingsWindow;
use crate::webview::{
    GtkCommand, GtkEvent, GtkHandle, LoadState, OVERLAY_MARGIN, TITLEBAR_HEIGHT, WHATSAPP_URL,
};
use crate::x11::X11;

pub trait AppModelEntityExt {
    fn update_settings(&self, cx: &mut App, update: impl FnOnce(&mut Settings));
}

impl AppModelEntityExt for Entity<AppModel> {
    fn update_settings(&self, cx: &mut App, update: impl FnOnce(&mut Settings)) {
        self.update(cx, |model, cx| model.update_settings(cx, update));
    }
}

pub struct SmokeState {
    pub account_id: String,
    pub started: Instant,
    pub gtk_xid: Option<u32>,
    pub loaded_at: Option<Instant>,
    pub verified: bool,
}

pub struct AppModel {
    pub paths: Paths,
    pub settings: Settings,
    pub gtk: GtkHandle,
    pub x11: X11,
    pub locked: bool,
    pub quitting: bool,
    gtk_events: Receiver<GtkEvent>,
    instance_messages: Receiver<InstanceMessage>,
    hotkey_actions: Receiver<HotkeyAction>,
    tray_actions: Receiver<TrayAction>,
    tray: Option<TrayHandle>,
    hotkey: HotkeyHandle,
    badge: Badge,
    instance_listener: Option<InstanceListener>,
    account_windows: HashMap<String, WindowHandle<AccountWindow>>,
    account_xids: HashMap<String, u32>,
    scales: HashMap<String, f32>,
    settings_window: Option<WindowHandle<SettingsWindow>>,
    lock_window: Option<WindowHandle<LockWindow>>,
    smoke: Option<SmokeState>,
    startup_html: Option<String>,
    startup_url: Option<String>,
    start_hidden: bool,
    pending_accounts: HashSet<String>,
    pending_settings: bool,
    pending_lock: bool,
    last_idle_check: Instant,
    last_scale_check: Instant,
    last_notifications: Vec<(String, Instant)>,
}

impl AppModel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut Context<Self>,
        paths: Paths,
        settings: Settings,
        gtk: GtkHandle,
        gtk_events: Receiver<GtkEvent>,
        x11: X11,
        tray: Option<TrayHandle>,
        tray_actions: Receiver<TrayAction>,
        hotkey: HotkeyHandle,
        hotkey_actions: Receiver<HotkeyAction>,
        instance_listener: Option<InstanceListener>,
        instance_messages: Receiver<InstanceMessage>,
        badge: Badge,
        smoke: Option<SmokeState>,
        startup_html: Option<String>,
        startup_url: Option<String>,
        start_hidden: bool,
    ) -> Self {
        let _ = cx;
        Self {
            paths,
            settings,
            gtk,
            x11,
            locked: false,
            quitting: false,
            gtk_events,
            instance_messages,
            hotkey_actions,
            tray_actions,
            tray,
            hotkey,
            badge,
            instance_listener,
            account_windows: HashMap::new(),
            account_xids: HashMap::new(),
            scales: HashMap::new(),
            settings_window: None,
            lock_window: None,
            smoke,
            startup_html,
            startup_url,
            start_hidden,
            pending_accounts: HashSet::new(),
            pending_settings: false,
            pending_lock: false,
            last_idle_check: Instant::now(),
            last_scale_check: Instant::now(),
            last_notifications: Vec::new(),
        }
    }

    pub fn start_pump(&mut self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if weak.update(cx, |model, cx| model.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(smoke) = &self.smoke {
            let account_id = smoke.account_id.clone();
            self.open_account_window(&account_id, cx);
            return;
        }

        if let Some(account_id) = self.settings.active_account_id() {
            self.open_account_window(&account_id, cx);
        }
        if self.settings.lock.enabled
            && self.settings.lock.lock_on_launch
            && self.settings.lock.hash.is_some()
        {
            self.lock(cx);
        }
        if self.start_hidden {
            self.hide_all(cx);
        }
        cx.activate(true);
    }

    pub fn theme(&self, window: &Window) -> Theme {
        Theme::resolve(self.settings.theme, window.appearance())
    }

    pub fn account_name(&self, id: &str) -> String {
        self.settings
            .account(id)
            .map(|account| account.name.clone())
            .unwrap_or_else(|| "Account".to_string())
    }

    pub fn account_unread(&self, id: &str) -> u32 {
        self.settings
            .account(id)
            .map(|account| account.unread)
            .unwrap_or(0)
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        while let Ok(event) = self.gtk_events.try_recv() {
            self.handle_gtk_event(event, cx);
        }
        while let Ok(message) = self.instance_messages.try_recv() {
            self.handle_instance_message(message, cx);
        }
        while let Ok(action) = self.hotkey_actions.try_recv() {
            if action == HotkeyAction::Toggle {
                self.toggle_visibility(cx);
            }
        }
        while let Ok(action) = self.tray_actions.try_recv() {
            self.handle_tray_action(action, cx);
        }

        if self.settings.lock.enabled
            && self.settings.lock.idle_minutes > 0
            && !self.locked
            && self.last_idle_check.elapsed() >= Duration::from_secs(5)
        {
            self.last_idle_check = Instant::now();
            if let Some(idle) = self.x11.idle_ms() {
                let threshold = u64::from(self.settings.lock.idle_minutes) * 60 * 1000;
                if u64::from(idle) >= threshold {
                    self.lock(cx);
                }
            }
        }

        if self.last_scale_check.elapsed() >= Duration::from_secs(1) {
            self.last_scale_check = Instant::now();
            let windows: Vec<(String, WindowHandle<AccountWindow>)> = self
                .account_windows
                .iter()
                .map(|(id, handle)| (id.clone(), *handle))
                .collect();
            for (id, handle) in windows {
                match handle.update(cx, |_, window, _| window.scale_factor()) {
                    Ok(scale) => {
                        let old = self.scales.get(&id).copied().unwrap_or(1.0);
                        if (old - scale).abs() > 0.01 {
                            self.scales.insert(id.clone(), scale);
                            self.gtk.set_scale(&id, scale);
                        }
                    }
                    Err(_) => {
                        self.account_windows.remove(&id);
                    }
                }
            }
        }

        if self.smoke.is_some() {
            self.poll_smoke(cx);
        }
    }

    fn handle_gtk_event(&mut self, event: GtkEvent, cx: &mut Context<Self>) {
        match event {
            GtkEvent::Ready { account_id, xid } => {
                if let Some(smoke) = &mut self.smoke {
                    if smoke.account_id == account_id {
                        smoke.gtk_xid = Some(xid);
                    }
                }
            }
            GtkEvent::LoadState { account_id, state } => {
                if state == LoadState::Finished {
                    self.gtk.set_zoom(&account_id, self.settings.zoom);
                    if let Some(smoke) = &mut self.smoke {
                        if smoke.account_id == account_id && smoke.loaded_at.is_none() {
                            smoke.loaded_at = Some(Instant::now());
                        }
                    }
                }
            }
            GtkEvent::Title {
                account_id,
                title,
                unread,
            } => {
                debug!(%account_id, %title, unread, "webview title changed");
                if self.settings.set_unread(&account_id, unread) {
                    self.save_settings();
                    self.update_account_title(&account_id, cx);
                    self.update_tray();
                }
            }
            GtkEvent::Notification {
                account_id,
                title,
                body,
            } => {
                debug!(%account_id, %title, "webview notification");
                if self.settings.notifications && !self.locked && self.should_notify(&title, &body)
                {
                    notify::notify(&title, &body, Some(&self.paths.runtime_icon()));
                }
            }
            GtkEvent::DownloadStarted {
                account_id,
                filename,
            } => {
                debug!(%account_id, %filename, "download started");
                if self.settings.notifications {
                    notify::notify(
                        "Downloading",
                        &format!(
                            "{filename}\nSaving to {}",
                            self.paths.downloads_dir.display()
                        ),
                        Some(&self.paths.runtime_icon()),
                    );
                }
            }
            GtkEvent::DownloadFinished {
                account_id,
                filename,
                path,
                success,
            } => {
                debug!(%account_id, %filename, path = %path.display(), success, "download finished");
                if self.settings.notifications {
                    if success {
                        notify::notify_download(&filename, &path, Some(&self.paths.runtime_icon()));
                    } else {
                        notify::notify(
                            "Download failed",
                            &filename,
                            Some(&self.paths.runtime_icon()),
                        );
                    }
                }
            }
            GtkEvent::OpenExternal { url } => {
                links::open_external(&url);
            }
            GtkEvent::WhatsAppUrl { account_id, url } => {
                self.open_url_in_account(&account_id, &url, cx);
            }
            GtkEvent::Crashed { account_id } => {
                warn!(account_id = %account_id, "the WebKit web process crashed");
                notify::notify(
                    "WhatsApp Web crashed",
                    "Reloading the page…",
                    Some(&self.paths.runtime_icon()),
                );
                self.gtk.reload(&account_id);
            }
            GtkEvent::Error {
                account_id,
                message,
            } => {
                error!(account_id = %account_id, message = %message, "WebKit error");
                notify::notify(
                    "WhatsApp Web could not start",
                    &message,
                    Some(&self.paths.runtime_icon()),
                );
            }
        }
    }

    fn handle_instance_message(&mut self, message: InstanceMessage, cx: &mut Context<Self>) {
        match message {
            InstanceMessage::Show => self.show_all(cx),
            InstanceMessage::Toggle => self.toggle_visibility(cx),
            InstanceMessage::Lock => self.lock(cx),
            InstanceMessage::Quit => self.quit(cx),
            InstanceMessage::OpenUrl(url) => self.handle_url(&url, cx),
        }
    }

    fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::ShowHide => self.toggle_visibility(cx),
            TrayAction::OpenAccount(id) => self.open_account_window(&id, cx),
            TrayAction::AddAccount => self.add_account(cx),
            TrayAction::Settings => self.open_settings_window(cx),
            TrayAction::Lock => self.lock(cx),
            TrayAction::Quit => self.quit(cx),
        }
    }

    fn handle_url(&mut self, url: &str, cx: &mut Context<Self>) {
        if url.starts_with("whatsapp://") {
            if let Some(web) = links::whatsapp_to_web(url) {
                if let Some(account_id) = self.settings.active_account_id() {
                    self.open_url_in_account(&account_id, &web, cx);
                    return;
                }
            }
            links::open_external(url);
            return;
        }

        if let Ok(parsed) = url::Url::parse(url) {
            let is_web = matches!(parsed.scheme(), "http" | "https");
            if is_web && links::is_whatsapp_host(parsed.host_str().unwrap_or_default()) {
                if let Some(account_id) = self.settings.active_account_id() {
                    self.open_url_in_account(&account_id, url, cx);
                    return;
                }
            }
        }
        links::open_external(url);
    }

    pub fn open_account_window(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                self.show_account_window(account_id, cx);
                return;
            }
            self.account_windows.remove(account_id);
        }
        if !self.pending_accounts.insert(account_id.to_string()) {
            return;
        }
        let model = cx.entity().clone();
        let account_id = account_id.to_string();
        cx.defer(move |cx| open_account_window_deferred(model, account_id, cx));
    }

    fn finish_open_account(
        &mut self,
        account: Account,
        handle: WindowHandle<AccountWindow>,
        html: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.account_windows.insert(account.id.clone(), handle);
        let scale = handle
            .update(cx, |_, window, _| window.scale_factor())
            .unwrap_or(1.0);
        self.scales.insert(account.id.clone(), scale);

        match self.find_and_tag_account(&account.id) {
            Ok(xid) => {
                self.account_xids.insert(account.id.clone(), xid);
            }
            Err(error) => {
                warn!(account_id = %account.id, %error, "could not tag the GPUI window");
            }
        }

        self.startup_html = None;
        let startup_url = self.startup_url.take();
        let url = match startup_url.as_deref().map(links::classify) {
            Some(links::LinkAction::WhatsApp(web)) => web,
            Some(links::LinkAction::Allow) => startup_url.clone().unwrap_or_default(),
            Some(links::LinkAction::External(url)) => {
                links::open_external(&url);
                WHATSAPP_URL.to_string()
            }
            None => WHATSAPP_URL.to_string(),
        };
        self.gtk.send(GtkCommand::Create {
            account_id: account.id.clone(),
            data_dir: self.paths.account_data_dir(&account.id),
            cache_dir: self.paths.account_cache_dir(&account.id),
            url,
            html,
            show: !self.start_hidden,
            scale,
            zoom: self.settings.zoom,
        });

        self.settings.active_account = Some(account.id.clone());
        self.save_settings();
        self.update_tray();

        let _ = handle.update(cx, |_, window, _| window.activate_window());
        if self.start_hidden {
            self.hide_account_window(&account.id, cx);
        }
    }

    pub fn open_settings_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.settings_window = None;
        }
        if self.pending_settings {
            return;
        }
        self.pending_settings = true;
        let model = cx.entity().clone();
        cx.defer(move |cx| open_settings_window_deferred(model, cx));
    }

    fn open_lock_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.lock_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.lock_window = None;
        }
        if self.pending_lock {
            return;
        }
        self.pending_lock = true;
        let model = cx.entity().clone();
        cx.defer(move |cx| open_lock_window_deferred(model, cx));
    }

    pub fn lock(&mut self, cx: &mut Context<Self>) {
        if !self.settings.lock.enabled || self.settings.lock.hash.is_none() || self.locked {
            return;
        }
        self.locked = true;
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.open_lock_window(cx);
        self.update_tray();
    }

    pub fn try_unlock(&mut self, password: &str, cx: &mut Context<Self>) -> bool {
        let Some(hash) = &self.settings.lock.hash else {
            return false;
        };
        if !lock::verify_password(hash, password) {
            return false;
        }
        self.locked = false;
        if let Some(handle) = self.lock_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.show_all(cx);
        self.update_tray();
        true
    }

    pub fn toggle_visibility(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if self.any_account_visible() {
            self.hide_all(cx);
        } else {
            self.show_all(cx);
        }
    }

    pub fn hide_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.update_tray();
    }

    pub fn show_all(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.show_account_window(&id, cx);
        }
        self.update_tray();
    }

    fn hide_account_window(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            if let Err(error) = self.x11.unmap(xid) {
                warn!(%error, "could not hide the GPUI window");
            }
        }
        self.gtk.hide(account_id);
        if self.settings.lock.enabled
            && self.settings.lock.lock_on_hide
            && self.settings.lock.hash.is_some()
            && !self.locked
        {
            self.lock(cx);
        }
    }

    fn show_account_window(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            let _ = self.x11.map(xid);
            let _ = self.x11.activate(xid);
        }
        self.gtk.show(account_id);
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        self.update_tray();
    }

    fn any_account_visible(&self) -> bool {
        self.account_xids
            .values()
            .any(|xid| self.x11.is_viewable(*xid).unwrap_or(false))
    }

    pub fn on_account_close_requested(
        &mut self,
        account_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quitting {
            window.remove_window();
            return;
        }
        self.close_account_window(account_id, window, cx);
    }

    pub fn close_account_window(
        &mut self,
        account_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = window.bounds();
        let maximized = window.is_maximized();
        if let Some(account) = self.settings.account_mut(account_id) {
            account.bounds = Some(SavedBounds {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
            });
            account.maximized = maximized;
        }
        self.save_settings();

        if self.settings.close_to_tray {
            self.hide_account_window(account_id, cx);
            return;
        }

        self.account_windows.remove(account_id);
        self.account_xids.remove(account_id);
        self.scales.remove(account_id);
        self.gtk.close(account_id);
        window.remove_window();
        if self.account_windows.is_empty() {
            self.quit(cx);
        }
    }

    pub fn add_account(&mut self, cx: &mut Context<Self>) {
        let name = format!("Account {}", self.settings.accounts.len() + 1);
        let id = self.settings.add_account(name);
        self.save_settings();
        self.open_account_window(&id, cx);
        self.update_tray();
    }

    pub fn rename_account(&mut self, account_id: &str, name: &str, cx: &mut Context<Self>) {
        if self.settings.rename_account(account_id, name) {
            self.save_settings();
            self.update_account_title(account_id, cx);
            self.update_tray();
            cx.refresh_windows();
        }
    }

    pub fn remove_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.settings.accounts.len() <= 1 {
            return;
        }
        if let Some(handle) = self.account_windows.remove(account_id) {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.account_xids.remove(account_id);
        self.scales.remove(account_id);
        self.gtk.close(account_id);
        self.settings.remove_account(account_id);
        self.save_settings();
        self.update_tray();
        cx.refresh_windows();

        let directory = self.paths.account_dir(account_id);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            if let Err(error) = std::fs::remove_dir_all(&directory) {
                warn!(path = %directory.display(), %error, "could not remove account data");
            }
        });
    }

    pub fn set_theme(&mut self, theme: crate::settings::ThemeMode, cx: &mut Context<Self>) {
        if self.settings.set_theme(theme) {
            self.apply_settings(cx);
        }
    }

    pub fn set_zoom(&mut self, zoom: f64, cx: &mut Context<Self>) {
        if self.settings.set_zoom(zoom) {
            self.apply_settings(cx);
        }
    }

    pub fn set_shortcut(&mut self, shortcut: Shortcut, cx: &mut Context<Self>) {
        self.settings.shortcut = shortcut.to_string();
        self.hotkey.set(shortcut);
        self.apply_settings(cx);
    }

    pub fn set_password(&mut self, password: &str, cx: &mut Context<Self>) -> Result<(), String> {
        lock::validate_password(password)?;
        let hash = lock::hash_password(password).map_err(|error| error.to_string())?;
        self.settings.lock.hash = Some(hash);
        self.settings.lock.enabled = true;
        self.apply_settings(cx);
        Ok(())
    }

    pub fn remove_password(&mut self, cx: &mut Context<Self>) {
        self.settings.lock.hash = None;
        self.settings.lock.enabled = false;
        self.settings.lock.lock_on_launch = false;
        self.settings.lock.lock_on_hide = false;
        self.settings.lock.idle_minutes = 0;
        self.apply_settings(cx);
    }

    pub fn update_settings(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut Settings)) {
        update(&mut self.settings);
        self.settings.normalize();
        self.apply_settings(cx);
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        self.save_settings();
        if let Err(error) = autostart::apply(self.settings.start_at_login, &self.paths) {
            warn!(%error, "could not update the autostart entry");
        }
        for account_id in self
            .settings
            .accounts
            .iter()
            .map(|account| account.id.clone())
        {
            self.gtk.set_zoom(&account_id, self.settings.zoom);
        }
        if let Ok(shortcut) = Shortcut::parse(&self.settings.shortcut) {
            self.hotkey.set(shortcut);
        }
        self.update_tray();
        cx.refresh_windows();
    }

    fn save_settings(&self) {
        if let Err(error) = self.settings.save(&self.paths.settings_file) {
            warn!(%error, "could not save settings");
        }
    }

    fn update_account_title(&self, account_id: &str, cx: &mut Context<Self>) {
        let name = self.account_name(account_id);
        let unread = self.account_unread(account_id);
        let title = if unread > 0 {
            format!("OxideZap — {name} ({unread})")
        } else {
            format!("OxideZap — {name}")
        };
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, window, _| window.set_window_title(&title));
        }
    }

    fn update_tray(&self) {
        let unread = self.settings.total_unread();
        let visible = self.any_account_visible();
        if let Some(tray) = &self.tray {
            tray.update(&self.settings.accounts, unread, visible);
        }
        self.badge.set(unread);
    }

    fn should_notify(&mut self, title: &str, body: &str) -> bool {
        let now = Instant::now();
        self.last_notifications
            .retain(|(_, at)| now.duration_since(*at) < Duration::from_secs(5));
        let key = format!("{title}\u{0}{body}");
        if self
            .last_notifications
            .iter()
            .any(|(existing, _)| existing == &key)
        {
            return false;
        }
        self.last_notifications.push((key, now));
        true
    }

    fn find_and_tag_account(&self, account_id: &str) -> Result<u32> {
        if let Some(xid) = self.x11.find_account_window(account_id)? {
            return Ok(xid);
        }
        for _ in 0..40 {
            if let Some(xid) = self.x11.find_untagged_account_window()? {
                self.x11.tag_account(xid, account_id)?;
                return Ok(xid);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(anyhow!("the account window was not found in the X11 tree"))
    }

    fn open_url_in_account(&mut self, account_id: &str, url: &str, cx: &mut Context<Self>) {
        self.show_account_window(account_id, cx);
        self.gtk.load_url(account_id, url);
    }

    pub fn quit(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.quitting = true;

        let windows: Vec<(String, WindowHandle<AccountWindow>)> = self
            .account_windows
            .iter()
            .map(|(id, handle)| (id.clone(), *handle))
            .collect();
        for (id, handle) in windows {
            if let Ok((bounds, maximized)) =
                handle.update(cx, |_, window, _| (window.bounds(), window.is_maximized()))
            {
                if let Some(account) = self.settings.account_mut(&id) {
                    account.bounds = Some(SavedBounds {
                        x: f32::from(bounds.origin.x),
                        y: f32::from(bounds.origin.y),
                        width: f32::from(bounds.size.width),
                        height: f32::from(bounds.size.height),
                    });
                    account.maximized = maximized;
                }
            }
        }
        self.save_settings();
        self.gtk.quit();
        if let Some(tray) = &self.tray {
            tray.shutdown();
        }
        self.hotkey.stop();
        self.instance_listener = None;
        cx.quit();
    }

    fn poll_smoke(&mut self, cx: &mut Context<Self>) {
        let Some(smoke) = &self.smoke else {
            return;
        };
        if smoke.verified {
            return;
        }
        if smoke.started.elapsed() > Duration::from_secs(30) {
            println!("SMOKE FAIL: timed out waiting for the WebKit overlay");
            std::process::exit(1);
        }
        let Some(loaded_at) = smoke.loaded_at else {
            return;
        };
        if loaded_at.elapsed() < Duration::from_millis(1000) {
            return;
        }

        let account_id = smoke.account_id.clone();
        let Some(gtk_xid) = smoke.gtk_xid else {
            println!("SMOKE FAIL: the WebKit window was not created");
            std::process::exit(1);
        };
        let gpui_xid = match self.x11.find_account_window(&account_id) {
            Ok(Some(xid)) => xid,
            _ => {
                println!("SMOKE FAIL: the GPUI window was not tagged");
                std::process::exit(1);
            }
        };
        let gpui_geometry = match self.x11.geometry(gpui_xid) {
            Ok(geometry) => geometry,
            Err(error) => {
                println!("SMOKE FAIL: could not read the GPUI geometry: {error}");
                std::process::exit(1);
            }
        };
        let (gpui_x, gpui_y) = match self.x11.root_position(gpui_xid) {
            Ok(position) => position,
            Err(error) => {
                println!("SMOKE FAIL: could not translate the GPUI position: {error}");
                std::process::exit(1);
            }
        };
        let gtk_geometry = match self.x11.geometry(gtk_xid) {
            Ok(geometry) => geometry,
            Err(error) => {
                println!("SMOKE FAIL: could not read the WebKit geometry: {error}");
                std::process::exit(1);
            }
        };
        let transient = self.x11.transient_for(gtk_xid);
        let viewable = self.x11.is_viewable(gtk_xid).unwrap_or(false);
        let scale = self.scales.get(&account_id).copied().unwrap_or(1.0);
        let titlebar = (TITLEBAR_HEIGHT * scale).round() as i32;
        let margin = (OVERLAY_MARGIN * scale).round().max(1.0) as i32;

        let expected_x = gpui_x + margin;
        let expected_y = gpui_y + titlebar;
        let expected_width = u32::from(gpui_geometry.width).saturating_sub((margin * 2) as u32);
        let expected_height =
            u32::from(gpui_geometry.height).saturating_sub(titlebar as u32 + margin as u32);
        let ok = transient == Some(gpui_xid)
            && viewable
            && i32::from(gtk_geometry.x) == expected_x
            && i32::from(gtk_geometry.y) == expected_y
            && u32::from(gtk_geometry.width) == expected_width
            && u32::from(gtk_geometry.height) == expected_height;

        if ok {
            println!(
                "SMOKE OK: GPUI {}x{}+{}+{}, WebKit {}x{}+{}+{}, transient={}",
                gpui_geometry.width,
                gpui_geometry.height,
                gpui_x,
                gpui_y,
                gtk_geometry.width,
                gtk_geometry.height,
                gtk_geometry.x,
                gtk_geometry.y,
                transient.is_some()
            );
            self.smoke.as_mut().unwrap().verified = true;
            self.quit(cx);
            std::process::exit(0);
        } else {
            println!(
                "SMOKE FAIL: GPUI {}x{}+{}+{}, WebKit {}x{}+{}+{}, transient={:?}, viewable={}, titlebar={}",
                gpui_geometry.width,
                gpui_geometry.height,
                gpui_x,
                gpui_y,
                gtk_geometry.width,
                gtk_geometry.height,
                gtk_geometry.x,
                gtk_geometry.y,
                transient,
                viewable,
                titlebar
            );
            std::process::exit(1);
        }
    }
}

fn open_account_window_deferred(model: Entity<AppModel>, account_id: String, cx: &mut App) {
    let data = {
        let model_ref = model.read(cx);
        model_ref
            .settings
            .account(&account_id)
            .cloned()
            .map(|account| {
                (
                    account,
                    model_ref.settings.remember_bounds,
                    model_ref.startup_html.clone(),
                )
            })
    };
    let Some((account, remember_bounds, html)) = data else {
        model.update(cx, |model, _| {
            model.pending_accounts.remove(&account_id);
        });
        return;
    };

    let window_bounds = if remember_bounds {
        account
            .bounds
            .as_ref()
            .filter(|bounds| bounds.is_valid())
            .map(|bounds| {
                let bounds = Bounds::new(
                    point(px(bounds.x), px(bounds.y)),
                    size(px(bounds.width), px(bounds.height)),
                );
                if account.maximized {
                    WindowBounds::Maximized(bounds)
                } else {
                    WindowBounds::Windowed(bounds)
                }
            })
            .unwrap_or_else(|| WindowBounds::centered(size(px(1180.0), px(780.0)), cx))
    } else {
        WindowBounds::centered(size(px(1180.0), px(780.0)), cx)
    };

    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from(format!("OxideZap — {}", account.name))),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        display_id: None,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some(APP_ID.to_string()),
        window_min_size: Some(size(px(760.0), px(520.0))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
    };

    let model_for_view = model.clone();
    let account_for_view = account.id.clone();
    let model_for_close = model.clone();
    let account_for_close = account.id.clone();
    let handle = match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id(APP_ID);
        let view =
            cx.new(|cx| AccountWindow::new(model_for_view.clone(), account_for_view.clone(), cx));
        let model = model_for_close.clone();
        let account_id = account_for_close.clone();
        window.on_window_should_close(cx, move |window, cx| {
            model.update(cx, |model, cx| {
                model.on_account_close_requested(&account_id, window, cx);
            });
            false
        });
        view
    }) {
        Ok(handle) => handle,
        Err(error) => {
            error!(%error, "could not open an account window");
            let icon = model.read(cx).paths.runtime_icon();
            notify::notify(
                "Could not open the account window",
                &error.to_string(),
                Some(&icon),
            );
            model.update(cx, |model, _| {
                model.pending_accounts.remove(&account_id);
            });
            return;
        }
    };

    model.update(cx, |model, cx| {
        model.pending_accounts.remove(&account_id);
        model.finish_open_account(account, handle, html, cx);
    });
}

fn open_settings_window_deferred(model: Entity<AppModel>, cx: &mut App) {
    info!("opening settings window");
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(920.0), px(660.0)), cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from("OxideZap Settings")),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        display_id: None,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("oxidezap-settings".to_string()),
        window_min_size: Some(size(px(780.0), px(540.0))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
    };

    let model_for_view = model.clone();
    match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id("oxidezap-settings");
        cx.new(|cx| SettingsWindow::new(model_for_view.clone(), cx))
    }) {
        Ok(handle) => {
            info!("settings window created");
            model.update(cx, |model, _| {
                model.pending_settings = false;
                model.settings_window = Some(handle);
            });
        }
        Err(error) => {
            error!(%error, "could not open the settings window");
            model.update(cx, |model, _| model.pending_settings = false);
        }
    }
}

fn open_lock_window_deferred(model: Entity<AppModel>, cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(460.0), px(420.0)), cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from("OxideZap Locked")),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        focus: true,
        show: true,
        kind: WindowKind::Floating,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        display_id: None,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("oxidezap-lock".to_string()),
        window_min_size: Some(size(px(460.0), px(420.0))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
    };

    let model_for_view = model.clone();
    match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id("oxidezap-lock");
        let view = cx.new(|cx| LockWindow::new(model_for_view.clone(), cx));
        let focus = view.read(cx).password_focus(cx);
        window.focus(&focus);
        window.on_window_should_close(cx, |_, _| false);
        view
    }) {
        Ok(handle) => {
            model.update(cx, |model, _| {
                model.pending_lock = false;
                model.lock_window = Some(handle);
            });
        }
        Err(error) => {
            error!(%error, "could not open the lock window");
            model.update(cx, |model, _| model.pending_lock = false);
        }
    }
}
