use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// The download center belongs to the application model. It is declared here
/// with an explicit path so `src/downloads.rs` stays a focused module.
#[path = "downloads.rs"]
pub mod downloads;

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use gpui::{
    App, Bounds, Context, Entity, SharedString, Timer, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    WindowOptions, point, prelude::*, px, size,
};
use tracing::{debug, error, info, warn};
use zbus::MatchRule;
use zbus::blocking::MessageIterator;
use zbus::message::Type as MessageType;

use crate::autostart;
use crate::badge::Badge;
use crate::hotkey::{HotkeyAction, HotkeyBindings, HotkeyHandle};
use crate::links;
use crate::lock;
use crate::notify;
use crate::paths::{APP_ID, Paths};
use crate::settings::storage::{self, StorageOperation, StorageSnapshot};
use crate::settings::{
    Account, LockSettings, ProxySettings, SETTINGS_SAVE_DEBOUNCE, SaveDebounce, SavedBounds,
    Settings, SettingsWriter, WritingSettings,
};
use crate::shortcut::Shortcut;
use crate::single_instance::{
    ChatTarget, DndMode, InstanceListener, InstanceMessage, InstanceReply, InstanceRequest,
};
use crate::theme::Theme;
use crate::tray::{TrayAction, TrayHandle};
use crate::ui::account_window::AccountWindow;
use crate::ui::downloads_window::DownloadsWindow;
use crate::ui::lock_window::LockWindow;
use crate::ui::settings_window::SettingsWindow;
use crate::updates::{self, UpdateState};
use crate::webview::{
    ConnectionState as BrowserConnectionState, GtkCommand, GtkEvent, GtkHandle, LoadState,
    OVERLAY_MARGIN, TITLEBAR_HEIGHT, WHATSAPP_URL,
};
use crate::x11::{WindowInfo, X11, select_account_window};

use self::downloads::{
    DownloadLayout, DownloadManager, DownloadRecord, DownloadStatus, DownloadWriter, now_millis,
};

pub trait AppModelEntityExt {
    fn update_settings(&self, cx: &mut App, update: impl FnOnce(&mut Settings));
}

impl AppModelEntityExt for Entity<AppModel> {
    fn update_settings(&self, cx: &mut App, update: impl FnOnce(&mut Settings)) {
        self.update(cx, |model, cx| model.update_settings(cx, update));
    }
}

const ACCOUNT_TAG_ATTEMPTS: usize = 40;
const ACCOUNT_TAG_RETRY_INTERVAL: Duration = Duration::from_millis(5);

enum AccountTag {
    Tagged,
    Pending,
    Closed,
}

/// Why the application lock is being requested. Manual locks always act;
/// automatic triggers respect their individual Security toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockTrigger {
    Manual,
    Idle,
    Hide,
    Suspend,
    Resume,
    ScreenLocked,
}

/// Session events received from logind and desktop ScreenSaver services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemLockEvent {
    Suspend,
    Resume,
    ScreenLocked,
}

impl SystemLockEvent {
    fn label(self) -> &'static str {
        match self {
            SystemLockEvent::Suspend => "system suspend",
            SystemLockEvent::Resume => "system resume",
            SystemLockEvent::ScreenLocked => "desktop screen lock",
        }
    }

    fn trigger(self) -> LockTrigger {
        match self {
            SystemLockEvent::Suspend => LockTrigger::Suspend,
            SystemLockEvent::Resume => LockTrigger::Resume,
            SystemLockEvent::ScreenLocked => LockTrigger::ScreenLocked,
        }
    }
}

fn lock_is_configured(lock: &LockSettings) -> bool {
    lock.enabled && lock.hash.is_some()
}

fn lock_on_trigger(trigger: LockTrigger, lock: &LockSettings) -> bool {
    if !lock_is_configured(lock) {
        return false;
    }
    match trigger {
        LockTrigger::Manual | LockTrigger::Idle => true,
        LockTrigger::Hide => lock.lock_on_hide,
        LockTrigger::Suspend | LockTrigger::Resume => lock.lock_on_suspend,
        LockTrigger::ScreenLocked => lock.lock_on_screen_lock,
    }
}

/// A lock is only started once. Repeated suspend or ScreenSaver signals while
/// the lock window is already up are intentionally ignored.
fn should_begin_lock(currently_locked: bool, trigger: LockTrigger, lock: &LockSettings) -> bool {
    !currently_locked && lock_on_trigger(trigger, lock)
}

fn unlock_is_authorized(hash: Option<&str>, password: &str) -> bool {
    hash.is_some_and(|hash| lock::verify_password(hash, password))
}

fn hotkey_bindings(toggle: Shortcut, boss_key: &str) -> HotkeyBindings {
    HotkeyBindings::new(toggle, Shortcut::parse(boss_key).ok())
}

fn logind_event(sleeping: bool) -> SystemLockEvent {
    if sleeping {
        SystemLockEvent::Suspend
    } else {
        SystemLockEvent::Resume
    }
}

/// ScreenSaver implementations use different bus names and object paths, but
/// every known interface contains `ScreenSaver`.
fn is_screen_saver_interface(interface: &str) -> bool {
    interface.to_ascii_lowercase().contains("screensaver")
}

/// Signals are rare; a small queue keeps memory bounded if the UI thread is
/// busy while the machine suspends.
const LOCK_EVENT_QUEUE: usize = 8;

/// Starts best-effort session watchers. Every failure is logged and ignored:
/// locking still works manually, on hide, and on idle when those are enabled.
fn start_system_lock_watchers(tx: Sender<SystemLockEvent>) {
    spawn_lock_watcher("oxidezap-logind", tx.clone(), watch_logind);
    spawn_lock_watcher("oxidezap-screensaver", tx, watch_screen_saver);
}

fn spawn_lock_watcher(name: &str, tx: Sender<SystemLockEvent>, watch: fn(Sender<SystemLockEvent>)) {
    if let Err(error) = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || watch(tx))
    {
        warn!(%error, watcher = name, "could not start the system lock watcher");
    }
}

/// Watches `org.freedesktop.login1.Manager.PrepareForSleep` on the system bus.
/// Both edges lock: the pre-sleep signal may arrive while the UI is already
/// frozen, and the resume signal guarantees the lock is active after waking.
fn watch_logind(tx: Sender<SystemLockEvent>) {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(error) => {
            debug!(%error, "lock on suspend unavailable: no system bus");
            return;
        }
    };
    let rule = match MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface("org.freedesktop.login1.Manager")
        .and_then(|rule| rule.member("PrepareForSleep"))
        .map(|rule| rule.build())
    {
        Ok(rule) => rule,
        Err(error) => {
            warn!(%error, "could not build the logind signal match rule");
            return;
        }
    };
    let messages = match MessageIterator::for_match_rule(rule, &connection, Some(LOCK_EVENT_QUEUE))
    {
        Ok(messages) => messages,
        Err(error) => {
            debug!(%error, "could not watch logind PrepareForSleep");
            return;
        }
    };
    for message in messages {
        let Ok(message) = message else {
            continue;
        };
        let Ok((sleeping,)) = message.body().deserialize::<(bool,)>() else {
            continue;
        };
        if tx.send(logind_event(sleeping)).is_err() {
            return;
        }
    }
    debug!("the logind signal watcher stopped");
}

/// Watches `ActiveChanged` signals from any desktop ScreenSaver interface.
/// GNOME, KDE, Cinnamon, MATE, and XFCE expose different names, so the
/// interface name is matched by suffix instead of assuming one desktop.
fn watch_screen_saver(tx: Sender<SystemLockEvent>) {
    let connection = match zbus::blocking::Connection::session() {
        Ok(connection) => connection,
        Err(error) => {
            debug!(%error, "lock on screen lock unavailable: no session bus");
            return;
        }
    };
    let rule = match MatchRule::builder()
        .msg_type(MessageType::Signal)
        .member("ActiveChanged")
        .map(|rule| rule.build())
    {
        Ok(rule) => rule,
        Err(error) => {
            warn!(%error, "could not build the ScreenSaver signal match rule");
            return;
        }
    };
    let messages = match MessageIterator::for_match_rule(rule, &connection, Some(LOCK_EVENT_QUEUE))
    {
        Ok(messages) => messages,
        Err(error) => {
            debug!(%error, "could not watch the desktop ScreenSaver");
            return;
        }
    };
    for message in messages {
        let Ok(message) = message else {
            continue;
        };
        let header = message.header();
        let Some(interface) = header.interface() else {
            continue;
        };
        if !is_screen_saver_interface(interface.as_str()) {
            continue;
        }
        let Ok((active,)) = message.body().deserialize::<(bool,)>() else {
            continue;
        };
        if active && tx.send(SystemLockEvent::ScreenLocked).is_err() {
            return;
        }
    }
    debug!("the ScreenSaver signal watcher stopped");
}

pub struct SmokeState {
    pub account_id: String,
    pub started: Instant,
    pub gtk_xid: Option<u32>,
    pub loaded_at: Option<Instant>,
    pub verified: bool,
}

/// Maximum number of automatic reloads after a WebKit crash. A crash that
/// happens after every retry stops the loop once this budget is spent.
const MAX_CRASH_ATTEMPTS: u32 = 3;

/// Delay before each automatic reload. One entry per allowed attempt.
const CRASH_BACKOFF: [Duration; MAX_CRASH_ATTEMPTS as usize] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
];

/// Per-account crash recovery policy. The clock is injected so the policy can
/// be tested without waiting for real timers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CrashRecovery {
    attempts: u32,
    next_retry: Option<Instant>,
    failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrashDecision {
    RetryAfter(Duration),
    Exhausted,
    Ignored,
}

impl CrashRecovery {
    fn on_crash(&mut self, now: Instant) -> CrashDecision {
        if self.failed || self.next_retry.is_some() {
            return CrashDecision::Ignored;
        }
        if self.attempts >= MAX_CRASH_ATTEMPTS {
            self.failed = true;
            return CrashDecision::Exhausted;
        }
        let delay = CRASH_BACKOFF[self.attempts as usize];
        self.attempts += 1;
        self.next_retry = Some(now + delay);
        CrashDecision::RetryAfter(delay)
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.next_retry.is_some_and(|at| now >= at)
    }

    fn take_due_retry(&mut self, now: Instant) -> bool {
        if !self.retry_due(now) {
            return false;
        }
        self.next_retry = None;
        true
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn crash_decision(quitting: bool, recovery: &mut CrashRecovery, now: Instant) -> CrashDecision {
    if quitting {
        return CrashDecision::Ignored;
    }
    recovery.on_crash(now)
}

/// Maximum number of automatic connection reloads. The browser's own
/// reconnect is preferred; OxideZap only reloads a page in an error state.
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Delay before each automatic connection reload. One entry per attempt.
const RECONNECT_BACKOFF: [Duration; MAX_RECONNECT_ATTEMPTS as usize] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// Native fallback for a main-frame load that never finishes. WebKit usually
/// reports `load-failed` first; this covers a network stack that hangs without
/// producing an error.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// What the account titlebar shows. The browser script is the primary source;
/// `Connecting` is the short window before the first report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionState {
    #[default]
    Connecting,
    Connected,
    Offline,
    Reconnecting,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            ConnectionState::Connecting => "Connecting…",
            ConnectionState::Connected => "Connected",
            ConnectionState::Offline => "Offline",
            ConnectionState::Reconnecting => "Reconnecting…",
        }
    }

    pub fn retry_available(self) -> bool {
        matches!(
            self,
            ConnectionState::Offline | ConnectionState::Reconnecting
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionDecision {
    None,
    Offline,
    Reconnected,
    LoginPageBlocked,
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryAction {
    Idle,
    Postponed,
    Fire,
    LoginPageBlocked,
    Cancelled,
}

/// Pure per-account connection state machine. It owns the backoff clock so
/// the policy can be tested without GTK, WebKit, or real timers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ConnectionTracker {
    state: ConnectionState,
    online: bool,
    reported: bool,
    login_page: bool,
    page_error: bool,
    loaded: bool,
    attempts: u32,
    next_retry: Option<Instant>,
    exhausted: bool,
    load_started: Option<Instant>,
    offline_notified: bool,
    recovery_notified: bool,
    login_warned: bool,
}

impl ConnectionTracker {
    fn state(&self) -> ConnectionState {
        self.state
    }

    fn on_browser(&mut self, now: Instant, online: bool, login_page: bool) -> ConnectionDecision {
        self.login_page = login_page;
        self.online = online;
        self.reported = true;
        if !online {
            self.next_retry = None;
            self.recovery_notified = false;
            let previous = self.state;
            self.state = ConnectionState::Offline;
            if previous != ConnectionState::Offline && !self.offline_notified {
                self.offline_notified = true;
                return ConnectionDecision::Offline;
            }
            return ConnectionDecision::None;
        }
        if self.page_error {
            if login_page {
                self.next_retry = None;
                self.state = ConnectionState::Offline;
                if self.login_warned {
                    return ConnectionDecision::None;
                }
                self.login_warned = true;
                return ConnectionDecision::LoginPageBlocked;
            }
            return self.schedule_retry(now);
        }
        if self.loaded {
            return self.mark_connected();
        }
        if self.state == ConnectionState::Offline {
            self.state = ConnectionState::Connecting;
        }
        ConnectionDecision::None
    }

    fn on_load_started(&mut self, now: Instant) {
        if self.state == ConnectionState::Connected && !self.page_error {
            // A normal in-app navigation is not an outage; the timeout
            // fallback is only for startup and reconnect attempts.
            self.load_started = None;
            return;
        }
        self.load_started = Some(now);
        self.loaded = false;
    }

    fn on_load_finished(&mut self, now: Instant) -> ConnectionDecision {
        self.load_started = None;
        if self.page_error {
            self.loaded = false;
            return self.schedule_retry(now);
        }
        self.loaded = true;
        if !self.online {
            if !self.reported {
                // The first browser report has not arrived yet. A finished
                // load is not evidence of an outage; wait for the report.
                return self.mark_connected();
            }
            self.recovery_notified = false;
            let previous = self.state;
            self.state = ConnectionState::Offline;
            if previous != ConnectionState::Offline && !self.offline_notified {
                self.offline_notified = true;
                return ConnectionDecision::Offline;
            }
            return ConnectionDecision::None;
        }
        self.mark_connected()
    }

    fn on_load_failed(&mut self, now: Instant) -> ConnectionDecision {
        self.load_started = None;
        self.loaded = false;
        self.page_error = true;
        self.next_retry = None;
        if self.login_page {
            self.state = ConnectionState::Offline;
            if self.login_warned {
                return ConnectionDecision::None;
            }
            self.login_warned = true;
            return ConnectionDecision::LoginPageBlocked;
        }
        if !self.online {
            self.recovery_notified = false;
            let previous = self.state;
            self.state = ConnectionState::Offline;
            if previous != ConnectionState::Offline && !self.offline_notified {
                self.offline_notified = true;
                return ConnectionDecision::Offline;
            }
            return ConnectionDecision::None;
        }
        self.schedule_retry(now)
    }

    fn on_load_timeout(&mut self, now: Instant) -> ConnectionDecision {
        let Some(started) = self.load_started else {
            return ConnectionDecision::None;
        };
        if now.duration_since(started) < LOAD_TIMEOUT {
            return ConnectionDecision::None;
        }
        self.on_load_failed(now)
    }

    fn schedule_retry(&mut self, now: Instant) -> ConnectionDecision {
        if self.next_retry.is_some() || self.exhausted || !self.online || self.login_page {
            return ConnectionDecision::None;
        }
        if self.attempts >= MAX_RECONNECT_ATTEMPTS {
            self.exhausted = true;
            self.state = ConnectionState::Offline;
            return ConnectionDecision::Exhausted;
        }
        let delay = RECONNECT_BACKOFF[self.attempts as usize];
        self.attempts += 1;
        self.next_retry = Some(now + delay);
        self.state = ConnectionState::Reconnecting;
        ConnectionDecision::None
    }

    fn take_due_retry(&mut self, now: Instant, downloads_active: bool) -> RetryAction {
        let Some(at) = self.next_retry else {
            return RetryAction::Idle;
        };
        if now < at {
            return RetryAction::Idle;
        }
        if downloads_active {
            return RetryAction::Postponed;
        }
        if self.login_page {
            self.next_retry = None;
            self.state = ConnectionState::Offline;
            if self.login_warned {
                return RetryAction::Cancelled;
            }
            self.login_warned = true;
            return RetryAction::LoginPageBlocked;
        }
        if !self.online {
            self.next_retry = None;
            self.state = ConnectionState::Offline;
            return RetryAction::Cancelled;
        }
        self.next_retry = None;
        self.page_error = false;
        self.loaded = false;
        self.load_started = Some(now);
        self.state = ConnectionState::Reconnecting;
        RetryAction::Fire
    }

    fn retry_now(&mut self, now: Instant) {
        self.attempts = 0;
        self.exhausted = false;
        self.next_retry = None;
        self.page_error = false;
        self.loaded = false;
        self.load_started = Some(now);
        self.login_warned = false;
        self.recovery_notified = false;
        self.state = if self.online {
            ConnectionState::Reconnecting
        } else {
            ConnectionState::Offline
        };
    }

    fn on_reload_blocked(&mut self) {
        self.load_started = None;
        self.loaded = false;
        self.next_retry = None;
        self.state = ConnectionState::Offline;
    }

    fn on_process_terminated(&mut self) {
        self.page_error = false;
        self.loaded = false;
        self.load_started = None;
        self.next_retry = None;
        self.state = if self.online {
            ConnectionState::Reconnecting
        } else {
            ConnectionState::Offline
        };
    }

    fn mark_connected(&mut self) -> ConnectionDecision {
        let previous = self.state;
        self.state = ConnectionState::Connected;
        self.page_error = false;
        self.loaded = true;
        self.load_started = None;
        self.attempts = 0;
        self.exhausted = false;
        self.next_retry = None;
        self.login_warned = false;
        self.offline_notified = false;
        let was_recovering = matches!(
            previous,
            ConnectionState::Offline | ConnectionState::Reconnecting
        );
        if was_recovering && !self.recovery_notified {
            self.recovery_notified = true;
            ConnectionDecision::Reconnected
        } else {
            ConnectionDecision::None
        }
    }
}

fn is_whatsapp_web_uri(uri: &str) -> bool {
    url::Url::parse(uri.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "web.whatsapp.com")
}

/// A storage operation currently running on the GTK/storage worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningStorageOperation {
    pub account_id: String,
    pub operation: StorageOperation,
}

/// Result banner shown by Settings > Storage.
#[derive(Clone, Debug)]
pub struct StorageNotice {
    pub error: bool,
    pub text: String,
}

impl StorageNotice {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            error: false,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            error: true,
            text: text.into(),
        }
    }
}

/// Storage UI state shared by the app model and the settings window.
#[derive(Clone, Debug, Default)]
pub struct StorageState {
    pub scanning: bool,
    pub snapshot: Option<StorageSnapshot>,
    pub running: Option<RunningStorageOperation>,
    pub notice: Option<StorageNotice>,
}

pub struct AppModel {
    pub paths: Paths,
    pub settings: Settings,
    pub gtk: GtkHandle,
    pub x11: X11,
    pub locked: bool,
    pub quitting: bool,
    gtk_events: Receiver<GtkEvent>,
    instance_messages: Receiver<InstanceRequest>,
    hotkey_actions: Receiver<HotkeyAction>,
    tray_actions: Receiver<TrayAction>,
    tray: Option<TrayHandle>,
    hotkey: HotkeyHandle,
    badge: Badge,
    instance_listener: Option<InstanceListener>,
    account_windows: HashMap<String, WindowHandle<AccountWindow>>,
    account_xids: HashMap<String, u32>,
    crash_recovery: HashMap<String, CrashRecovery>,
    connection: HashMap<String, ConnectionTracker>,
    active_downloads: HashMap<String, u32>,
    scales: HashMap<String, f32>,
    settings_window: Option<WindowHandle<SettingsWindow>>,
    lock_window: Option<WindowHandle<LockWindow>>,
    downloads_window: Option<WindowHandle<DownloadsWindow>>,
    downloads: DownloadManager,
    downloads_writer: DownloadWriter,
    pending_downloads: bool,
    smoke: Option<SmokeState>,
    startup_html: Option<String>,
    startup_url: Option<String>,
    initial_action: Option<InstanceMessage>,
    pending_urls: HashMap<String, String>,
    start_hidden: bool,
    pending_accounts: HashSet<String>,
    pending_settings: bool,
    pending_lock: bool,
    hidden_accounts: HashSet<String>,
    settings_writer: SettingsWriter,
    save_debounce: SaveDebounce,
    pub update_state: UpdateState,
    pub storage: StorageState,
    update_tx: Sender<UpdateState>,
    update_rx: Receiver<UpdateState>,
    update_check_running: bool,
    last_idle_check: Instant,
    last_scale_check: Instant,
    last_notifications: Vec<(String, Instant)>,
    lock_events: Receiver<SystemLockEvent>,
    boss_hidden: Option<HashSet<String>>,
    boss_settings_hidden: bool,
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
        instance_messages: Receiver<InstanceRequest>,
        badge: Badge,
        smoke: Option<SmokeState>,
        startup_html: Option<String>,
        startup_url: Option<String>,
        initial_action: Option<InstanceMessage>,
        start_hidden: bool,
    ) -> Self {
        let _ = cx;
        let (update_tx, update_rx) = unbounded();
        let update_config = updates::CheckConfig::new(
            env!("CARGO_PKG_VERSION"),
            settings.automatic_update_checks,
            settings.skipped_update_version.clone(),
            paths.update_cache_file(),
        );
        let update_state = updates::cached_state(&update_config);
        let downloads = DownloadManager::load(paths.downloads_index_file());
        let (lock_event_tx, lock_event_rx) = unbounded();
        if smoke.is_none() {
            start_system_lock_watchers(lock_event_tx);
        }
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
            crash_recovery: HashMap::new(),
            connection: HashMap::new(),
            active_downloads: HashMap::new(),
            scales: HashMap::new(),
            settings_window: None,
            lock_window: None,
            downloads_window: None,
            downloads,
            downloads_writer: DownloadWriter::start(),
            pending_downloads: false,
            smoke,
            startup_html,
            startup_url,
            initial_action,
            pending_urls: HashMap::new(),
            start_hidden,
            pending_accounts: HashSet::new(),
            pending_settings: false,
            pending_lock: false,
            hidden_accounts: HashSet::new(),
            settings_writer: SettingsWriter::start(),
            save_debounce: SaveDebounce::default(),
            update_state,
            storage: StorageState::default(),
            update_tx,
            update_rx,
            update_check_running: false,
            last_idle_check: Instant::now(),
            last_scale_check: Instant::now(),
            last_notifications: Vec::new(),
            lock_events: lock_event_rx,
            boss_hidden: None,
            boss_settings_hidden: false,
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

        self.sync_download_layout();

        let initial_action = self.initial_action.take();

        if let Some(InstanceMessage::OpenAccount(query)) = &initial_action {
            if let Ok(account_id) = resolve_account(&self.settings, query) {
                self.settings.active_account = Some(account_id);
            }
        }
        if let Some(InstanceMessage::OpenChat { account, chat }) = &initial_action {
            let account_id = match account {
                Some(query) => resolve_account(&self.settings, query).ok(),
                None => self.settings.active_account_id(),
            };
            if let Some(account_id) = account_id {
                self.settings.active_account = Some(account_id.clone());
                self.pending_urls.insert(account_id, chat.web_url());
            }
        }
        if let Some(InstanceMessage::OpenUrl(url)) = &initial_action {
            self.startup_url = Some(url.clone());
        }
        if let Some(InstanceMessage::SetDoNotDisturb(mode)) = &initial_action {
            self.apply_do_not_disturb(*mode, cx);
        }
        if matches!(initial_action.as_ref(), Some(InstanceMessage::Hide)) {
            self.start_hidden = true;
        }

        self.apply_hotkeys();
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
        match initial_action {
            Some(InstanceMessage::Lock) => self.lock(cx),
            Some(InstanceMessage::OpenDownloads) => self.open_downloads(cx),
            _ => {}
        }
        cx.activate(true);
        self.start_update_check();
    }

    /// Starts one throttled GitHub release check on a worker thread. The GPUI
    /// thread only receives the result through a channel in `poll`.
    fn start_update_check(&mut self) {
        if self.update_check_running || !self.settings.automatic_update_checks {
            return;
        }
        let config = updates::CheckConfig::new(
            env!("CARGO_PKG_VERSION"),
            self.settings.automatic_update_checks,
            self.settings.skipped_update_version.clone(),
            self.paths.update_cache_file(),
        );
        self.update_check_running = true;
        let tx = self.update_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("oxidezap-updates".to_string())
            .spawn(move || {
                let state = updates::run_check(&config);
                let _ = tx.send(state);
            });
        if let Err(error) = spawned {
            warn!(%error, "could not start the update checker");
            self.update_check_running = false;
        }
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
        while let Ok(state) = self.update_rx.try_recv() {
            self.update_check_running = false;
            self.update_state =
                updates::apply_skipped(state, self.settings.skipped_update_version.as_deref());
            cx.refresh_windows();
        }
        while let Ok(event) = self.gtk_events.try_recv() {
            self.handle_gtk_event(event, cx);
        }
        self.poll_connection_recovery(cx);
        self.poll_crash_recovery();
        while let Ok(request) = self.instance_messages.try_recv() {
            self.handle_instance_request(request, cx);
        }
        while let Ok(action) = self.hotkey_actions.try_recv() {
            match action {
                HotkeyAction::Toggle => self.toggle_visibility(cx),
                HotkeyAction::BossKey => self.boss_toggle(cx),
            }
        }
        while let Ok(event) = self.lock_events.try_recv() {
            let trigger = event.trigger();
            if should_begin_lock(self.locked, trigger, &self.settings.lock) {
                debug!(
                    event = event.label(),
                    "locking after a system session event"
                );
                self.lock_for(trigger, cx);
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
                    self.lock_for(LockTrigger::Idle, cx);
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
                        self.account_xids.remove(&id);
                    }
                }
            }
        }

        if self.smoke.is_some() {
            self.poll_smoke(cx);
        }
    }

    fn poll_crash_recovery(&mut self) {
        if self.quitting {
            self.crash_recovery.clear();
            return;
        }
        let now = Instant::now();
        let due: Vec<String> = self
            .crash_recovery
            .iter()
            .filter(|(_, recovery)| recovery.retry_due(now))
            .map(|(account_id, _)| account_id.clone())
            .collect();
        for account_id in due {
            if self.downloads_active(&account_id) {
                // Keep the retry pending; reloading would cancel the download.
                continue;
            }
            let Some(recovery) = self.crash_recovery.get_mut(&account_id) else {
                continue;
            };
            if !recovery.take_due_retry(now) {
                continue;
            }
            if !self.account_windows.contains_key(&account_id) {
                self.crash_recovery.remove(&account_id);
                continue;
            }
            info!(account_id = %account_id, "reloading WhatsApp Web after a crash");
            self.gtk.reload(&account_id);
        }
    }

    pub fn account_crash_failed(&self, account_id: &str) -> bool {
        self.crash_recovery
            .get(account_id)
            .is_some_and(|recovery| recovery.failed)
    }

    pub fn account_connection(&self, account_id: &str) -> ConnectionState {
        self.connection
            .get(account_id)
            .map(ConnectionTracker::state)
            .unwrap_or_default()
    }

    fn downloads_active(&self, account_id: &str) -> bool {
        self.active_downloads.get(account_id).copied().unwrap_or(0) > 0
    }

    fn poll_connection_recovery(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            self.connection.clear();
            self.active_downloads.clear();
            return;
        }
        let now = Instant::now();
        let accounts: Vec<String> = self.connection.keys().cloned().collect();
        for account_id in accounts {
            if !self.account_windows.contains_key(&account_id) {
                self.connection.remove(&account_id);
                self.active_downloads.remove(&account_id);
                continue;
            }
            // Crash recovery owns the retry schedule while it is active. An
            // explicitly reset entry (failed = false, no pending retry) does
            // not block connection recovery after a manual retry.
            let crash_active = self
                .crash_recovery
                .get(&account_id)
                .is_some_and(|recovery| recovery.failed || recovery.next_retry.is_some());
            if crash_active {
                continue;
            }
            let downloads_active = self.downloads_active(&account_id);
            let timeout = self
                .connection
                .get_mut(&account_id)
                .map(|tracker| tracker.on_load_timeout(now))
                .unwrap_or(ConnectionDecision::None);
            if timeout != ConnectionDecision::None {
                self.announce_connection(&account_id, timeout);
                self.refresh_account_window(&account_id, cx);
            }
            let action = self
                .connection
                .get_mut(&account_id)
                .map(|tracker| tracker.take_due_retry(now, downloads_active))
                .unwrap_or(RetryAction::Idle);
            match action {
                RetryAction::Idle => {}
                RetryAction::Postponed => {
                    debug!(
                        account_id = %account_id,
                        "postponing a WhatsApp Web reconnect while a download is active"
                    );
                }
                RetryAction::Fire => {
                    info!(account_id = %account_id, "reloading WhatsApp Web after a connection failure");
                    self.gtk.reload(&account_id);
                    self.refresh_account_window(&account_id, cx);
                }
                RetryAction::LoginPageBlocked => {
                    self.announce_connection(&account_id, ConnectionDecision::LoginPageBlocked);
                    self.refresh_account_window(&account_id, cx);
                }
                RetryAction::Cancelled => {
                    self.refresh_account_window(&account_id, cx);
                }
            }
        }
    }

    fn announce_connection(&mut self, account_id: &str, decision: ConnectionDecision) {
        if decision == ConnectionDecision::None {
            return;
        }
        let name = self.account_name(account_id);
        let icon = self.paths.runtime_icon();
        let notify_enabled =
            !self.locked && self.settings.connection_notifications_enabled(account_id);
        match decision {
            ConnectionDecision::None => {}
            ConnectionDecision::Offline => {
                if notify_enabled {
                    notify::notify(
                        "WhatsApp Web is offline",
                        &format!(
                            "{name} lost its connection. OxideZap will reconnect when the network returns."
                        ),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::Reconnected => {
                if notify_enabled {
                    notify::notify(
                        "Connection restored",
                        &format!("{name} is connected again."),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::LoginPageBlocked => {
                if notify_enabled {
                    notify::notify(
                        "Connection restored",
                        &format!(
                            "{name} is on the WhatsApp login page. OxideZap did not reload it automatically; use Retry now if the page is stuck."
                        ),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::Exhausted => {
                notify::notify(
                    "Could not reconnect WhatsApp Web",
                    &format!(
                        "OxideZap stopped automatic reconnects for {name} after {MAX_RECONNECT_ATTEMPTS} attempts. Use Retry now in the account window."
                    ),
                    Some(&icon),
                );
            }
        }
    }

    pub fn retry_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if self.downloads_active(account_id) {
            warn!(
                account_id = %account_id,
                "manual WhatsApp Web reload skipped while a download is active"
            );
            if self.settings.notifications {
                notify::notify(
                    "Download in progress",
                    "OxideZap did not reload WhatsApp Web while a download is active. Retry when the download finishes.",
                    Some(&self.paths.runtime_icon()),
                );
            }
            self.refresh_account_window(account_id, cx);
            return;
        }
        info!(account_id = %account_id, "manual WhatsApp Web reload requested");
        self.crash_recovery
            .entry(account_id.to_string())
            .or_default()
            .reset();
        self.connection
            .entry(account_id.to_string())
            .or_default()
            .retry_now(Instant::now());
        self.gtk.show(account_id);
        self.gtk.reload(account_id);
        self.refresh_account_window(account_id, cx);
    }

    fn refresh_account_window(&self, account_id: &str, cx: &mut Context<Self>) {
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, _, cx| cx.notify());
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
            GtkEvent::LoadState { account_id, state } => match state {
                LoadState::Started => {
                    self.connection
                        .entry(account_id.clone())
                        .or_default()
                        .on_load_started(Instant::now());
                    self.refresh_account_window(&account_id, cx);
                }
                LoadState::Finished => {
                    let recovered = self
                        .crash_recovery
                        .remove(&account_id)
                        .is_some_and(|recovery| recovery.failed);
                    self.gtk.set_zoom(&account_id, self.settings.zoom);
                    if recovered {
                        self.gtk.show(&account_id);
                    }
                    let decision = self
                        .connection
                        .entry(account_id.clone())
                        .or_default()
                        .on_load_finished(Instant::now());
                    self.announce_connection(&account_id, decision);
                    self.refresh_account_window(&account_id, cx);
                    if let Some(smoke) = &mut self.smoke {
                        if smoke.account_id == account_id && smoke.loaded_at.is_none() {
                            smoke.loaded_at = Some(Instant::now());
                        }
                    }
                }
                LoadState::Redirected | LoadState::Committed => {}
            },
            GtkEvent::LoadFailed {
                account_id,
                uri,
                message,
            } => {
                if !is_whatsapp_web_uri(&uri) {
                    debug!(%account_id, %uri, %message, "ignored a load failure outside WhatsApp Web");
                } else {
                    warn!(%account_id, %uri, %message, "WhatsApp Web is in a load error state");
                    let decision = self
                        .connection
                        .entry(account_id.clone())
                        .or_default()
                        .on_load_failed(Instant::now());
                    self.announce_connection(&account_id, decision);
                    self.refresh_account_window(&account_id, cx);
                }
            }
            GtkEvent::ConnectionState { account_id, report } => {
                let online = report.state == BrowserConnectionState::Online;
                debug!(
                    %account_id,
                    online,
                    login_page = report.login_page,
                    "browser connection report"
                );
                let decision = self
                    .connection
                    .entry(account_id.clone())
                    .or_default()
                    .on_browser(Instant::now(), online, report.login_page);
                self.announce_connection(&account_id, decision);
                self.refresh_account_window(&account_id, cx);
            }
            GtkEvent::ReloadBlocked { account_id } => {
                warn!(
                    account_id = %account_id,
                    "the GTK thread refused a reload during an active download"
                );
                if let Some(tracker) = self.connection.get_mut(&account_id) {
                    tracker.on_reload_blocked();
                }
                if self.settings.notifications {
                    notify::notify(
                        "Download in progress",
                        "OxideZap did not reload WhatsApp Web while a download is active.",
                        Some(&self.paths.runtime_icon()),
                    );
                }
                self.refresh_account_window(&account_id, cx);
            }
            GtkEvent::Title {
                account_id,
                title,
                unread,
            } => {
                debug!(%account_id, %title, unread, "webview title changed");
                if self.settings.set_unread(&account_id, unread) {
                    self.save_settings(cx);
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
                if self.settings.message_notifications_enabled(&account_id)
                    && !self.locked
                    && self.should_notify(&title, &body)
                {
                    let sound = self.settings.notification_sound_enabled(&account_id);
                    notify::notify_with_sound(
                        &title,
                        &body,
                        Some(&self.paths.runtime_icon()),
                        sound,
                    );
                }
            }
            GtkEvent::DownloadStarted {
                id,
                account_id,
                filename,
                path,
                source_url,
                total_bytes,
            } => {
                debug!(%account_id, %filename, path = %path.display(), "download started");
                let account_name = self.account_name(&account_id);
                let record = DownloadRecord {
                    id,
                    account_id: account_id.clone(),
                    account_name,
                    filename: filename.clone(),
                    path: Some(path.clone()),
                    source_url,
                    received_bytes: 0,
                    total_bytes,
                    status: DownloadStatus::InProgress,
                    started_at: now_millis(),
                    finished_at: None,
                };
                if self.downloads.start(record) {
                    self.save_downloads();
                }
                if self.settings.notifications {
                    let folder = path
                        .parent()
                        .map(|parent| parent.display().to_string())
                        .unwrap_or_else(|| self.downloads_base_dir().display().to_string());
                    notify::notify(
                        "Downloading",
                        &format!("{filename}\nSaving to {folder}"),
                        Some(&self.paths.runtime_icon()),
                    );
                }
                self.refresh_downloads_window(cx);
            }
            GtkEvent::DownloadProgress {
                id,
                received,
                total,
            } => {
                if self.downloads.progress(&id, received, total) {
                    self.refresh_downloads_window(cx);
                }
            }
            GtkEvent::DownloadFinished {
                id,
                success,
                cancelled,
            } => {
                let record = self.downloads.get(&id).cloned();
                if let Some(record) = record {
                    debug!(
                        %id,
                        filename = %record.filename,
                        success,
                        cancelled,
                        "download finished"
                    );
                    if self.downloads.finish(&id, success, cancelled, now_millis()) {
                        self.save_downloads();
                        if self.settings.notifications {
                            self.notify_download_outcome(&record, success, cancelled);
                        }
                    }
                } else {
                    debug!(%id, "ignored a download event for an unknown record");
                }
                self.refresh_downloads_window(cx);
            }
            GtkEvent::DownloadRejected {
                account_id,
                filename,
                reason,
            } => {
                warn!(%account_id, %filename, %reason, "download could not start");
                if self.settings.notifications {
                    notify::notify(
                        "Download could not start",
                        &format!("{filename}\n{reason}"),
                        Some(&self.paths.runtime_icon()),
                    );
                }
            }
            GtkEvent::DownloadRetryFailed { id, message } => {
                debug!(%id, %message, "download retry failed");
                if self.settings.notifications {
                    notify::notify(
                        "Download could not be retried",
                        &message,
                        Some(&self.paths.runtime_icon()),
                    );
                }
            }
            GtkEvent::OpenExternal { url } => {
                Self::open_external(cx, &url);
            }
            GtkEvent::WhatsAppUrl { account_id, url } => {
                self.open_url_in_account(&account_id, &url, cx);
            }
            GtkEvent::Crashed { account_id } => {
                warn!(account_id = %account_id, "the WebKit web process crashed");
                if let Some(tracker) = self.connection.get_mut(&account_id) {
                    tracker.on_process_terminated();
                }
                let quitting = self.quitting;
                let now = Instant::now();
                let recovery = self.crash_recovery.entry(account_id.clone()).or_default();
                match crash_decision(quitting, recovery, now) {
                    CrashDecision::RetryAfter(delay) => {
                        info!(
                            account_id = %account_id,
                            retry_in_ms = delay.as_millis() as u64,
                            "scheduling a WhatsApp Web reload after a crash"
                        );
                    }
                    CrashDecision::Exhausted => {
                        error!(account_id = %account_id, "WebKit crash recovery exhausted");
                        self.gtk.hide(&account_id);
                        notify::notify(
                            "WhatsApp Web crashed",
                            &format!(
                                "Automatic recovery failed after {MAX_CRASH_ATTEMPTS} attempts. Use \"Retry now\" in the account window."
                            ),
                            Some(&self.paths.runtime_icon()),
                        );
                        self.refresh_account_window(&account_id, cx);
                    }
                    CrashDecision::Ignored => {}
                }
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
            GtkEvent::StorageFinished {
                account_id,
                operation,
                result,
            } => {
                self.handle_storage_finished(account_id, operation, result, cx);
            }
        }
    }

    fn handle_instance_request(&mut self, request: InstanceRequest, cx: &mut Context<Self>) {
        let InstanceRequest { message, reply } = request;
        let result = self.apply_instance_message(message, cx);
        let Some(sender) = reply else {
            return;
        };
        let answer = match result {
            Ok(Some(output)) => InstanceReply::Output(output),
            Ok(None) => InstanceReply::Ok,
            Err(error) => InstanceReply::Error(error),
        };
        let _ = sender.send(answer);
    }

    fn apply_instance_message(
        &mut self,
        message: InstanceMessage,
        cx: &mut Context<Self>,
    ) -> Result<Option<String>, String> {
        match message {
            InstanceMessage::Show => {
                self.show_all(cx);
                Ok(None)
            }
            InstanceMessage::Toggle => {
                self.toggle_visibility(cx);
                Ok(None)
            }
            InstanceMessage::Lock => self.request_lock(cx).map(|_| None),
            InstanceMessage::Hide => {
                self.hide_all(cx);
                Ok(None)
            }
            InstanceMessage::Quit => {
                self.quit(cx);
                Ok(None)
            }
            InstanceMessage::OpenUrl(url) => {
                self.handle_url(&url, cx);
                Ok(None)
            }
            InstanceMessage::OpenAccount(query) => {
                let account_id = resolve_account(&self.settings, &query)?;
                self.open_account_window(&account_id, cx);
                Ok(None)
            }
            InstanceMessage::OpenChat { account, chat } => {
                self.open_chat(account, chat, cx).map(|_| None)
            }
            InstanceMessage::SetDoNotDisturb(mode) => {
                self.apply_do_not_disturb(mode, cx);
                Ok(None)
            }
            InstanceMessage::OpenDownloads => {
                self.open_downloads(cx);
                Ok(None)
            }
            InstanceMessage::Status => Ok(Some(status_text(
                &self.settings,
                true,
                self.account_windows.len(),
            ))),
            InstanceMessage::ListAccounts => Ok(Some(account_list_text(&self.settings))),
        }
    }

    fn request_lock(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if !lock_is_configured(&self.settings.lock) {
            return Err("the application lock is not configured".to_string());
        }
        self.lock(cx);
        Ok(())
    }

    fn open_chat(
        &mut self,
        account: Option<String>,
        chat: ChatTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.locked {
            return Err("the application is locked; unlock it first".to_string());
        }
        let account_id = match account {
            Some(query) => resolve_account(&self.settings, &query)?,
            None => self
                .settings
                .active_account_id()
                .ok_or_else(|| "no active account; pass --account".to_string())?,
        };
        let url = chat.web_url();
        if self.account_windows.contains_key(&account_id) {
            self.show_account_window(&account_id, cx);
            self.gtk.load_url(&account_id, &url);
        } else {
            self.pending_urls.insert(account_id.clone(), url);
            self.open_account_window(&account_id, cx);
        }
        Ok(())
    }

    fn open_downloads(&mut self, cx: &mut Context<Self>) {
        self.open_downloads_folder(cx);
    }

    pub fn open_downloads_window(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(handle) = self.downloads_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.downloads_window = None;
        }
        if self.pending_downloads {
            return;
        }
        self.pending_downloads = true;
        let model = cx.entity().clone();
        cx.defer(move |cx| open_downloads_window_deferred(model, cx));
    }

    pub fn downloads(&self) -> &[DownloadRecord] {
        self.downloads.records()
    }

    pub fn active_downloads(&self) -> usize {
        self.downloads.active_count()
    }

    pub fn downloads_base_dir(&self) -> PathBuf {
        self.settings
            .downloads_directory
            .as_deref()
            .and_then(downloads::valid_base_dir)
            .unwrap_or_else(|| self.paths.downloads_dir.clone())
    }

    pub fn download_path(&self, id: &str) -> Option<PathBuf> {
        self.downloads
            .get(id)
            .and_then(|record| record.path.clone())
    }

    pub fn cancel_download(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.downloads.cancel(id, now_millis()) {
            return;
        }
        self.gtk.cancel_download(id);
        self.save_downloads();
        self.refresh_downloads_window(cx);
    }

    pub fn retry_download(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(record) = self.downloads.get(id) else {
            return;
        };
        let Some(url) = record.retry_url().map(str::to_string) else {
            notify::notify(
                "Retry unavailable",
                "This download has no reusable source URL. Start it again from WhatsApp Web.",
                Some(&self.paths.runtime_icon()),
            );
            return;
        };
        let account_id = record.account_id.clone();
        self.gtk.retry_download(id, &account_id, &url);
        cx.notify();
    }

    pub fn remove_download(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.downloads.remove(id) {
            return;
        }
        self.save_downloads();
        self.refresh_downloads_window(cx);
    }

    pub fn open_download(&self, id: &str, cx: &mut Context<Self>) {
        let Some(path) = self.download_path(id) else {
            return;
        };
        self.open_path(path, cx);
    }

    pub fn open_download_folder(&self, id: &str, cx: &mut Context<Self>) {
        let Some(folder) = self
            .downloads
            .get(id)
            .and_then(|record| record.destination_dir().map(PathBuf::from))
        else {
            return;
        };
        self.open_path(folder, cx);
    }

    pub fn open_downloads_folder(&self, cx: &mut Context<Self>) {
        let folder = self.downloads_base_dir();
        self.open_path(folder, cx);
    }

    fn open_path(&self, path: PathBuf, cx: &mut Context<Self>) {
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = open::that_detached(&path) {
                    warn!(path = %path.display(), %error, "could not open a download path");
                }
            })
            .detach();
    }

    fn notify_download_outcome(&self, record: &DownloadRecord, success: bool, cancelled: bool) {
        let icon = self.paths.runtime_icon();
        if success {
            if let Some(path) = record.path.as_deref() {
                notify::notify_download(&record.filename, path, Some(&icon));
            } else {
                notify::notify("Download complete", &record.filename, Some(&icon));
            }
        } else if cancelled {
            notify::notify("Download cancelled", &record.filename, Some(&icon));
        } else {
            notify::notify(
                "Download failed",
                &format!("{}\nOpen the Download Center to retry.", record.filename),
                Some(&icon),
            );
        }
    }

    fn download_layout(&self) -> DownloadLayout {
        DownloadLayout::new(self.downloads_base_dir())
            .with_options(
                self.settings.downloads_account_folders,
                self.settings.downloads_date_folders,
            )
            .with_accounts(
                self.settings
                    .accounts
                    .iter()
                    .map(|account| (account.id.clone(), account.name.clone())),
            )
    }

    fn sync_download_layout(&self) {
        self.gtk.set_download_layout(self.download_layout());
    }

    fn save_downloads(&self) {
        self.downloads_writer.save(
            self.downloads.snapshot(),
            self.downloads.file().to_path_buf(),
        );
    }

    fn refresh_downloads_window(&self, cx: &mut Context<Self>) {
        if let Some(handle) = self.downloads_window {
            let _ = handle.update(cx, |_, _, cx| cx.notify());
        }
    }

    fn apply_do_not_disturb(&mut self, mode: DndMode, cx: &mut Context<Self>) {
        if self.settings.set_do_not_disturb(mode.is_enabled()) {
            self.save_settings(cx);
            self.update_tray();
            cx.refresh_windows();
        }
    }

    fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::ShowHide => self.toggle_visibility(cx),
            TrayAction::OpenAccount(id) => self.open_account_window(&id, cx),
            TrayAction::AddAccount => self.add_account(cx),
            TrayAction::Settings => self.open_settings_window(cx),
            TrayAction::Downloads => self.open_downloads_window(cx),
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
            Self::open_external(cx, url);
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
        Self::open_external(cx, url);
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
            self.account_xids.remove(account_id);
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
        known_windows: Vec<u32>,
        cx: &mut Context<Self>,
    ) {
        self.account_windows.insert(account.id.clone(), handle);
        self.hidden_accounts.remove(&account.id);
        let scale = handle
            .update(cx, |_, window, _| window.scale_factor())
            .unwrap_or(1.0);
        self.scales.insert(account.id.clone(), scale);

        let title = account_window_title(&account.name);
        let parent_xid = match self.try_tag_account(&account.id, &title, &known_windows) {
            Ok(AccountTag::Tagged) => self.account_xids.get(&account.id).copied(),
            Ok(AccountTag::Closed) => None,
            Ok(AccountTag::Pending) => {
                self.schedule_account_tag(&account.id, &title, &known_windows, cx);
                None
            }
            Err(error) => {
                warn!(account_id = %account.id, %error, "could not tag the GPUI window");
                None
            }
        };

        self.startup_html = None;
        let startup_url = self
            .pending_urls
            .remove(&account.id)
            .or_else(|| self.startup_url.take());
        let url = match startup_url.as_deref().map(links::classify) {
            Some(links::LinkAction::WhatsApp(web)) => web,
            Some(links::LinkAction::Allow) => startup_url.clone().unwrap_or_default(),
            Some(links::LinkAction::External(url)) => {
                Self::open_external(cx, &url);
                WHATSAPP_URL.to_string()
            }
            None => WHATSAPP_URL.to_string(),
        };
        self.gtk.send(GtkCommand::Create {
            account_id: account.id.clone(),
            parent_xid,
            data_dir: self.paths.account_data_dir(&account.id),
            cache_dir: self.paths.account_cache_dir(&account.id),
            url,
            html,
            show: !(self.start_hidden || self.locked),
            scale,
            zoom: self.settings.zoom,
            writing: self.settings.writing.clone(),
            proxy: self.settings.network.proxy.clone(),
        });

        self.settings.active_account = Some(account.id.clone());
        self.save_settings(cx);
        self.update_tray();

        let _ = handle.update(cx, |_, window, _| window.activate_window());
        if self.start_hidden || self.locked {
            self.hide_account_window(&account.id, cx);
        }
    }

    pub fn open_settings_window(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
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

    fn lock_for(&mut self, trigger: LockTrigger, cx: &mut Context<Self>) {
        if !should_begin_lock(self.locked, trigger, &self.settings.lock) {
            return;
        }
        self.locked = true;
        self.boss_hidden = None;
        self.boss_settings_hidden = false;
        self.pending_settings = false;
        // The privacy screen shows only the lock window. Closing the settings
        // window does not discard preferences: every change is already saved.
        if let Some(handle) = self.settings_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.open_lock_window(cx);
        self.update_tray();
    }

    pub fn lock(&mut self, cx: &mut Context<Self>) {
        self.lock_for(LockTrigger::Manual, cx);
    }

    pub fn try_unlock(&mut self, password: &str, cx: &mut Context<Self>) -> bool {
        if !unlock_is_authorized(self.settings.lock.hash.as_deref(), password) {
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

    /// Boss key: hide every window immediately for privacy. A second press
    /// restores only the windows that were visible when the key was pressed.
    pub fn boss_toggle(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(visible_before) = self.boss_hidden.take() {
            let ids: Vec<String> = self.account_windows.keys().cloned().collect();
            for id in ids {
                if visible_before.contains(&id) {
                    self.show_account_window(&id, cx);
                }
            }
            if std::mem::take(&mut self.boss_settings_hidden) {
                self.open_settings_window(cx);
            }
            self.update_tray();
            return;
        }

        let visible_before: HashSet<String> = self
            .account_windows
            .keys()
            .filter(|account_id| !self.hidden_accounts.contains(*account_id))
            .cloned()
            .collect();
        self.boss_hidden = Some(visible_before);
        self.boss_settings_hidden = if let Some(handle) = self.settings_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            true
        } else {
            let pending = self.pending_settings;
            self.pending_settings = false;
            pending
        };
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.update_tray();
    }

    pub fn hide_all(&mut self, cx: &mut Context<Self>) {
        self.boss_hidden = None;
        self.boss_settings_hidden = false;
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.update_tray();
    }

    pub fn show_all(&mut self, cx: &mut Context<Self>) {
        self.boss_hidden = None;
        self.boss_settings_hidden = false;
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
        self.hidden_accounts.insert(account_id.to_string());
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            if let Err(error) = self.x11.unmap(xid) {
                warn!(%error, "could not hide the GPUI window");
            }
        }
        self.gtk.hide(account_id);
        if !self.locked {
            self.lock_for(LockTrigger::Hide, cx);
        }
    }

    fn show_account_window(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        self.hidden_accounts.remove(account_id);
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            let _ = self.x11.map(xid);
            let _ = self.x11.activate(xid);
        }
        if !self.account_crash_failed(account_id) {
            self.gtk.show(account_id);
        }
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        self.update_tray();
    }

    fn any_account_visible(&self) -> bool {
        self.account_windows.keys().any(|account_id| {
            if self.hidden_accounts.contains(account_id) {
                return false;
            }
            // A window that is still waiting to be tagged counts as visible;
            // otherwise a toggle during the first few milliseconds would show
            // it again instead of hiding it.
            match self.account_xids.get(account_id) {
                Some(xid) => self.x11.is_viewable(*xid).unwrap_or(true),
                None => true,
            }
        })
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
        self.save_settings(cx);

        if self.settings.close_to_tray {
            self.hide_account_window(account_id, cx);
            return;
        }

        self.account_windows.remove(account_id);
        self.account_xids.remove(account_id);
        self.crash_recovery.remove(account_id);
        self.connection.remove(account_id);
        self.active_downloads.remove(account_id);
        self.hidden_accounts.remove(account_id);
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
        self.save_settings(cx);
        self.sync_download_layout();
        self.open_account_window(&id, cx);
        self.update_tray();
    }

    pub fn rename_account(&mut self, account_id: &str, name: &str, cx: &mut Context<Self>) {
        if self.settings.rename_account(account_id, name) {
            self.save_settings(cx);
            self.sync_download_layout();
            self.update_account_title(account_id, cx);
            self.update_tray();
            cx.refresh_windows();
        }
    }

    pub fn remove_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.settings.accounts.len() <= 1 {
            return;
        }
        self.forget_account(account_id, cx);
        self.save_settings(cx);
        self.update_tray();
        cx.refresh_windows();

        let paths = self.paths.clone();
        let account_id = account_id.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            if let Err(error) = storage::perform(&paths, &account_id, StorageOperation::RemoveData)
            {
                warn!(account_id = %account_id, %error, "could not remove account data");
            }
        });
    }

    /// Shared by the Accounts section and a successful storage deletion: the
    /// window and every runtime map are dropped, and the account leaves the
    /// settings. The directory itself is removed separately, after the webview
    /// is gone.
    fn forget_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if let Some(handle) = self.account_windows.remove(account_id) {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.account_xids.remove(account_id);
        self.crash_recovery.remove(account_id);
        self.connection.remove(account_id);
        self.active_downloads.remove(account_id);
        self.hidden_accounts.remove(account_id);
        self.scales.remove(account_id);
        self.gtk.close(account_id);
        self.settings.remove_account(account_id);
    }

    pub fn is_account_window_open(&self, account_id: &str) -> bool {
        self.account_windows.contains_key(account_id)
    }

    /// Opens the folder that contains every account profile.
    pub fn open_storage_folder(&self, cx: &mut Context<Self>) {
        let path = self.paths.data_dir.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = open::that_detached(&path) {
                    warn!(
                        path = %path.display(),
                        %error,
                        "could not open the data folder"
                    );
                }
            })
            .detach();
    }

    /// Scans every account on a worker thread. The previous snapshot stays
    /// visible while the new one is being computed.
    pub fn refresh_storage(&mut self, cx: &mut Context<Self>) {
        if self.storage.scanning {
            return;
        }
        self.storage.scanning = true;
        let paths = self.paths.clone();
        let account_ids: Vec<String> = self
            .settings
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        cx.spawn(async move |this, cx| {
            let snapshot = cx
                .background_executor()
                .spawn(async move { storage::scan_storage(&paths, &account_ids) })
                .await;
            this.update(cx, |model, cx| {
                model.storage.scanning = false;
                model.storage.snapshot = Some(snapshot);
                cx.refresh_windows();
            })
            .ok();
        })
        .detach();
    }

    /// Starts a storage operation. The confirmation policy is enforced by the
    /// settings window; this method only enforces the account-level policy and
    /// hands the request to the GTK thread, which closes the webview before the
    /// worker deletes anything.
    pub fn start_storage_operation(
        &mut self,
        account_id: &str,
        operation: StorageOperation,
        cx: &mut Context<Self>,
    ) {
        if self.storage.running.is_some() {
            self.storage.notice = Some(StorageNotice::error(
                "Another storage operation is already running.",
            ));
            cx.refresh_windows();
            return;
        }
        if let Err(error) = storage::validate_operation(
            operation,
            self.settings.accounts.len(),
            self.settings.account(account_id).is_some(),
        ) {
            self.storage.notice = Some(StorageNotice::error(error.to_string()));
            cx.refresh_windows();
            return;
        }
        self.storage.running = Some(RunningStorageOperation {
            account_id: account_id.to_string(),
            operation,
        });
        self.storage.notice = None;
        self.gtk
            .clear_storage(account_id, operation, self.paths.clone());
        cx.refresh_windows();
    }

    fn handle_storage_finished(
        &mut self,
        account_id: String,
        operation: StorageOperation,
        result: Result<u64, String>,
        cx: &mut Context<Self>,
    ) {
        self.storage.running = None;
        let notice = match result {
            Ok(freed) => {
                self.settings.last_storage_cleanup =
                    Some(crate::settings::format_timestamp(SystemTime::now()));
                match operation {
                    StorageOperation::RemoveData => {
                        self.forget_account(&account_id, cx);
                        StorageNotice::success(format!(
                            "Deleted the account data and removed the account ({} freed).",
                            storage::format_bytes(freed)
                        ))
                    }
                    StorageOperation::ClearCache => {
                        self.recreate_account_webview(&account_id);
                        StorageNotice::success(format!(
                            "Cache cleared ({} freed). The WhatsApp session was kept.",
                            storage::format_bytes(freed)
                        ))
                    }
                    StorageOperation::ClearSession => {
                        self.recreate_account_webview(&account_id);
                        StorageNotice::success(
                            "Signed out. The account was kept; open it and scan the QR code again.",
                        )
                    }
                }
            }
            Err(error) => {
                self.recreate_account_webview(&account_id);
                StorageNotice::error(format!("Could not {}: {error}", operation.label()))
            }
        };
        self.storage.notice = Some(notice);
        self.save_settings(cx);
        self.update_tray();
        self.refresh_storage(cx);
        cx.refresh_windows();
    }

    /// Recreates the GTK overlay after its profile was cleared. A closed GPUI
    /// window is left alone: the next open creates a fresh webview anyway.
    fn recreate_account_webview(&mut self, account_id: &str) {
        if !self.account_windows.contains_key(account_id)
            || self.settings.account(account_id).is_none()
        {
            return;
        }
        let scale = self.scales.get(account_id).copied().unwrap_or(1.0);
        let show = !self.hidden_accounts.contains(account_id) && !self.locked;
        let parent_xid = self.account_xids.get(account_id).copied();
        let url = self
            .pending_urls
            .remove(account_id)
            .unwrap_or_else(|| WHATSAPP_URL.to_string());
        self.crash_recovery.remove(account_id);
        self.gtk.send(GtkCommand::Create {
            account_id: account_id.to_string(),
            parent_xid,
            data_dir: self.paths.account_data_dir(account_id),
            cache_dir: self.paths.account_cache_dir(account_id),
            url,
            html: None,
            show,
            scale,
            zoom: self.settings.zoom,
            writing: self.settings.writing.clone(),
            proxy: self.settings.network.proxy.clone(),
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

    pub fn set_writing(&mut self, mut writing: WritingSettings, cx: &mut Context<Self>) {
        writing.normalize();
        if self.settings.writing == writing {
            return;
        }
        self.settings.writing = writing;
        self.apply_settings(cx);
        self.gtk.set_writing(self.settings.writing.clone());
    }

    pub fn set_network_proxy(&mut self, proxy: ProxySettings, cx: &mut Context<Self>) {
        if self.settings.network.proxy == proxy {
            return;
        }
        self.settings.network.proxy = proxy;
        self.apply_settings(cx);
    }

    pub fn set_shortcut(
        &mut self,
        shortcut: Shortcut,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if Shortcut::parse(&self.settings.boss_key).ok().as_ref() == Some(&shortcut) {
            return Err("The show/hide shortcut must differ from the boss key.".to_string());
        }
        self.settings.shortcut = shortcut.to_string();
        self.apply_settings(cx);
        Ok(())
    }

    pub fn set_boss_key(
        &mut self,
        shortcut: Shortcut,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if Shortcut::parse(&self.settings.shortcut).ok().as_ref() == Some(&shortcut) {
            return Err("The boss key must differ from the show/hide shortcut.".to_string());
        }
        self.settings.boss_key = shortcut.to_string();
        self.apply_settings(cx);
        Ok(())
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
        self.settings.lock.lock_on_suspend = false;
        self.settings.lock.lock_on_screen_lock = false;
        self.settings.lock.idle_minutes = 0;
        self.apply_settings(cx);
    }

    pub fn set_automatic_update_checks(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.update_settings(cx, |settings| {
            settings.automatic_update_checks = enabled;
        });
        if enabled {
            self.start_update_check();
        }
    }

    pub fn skip_update(&mut self, cx: &mut Context<Self>) {
        let Some(version) = self
            .update_state
            .available
            .as_ref()
            .map(|info| info.version.clone())
        else {
            return;
        };
        self.update_state.available = None;
        self.update_state.skipped_version = Some(version.clone());
        self.update_settings(cx, |settings| {
            settings.skipped_update_version = Some(version);
        });
    }

    pub fn open_update_release(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self
            .update_state
            .available
            .as_ref()
            .map(|info| info.url.clone())
        else {
            return;
        };
        Self::open_external(cx, &url);
    }

    pub fn update_settings(&mut self, cx: &mut Context<Self>, update: impl FnOnce(&mut Settings)) {
        update(&mut self.settings);
        self.settings.normalize();
        self.apply_settings(cx);
    }

    /// Replaces the settings with a validated import. Accounts that disappear
    /// keep their local WebKit profiles on disk; only their windows are closed.
    pub fn apply_imported_settings(&mut self, mut settings: Settings, cx: &mut Context<Self>) {
        settings.normalize();
        let removed: Vec<String> = self
            .settings
            .accounts
            .iter()
            .filter(|account| settings.account(&account.id).is_none())
            .map(|account| account.id.clone())
            .collect();
        for account_id in removed {
            if let Some(handle) = self.account_windows.remove(&account_id) {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
            self.account_xids.remove(&account_id);
            self.crash_recovery.remove(&account_id);
            self.hidden_accounts.remove(&account_id);
            self.scales.remove(&account_id);
            self.gtk.close(&account_id);
        }
        self.settings = settings;
        self.release_lock_without_password(cx);
        self.apply_settings(cx);
    }

    /// Restores preference defaults while keeping the configured accounts (and
    /// therefore their WhatsApp sessions).
    pub fn reset_settings(&mut self, cx: &mut Context<Self>) {
        let mut settings = Settings::default();
        settings.accounts = self.settings.accounts.clone();
        settings.active_account = self.settings.active_account.clone();
        settings.normalize();
        self.settings = settings;
        self.release_lock_without_password(cx);
        self.apply_settings(cx);
    }

    /// Importing or resetting may remove the lock password while the app is
    /// locked. Without a hash the lock window could never accept a password,
    /// so the lock is released instead of trapping the user.
    fn release_lock_without_password(&mut self, cx: &mut Context<Self>) {
        if self.locked && self.settings.lock.hash.is_none() {
            self.locked = false;
            if let Some(handle) = self.lock_window.take() {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
            self.show_all(cx);
        }
    }

    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        self.save_settings(cx);
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
        self.apply_hotkeys();
        self.gtk.set_proxy(self.settings.network.proxy.clone());
        self.update_tray();
        cx.refresh_windows();
    }

    fn apply_hotkeys(&self) {
        if let Ok(shortcut) = Shortcut::parse(&self.settings.shortcut) {
            self.hotkey
                .set(hotkey_bindings(shortcut, &self.settings.boss_key));
        }
    }

    /// Coalesces a settings change and writes it after a short quiet period.
    /// No disk I/O happens on the GPUI thread.
    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let Some(generation) = self.save_debounce.mark_dirty() else {
            return;
        };
        cx.spawn(async move |model, cx| {
            Timer::after(SETTINGS_SAVE_DEBOUNCE).await;
            let _ = model.update(cx, |model, _cx| {
                if model.save_debounce.timer_fired(generation) {
                    model.flush_settings();
                }
            });
        })
        .detach();
    }

    fn flush_settings(&mut self) {
        self.settings_writer
            .save(self.settings.clone(), self.paths.settings_file.clone());
    }

    /// Writes the current settings before the process exits. This is the only
    /// path that waits for the writer.
    fn save_settings_now(&mut self) {
        self.save_debounce.clear();
        self.settings_writer
            .save_and_wait(self.settings.clone(), self.paths.settings_file.clone());
    }

    fn open_external(cx: &mut Context<Self>, url: &str) {
        let url = url.to_string();
        cx.background_executor()
            .spawn(async move {
                // open 5.4 probes each launcher for up to 250 ms, so it must
                // never run on the foreground thread.
                links::open_external(&url);
            })
            .detach();
    }

    fn update_account_title(&self, account_id: &str, cx: &mut Context<Self>) {
        let name = self.account_name(account_id);
        let unread = self.account_unread(account_id);
        let base_title = account_window_title(&name);
        let title = if unread > 0 {
            format!("{base_title} ({unread})")
        } else {
            base_title
        };
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, window, _| window.set_window_title(&title));
        }
    }

    fn update_tray(&self) {
        let unread = self.settings.total_unread();
        let visible = self.any_account_visible();
        if let Some(tray) = &self.tray {
            tray.update(
                &self.settings.accounts,
                unread,
                visible,
                self.settings.do_not_disturb,
            );
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

    /// Tries to tag the GPUI window without blocking. Returns `Pending` when
    /// the window is not in the X11 tree yet so the caller can retry on a
    /// timer instead of sleeping on the foreground thread.
    fn try_tag_account(
        &mut self,
        account_id: &str,
        expected_title: &str,
        known_windows: &[u32],
    ) -> Result<AccountTag> {
        if !self.account_windows.contains_key(account_id) {
            return Ok(AccountTag::Closed);
        }
        let candidates: Vec<WindowInfo> = self
            .x11
            .account_window_candidates()?
            .into_iter()
            .map(|window| WindowInfo {
                window,
                title: self.x11.title_of(window),
                account_tag: self.x11.account_for(window),
            })
            .collect();
        let Some(xid) =
            select_account_window(known_windows, &candidates, account_id, expected_title)
        else {
            return Ok(AccountTag::Pending);
        };

        self.x11.tag_account(xid, account_id)?;
        self.account_xids.insert(account_id.to_string(), xid);
        if self.hidden_accounts.contains(account_id) {
            if let Err(error) = self.x11.unmap(xid) {
                warn!(account_id = %account_id, %error, "could not hide the tagged GPUI window");
            }
        }
        Ok(AccountTag::Tagged)
    }

    fn schedule_account_tag(
        &mut self,
        account_id: &str,
        expected_title: &str,
        known_windows: &[u32],
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let id = account_id.to_string();
        let title = expected_title.to_string();
        let known_windows = known_windows.to_vec();
        cx.spawn(async move |_, cx| {
            for _ in 0..ACCOUNT_TAG_ATTEMPTS {
                Timer::after(ACCOUNT_TAG_RETRY_INTERVAL).await;
                match weak.update(cx, |model, _| {
                    model.try_tag_account(&id, &title, &known_windows)
                }) {
                    Ok(Ok(AccountTag::Tagged | AccountTag::Closed)) => return,
                    Ok(Ok(AccountTag::Pending)) => {}
                    Ok(Err(error)) => {
                        warn!(account_id = %id, %error, "could not tag the GPUI window");
                        return;
                    }
                    Err(_) => return,
                }
            }
            warn!(account_id = %id, "the account window was not found in the X11 tree");
        })
        .detach();
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
        self.crash_recovery.clear();
        self.connection.clear();
        self.active_downloads.clear();

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
        self.save_settings_now();
        self.downloads_writer.save_and_wait(
            self.downloads.snapshot(),
            self.downloads.file().to_path_buf(),
        );
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

/// Title GPUI assigns to an account window before the unread suffix is added.
/// Window discovery matches this exact prefix, so keep the format in sync with
/// `AccountWindow::render`.
fn account_window_title(name: &str) -> String {
    format!("OxideZap — {name}")
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
            title: Some(SharedString::from(account_window_title(&account.name))),
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

    let known_windows = model
        .read(cx)
        .x11
        .account_window_candidates()
        .unwrap_or_else(|error| {
            debug!(%error, "could not snapshot the account windows before opening one");
            Vec::new()
        });
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
        model.finish_open_account(account, handle, html, known_windows, cx);
    });
}

fn open_settings_window_deferred(model: Entity<AppModel>, cx: &mut App) {
    let hidden_or_locked = {
        let model = model.read(cx);
        model.locked || model.boss_hidden.is_some()
    };
    if hidden_or_locked {
        model.update(cx, |model, _| model.pending_settings = false);
        return;
    }
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

fn open_downloads_window_deferred(model: Entity<AppModel>, cx: &mut App) {
    if model.read(cx).locked {
        model.update(cx, |model, _| model.pending_downloads = false);
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1040.0), px(640.0)), cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from("OxideZap Downloads")),
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
        app_id: Some("oxidezap-downloads".to_string()),
        window_min_size: Some(size(px(820.0), px(480.0))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
    };

    let model_for_view = model.clone();
    match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id("oxidezap-downloads");
        cx.new(|cx| DownloadsWindow::new(model_for_view.clone(), cx))
    }) {
        Ok(handle) => {
            model.update(cx, |model, _| {
                model.pending_downloads = false;
                model.downloads_window = Some(handle);
            });
        }
        Err(error) => {
            error!(%error, "could not open the downloads window");
            model.update(cx, |model, _| model.pending_downloads = false);
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

/// Resolve `--account` by exact id first, then by exact name, then by a
/// case-insensitive name. Ambiguous names fail instead of picking one.
pub fn resolve_account(settings: &Settings, query: &str) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("an account id or name is required".to_string());
    }
    if let Some(account) = settings.accounts.iter().find(|account| account.id == query) {
        return Ok(account.id.clone());
    }

    let exact: Vec<&Account> = settings
        .accounts
        .iter()
        .filter(|account| account.name == query)
        .collect();
    match exact.as_slice() {
        [account] => return Ok(account.id.clone()),
        [] => {}
        _ => return Err(ambiguous_account_message(query, &exact)),
    }

    let folded: Vec<&Account> = settings
        .accounts
        .iter()
        .filter(|account| account.name.eq_ignore_ascii_case(query))
        .collect();
    match folded.as_slice() {
        [account] => Ok(account.id.clone()),
        [] => {
            let available = if settings.accounts.is_empty() {
                "no accounts are configured".to_string()
            } else {
                format!(
                    "available accounts: {}",
                    settings
                        .accounts
                        .iter()
                        .map(|account| format!("{} ({})", single_line(&account.name), account.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            Err(format!("account '{query}' was not found; {available}"))
        }
        _ => Err(ambiguous_account_message(query, &folded)),
    }
}

fn ambiguous_account_message(query: &str, accounts: &[&Account]) -> String {
    let ids = accounts
        .iter()
        .map(|account| format!("{} ({})", single_line(&account.name), account.id))
        .collect::<Vec<_>>()
        .join(", ");
    format!("account name '{query}' is ambiguous; use an id: {ids}")
}

/// Stable `key: value` output for `--status`. Every line has the same order so
/// scripts can read it with `sed`, `grep`, or `cut`.
pub fn status_text(settings: &Settings, running: bool, windows: usize) -> String {
    let active = settings
        .active_account_id()
        .map(|id| single_line(&id))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "version: {}\nrunning: {}\naccounts: {}\nactive_account: {}\ndnd: {}\nwindows: {}\n",
        env!("CARGO_PKG_VERSION"),
        if running { "true" } else { "false" },
        settings.accounts.len(),
        active,
        if settings.do_not_disturb { "on" } else { "off" },
        windows
    )
}

/// One tab-separated line per account for `--list-accounts`: id, name, and
/// whether the account is active. Names are flattened to one line so a stray
/// newline cannot forge an extra row.
pub fn account_list_text(settings: &Settings) -> String {
    let active = settings.active_account_id();
    let mut output = String::new();
    for account in &settings.accounts {
        let state = if active.as_deref() == Some(account.id.as_str()) {
            "active"
        } else {
            "inactive"
        };
        output.push_str(&format!(
            "{}\t{}\t{}\n",
            single_line(&account.id),
            single_line(&account.name),
            state
        ));
    }
    output
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_retries_back_off_then_stop() {
        let mut now = Instant::now();
        let mut recovery = CrashRecovery::default();

        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(1))
        );
        assert_eq!(recovery.next_retry, Some(now + Duration::from_secs(1)));
        now += Duration::from_secs(1);
        assert!(recovery.take_due_retry(now));

        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(3))
        );
        assert_eq!(recovery.next_retry, Some(now + Duration::from_secs(3)));
        now += Duration::from_secs(3);
        assert!(recovery.take_due_retry(now));

        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(10))
        );
        assert_eq!(recovery.next_retry, Some(now + Duration::from_secs(10)));
        now += Duration::from_secs(10);
        assert!(recovery.take_due_retry(now));

        assert_eq!(recovery.on_crash(now), CrashDecision::Exhausted);
        assert!(recovery.failed);
        assert_eq!(recovery.attempts, MAX_CRASH_ATTEMPTS);
        assert_eq!(recovery.next_retry, None);
        assert_eq!(recovery.on_crash(now), CrashDecision::Ignored);
    }

    #[test]
    fn crash_while_a_retry_is_pending_is_ignored() {
        let now = Instant::now();
        let mut recovery = CrashRecovery::default();

        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(1))
        );
        assert_eq!(recovery.on_crash(now), CrashDecision::Ignored);
        assert_eq!(recovery.attempts, 1);
    }

    #[test]
    fn scheduled_retry_fires_only_when_due_and_only_once() {
        let now = Instant::now();
        let mut recovery = CrashRecovery::default();
        recovery.on_crash(now);

        assert!(!recovery.take_due_retry(now));
        assert!(recovery.take_due_retry(now + Duration::from_secs(1)));
        assert!(!recovery.take_due_retry(now + Duration::from_secs(2)));
    }

    #[test]
    fn successful_load_resets_the_recovery_budget() {
        let now = Instant::now();
        let mut recovery = CrashRecovery::default();
        recovery.on_crash(now);
        recovery.take_due_retry(now + Duration::from_secs(1));
        recovery.on_crash(now);

        recovery.reset();

        assert_eq!(recovery, CrashRecovery::default());
        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn manual_retry_restarts_the_budget_after_exhaustion() {
        let now = Instant::now();
        let mut recovery = CrashRecovery::default();
        for _ in 0..MAX_CRASH_ATTEMPTS {
            let expected = CRASH_BACKOFF[recovery.attempts as usize];
            assert_eq!(recovery.on_crash(now), CrashDecision::RetryAfter(expected));
            recovery.take_due_retry(now + Duration::from_secs(10));
        }
        assert_eq!(recovery.on_crash(now), CrashDecision::Exhausted);

        recovery.reset();

        assert!(!recovery.failed);
        assert_eq!(recovery.attempts, 0);
        assert_eq!(
            recovery.on_crash(now),
            CrashDecision::RetryAfter(Duration::from_secs(1))
        );
    }

    #[test]
    fn crashes_are_ignored_while_quitting() {
        let now = Instant::now();
        let mut recovery = CrashRecovery::default();

        assert_eq!(
            crash_decision(true, &mut recovery, now),
            CrashDecision::Ignored
        );
        assert_eq!(recovery, CrashRecovery::default());
    }

    #[test]
    fn account_window_title_is_the_discovery_identity() {
        assert_eq!(account_window_title("Work"), "OxideZap — Work");
    }

    fn online_tracker(now: Instant) -> ConnectionTracker {
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);
        tracker.on_load_finished(now);
        assert_eq!(tracker.state(), ConnectionState::Connected);
        tracker
    }

    #[test]
    fn connection_tracker_reports_offline_and_reconnected() {
        let now = Instant::now();
        let mut tracker = online_tracker(now);

        assert_eq!(
            tracker.on_browser(now, false, false),
            ConnectionDecision::Offline
        );
        assert_eq!(tracker.state(), ConnectionState::Offline);
        assert_eq!(
            tracker.on_browser(now, false, false),
            ConnectionDecision::None
        );
        assert_eq!(
            tracker.on_browser(now, true, false),
            ConnectionDecision::Reconnected
        );
        assert_eq!(tracker.state(), ConnectionState::Connected);
        assert_eq!(
            tracker.on_browser(now, true, false),
            ConnectionDecision::None
        );
    }

    #[test]
    fn finished_load_without_a_browser_report_does_not_report_an_outage() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_load_started(now);

        assert_eq!(tracker.on_load_finished(now), ConnectionDecision::None);

        assert_eq!(tracker.state(), ConnectionState::Connected);
        assert!(!tracker.offline_notified);
    }

    #[test]
    fn load_failure_schedules_exponential_backoff() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);

        let mut at = now;
        assert_eq!(tracker.on_load_failed(at), ConnectionDecision::None);
        assert_eq!(tracker.state(), ConnectionState::Reconnecting);
        assert_eq!(tracker.attempts, 1);
        assert_eq!(tracker.next_retry, Some(at + Duration::from_secs(2)));
        assert_eq!(tracker.take_due_retry(at, false), RetryAction::Idle);

        for (index, delay) in RECONNECT_BACKOFF.iter().enumerate() {
            if index > 0 {
                assert_eq!(tracker.on_load_failed(at), ConnectionDecision::None);
                assert_eq!(tracker.attempts, index as u32 + 1);
                assert_eq!(tracker.next_retry, Some(at + *delay));
            }
            at += *delay;
            assert_eq!(tracker.take_due_retry(at, false), RetryAction::Fire);
            assert_eq!(tracker.state(), ConnectionState::Reconnecting);
        }

        assert_eq!(tracker.on_load_failed(at), ConnectionDecision::Exhausted);
        assert!(tracker.exhausted);
        assert_eq!(tracker.next_retry, None);
        assert_eq!(tracker.state(), ConnectionState::Offline);
        assert_eq!(tracker.on_load_failed(at), ConnectionDecision::None);
    }

    #[test]
    fn retry_waits_for_active_downloads_without_consuming_an_attempt() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);
        tracker.on_load_failed(now);
        let retry_at = now + Duration::from_secs(2);

        assert_eq!(
            tracker.take_due_retry(retry_at, true),
            RetryAction::Postponed
        );
        assert_eq!(tracker.attempts, 1);
        assert_eq!(tracker.next_retry, Some(retry_at));
        assert_eq!(tracker.take_due_retry(retry_at, false), RetryAction::Fire);
    }

    #[test]
    fn login_page_never_reloads_automatically() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, true);
        tracker.on_load_started(now);

        assert_eq!(
            tracker.on_load_failed(now),
            ConnectionDecision::LoginPageBlocked
        );
        assert_eq!(tracker.state(), ConnectionState::Offline);
        assert_eq!(tracker.next_retry, None);
        assert_eq!(tracker.attempts, 0);
        assert_eq!(tracker.take_due_retry(now, false), RetryAction::Idle);
    }

    #[test]
    fn login_page_cancels_an_already_scheduled_retry() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);
        tracker.on_load_failed(now);
        assert_eq!(tracker.next_retry, Some(now + Duration::from_secs(2)));

        assert_eq!(
            tracker.on_browser(now, true, true),
            ConnectionDecision::LoginPageBlocked
        );
        assert_eq!(tracker.next_retry, None);
        assert_eq!(
            tracker.take_due_retry(now + Duration::from_secs(2), false),
            RetryAction::Idle
        );
    }

    #[test]
    fn load_timeout_is_the_native_fallback() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);

        assert_eq!(
            tracker.on_load_timeout(now + LOAD_TIMEOUT - Duration::from_secs(1)),
            ConnectionDecision::None
        );
        assert_eq!(
            tracker.on_load_timeout(now + LOAD_TIMEOUT),
            ConnectionDecision::None
        );
        assert_eq!(tracker.state(), ConnectionState::Reconnecting);
        assert_eq!(
            tracker.next_retry,
            Some(now + LOAD_TIMEOUT + Duration::from_secs(2))
        );
    }

    #[test]
    fn an_internal_navigation_does_not_arm_the_load_timeout() {
        let now = Instant::now();
        let mut tracker = online_tracker(now);

        tracker.on_load_started(now + Duration::from_secs(1));

        assert_eq!(tracker.load_started, None);
        assert_eq!(
            tracker.on_load_timeout(now + LOAD_TIMEOUT),
            ConnectionDecision::None
        );
        assert_eq!(tracker.state(), ConnectionState::Connected);
    }

    #[test]
    fn successful_retry_resets_the_budget() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);
        tracker.on_load_failed(now);
        let retry_at = now + Duration::from_secs(2);
        assert_eq!(tracker.take_due_retry(retry_at, false), RetryAction::Fire);

        tracker.on_load_started(retry_at);
        assert_eq!(
            tracker.on_load_finished(retry_at),
            ConnectionDecision::Reconnected
        );

        assert_eq!(tracker.state(), ConnectionState::Connected);
        assert_eq!(tracker.attempts, 0);
        assert!(!tracker.exhausted);
        assert_eq!(tracker.next_retry, None);
    }

    #[test]
    fn manual_retry_restarts_an_exhausted_connection_budget() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.on_load_started(now);
        assert_eq!(tracker.on_load_failed(now), ConnectionDecision::None);
        let mut at = now;
        for (index, delay) in RECONNECT_BACKOFF.iter().enumerate() {
            at += *delay;
            assert_eq!(tracker.take_due_retry(at, false), RetryAction::Fire);
            let decision = tracker.on_load_failed(at);
            if index + 1 == RECONNECT_BACKOFF.len() {
                assert_eq!(decision, ConnectionDecision::Exhausted);
            } else {
                assert_eq!(decision, ConnectionDecision::None);
            }
        }
        assert!(tracker.exhausted);

        tracker.retry_now(at);

        assert_eq!(tracker.state(), ConnectionState::Reconnecting);
        assert_eq!(tracker.attempts, 0);
        assert!(!tracker.exhausted);
        assert_eq!(tracker.next_retry, None);
        assert_eq!(tracker.on_load_failed(at), ConnectionDecision::None);
        assert_eq!(tracker.next_retry, Some(at + Duration::from_secs(2)));
    }

    #[test]
    fn blocked_reload_returns_to_offline() {
        let now = Instant::now();
        let mut tracker = ConnectionTracker::default();
        tracker.on_browser(now, true, false);
        tracker.retry_now(now);
        assert_eq!(tracker.state(), ConnectionState::Reconnecting);

        tracker.on_reload_blocked();

        assert_eq!(tracker.state(), ConnectionState::Offline);
        assert_eq!(tracker.load_started, None);
    }

    fn settings_with_accounts() -> Settings {
        Settings {
            accounts: vec![
                Account::with_id("a", "Work"),
                Account::with_id("b", "work"),
                Account::with_id("c", "Home"),
            ],
            active_account: Some("a".to_string()),
            ..Settings::default()
        }
    }

    #[test]
    fn resolves_accounts_by_id_name_and_case() {
        let settings = settings_with_accounts();
        assert_eq!(resolve_account(&settings, "a").unwrap(), "a");
        assert_eq!(resolve_account(&settings, "Work").unwrap(), "a");
        assert_eq!(resolve_account(&settings, "Home").unwrap(), "c");
        assert_eq!(resolve_account(&settings, "home").unwrap(), "c");
        assert!(
            resolve_account(&settings, "missing")
                .unwrap_err()
                .contains("was not found")
        );
        assert!(resolve_account(&settings, "  ").is_err());
    }

    #[test]
    fn ambiguous_account_names_are_rejected() {
        let settings = settings_with_accounts();
        let error = resolve_account(&settings, "WORK").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("Work (a)"), "{error}");
        assert!(error.contains("work (b)"), "{error}");
    }

    #[test]
    fn status_output_is_stable_and_script_friendly() {
        let settings = Settings {
            accounts: vec![Account::with_id("a", "Work"), Account::with_id("b", "Home")],
            active_account: Some("a".to_string()),
            do_not_disturb: true,
            ..Settings::default()
        };
        assert_eq!(
            status_text(&settings, true, 2),
            format!(
                "version: {}\nrunning: true\naccounts: 2\nactive_account: a\ndnd: on\nwindows: 2\n",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(
            status_text(&settings, false, 0),
            format!(
                "version: {}\nrunning: false\naccounts: 2\nactive_account: a\ndnd: on\nwindows: 0\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn account_list_output_is_tab_separated() {
        let settings = Settings {
            accounts: vec![
                Account::with_id("a", "Work"),
                Account::with_id("b", "Bad\nName"),
            ],
            active_account: Some("b".to_string()),
            ..Settings::default()
        };
        assert_eq!(
            account_list_text(&settings),
            "a\tWork\tinactive\nb\tBad Name\tactive\n"
        );
        assert_eq!(account_list_text(&Settings::default()), "");
    }

    fn configured_lock() -> LockSettings {
        LockSettings {
            enabled: true,
            hash: Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def".to_string()),
            ..LockSettings::default()
        }
    }

    #[test]
    fn lock_triggers_follow_the_security_toggles() {
        let lock = configured_lock();

        assert!(lock_on_trigger(LockTrigger::Manual, &lock));
        assert!(lock_on_trigger(LockTrigger::Idle, &lock));
        assert!(lock_on_trigger(LockTrigger::Suspend, &lock));
        assert!(lock_on_trigger(LockTrigger::Resume, &lock));
        assert!(lock_on_trigger(LockTrigger::ScreenLocked, &lock));

        let mut lock = configured_lock();
        lock.lock_on_suspend = false;
        assert!(!lock_on_trigger(LockTrigger::Suspend, &lock));
        assert!(!lock_on_trigger(LockTrigger::Resume, &lock));
        assert!(lock_on_trigger(LockTrigger::ScreenLocked, &lock));

        lock.lock_on_screen_lock = false;
        assert!(!lock_on_trigger(LockTrigger::ScreenLocked, &lock));

        lock.lock_on_hide = false;
        assert!(!lock_on_trigger(LockTrigger::Hide, &lock));
        lock.lock_on_hide = true;
        assert!(lock_on_trigger(LockTrigger::Hide, &lock));
    }

    #[test]
    fn a_disabled_or_missing_password_never_locks() {
        let mut lock = configured_lock();
        lock.hash = None;
        assert!(!lock_on_trigger(LockTrigger::Manual, &lock));
        assert!(!lock_on_trigger(LockTrigger::Suspend, &lock));

        let mut lock = configured_lock();
        lock.enabled = false;
        assert!(!lock_on_trigger(LockTrigger::Manual, &lock));
        assert!(!lock_on_trigger(LockTrigger::ScreenLocked, &lock));
    }

    #[test]
    fn an_already_locked_session_ignores_new_triggers() {
        let lock = configured_lock();

        assert!(should_begin_lock(false, LockTrigger::Suspend, &lock));
        assert!(!should_begin_lock(true, LockTrigger::Suspend, &lock));
        assert!(!should_begin_lock(true, LockTrigger::Manual, &lock));
    }

    #[test]
    fn unlock_requires_the_configured_password() {
        let hash = crate::lock::hash_password("correct horse").unwrap();

        assert!(unlock_is_authorized(Some(&hash), "correct horse"));
        assert!(!unlock_is_authorized(Some(&hash), "wrong password"));
        assert!(!unlock_is_authorized(None, "correct horse"));
    }

    #[test]
    fn prepare_for_sleep_maps_to_lock_events() {
        assert_eq!(logind_event(true), SystemLockEvent::Suspend);
        assert_eq!(logind_event(false), SystemLockEvent::Resume);
    }

    #[test]
    fn screen_saver_interfaces_are_recognized() {
        assert!(is_screen_saver_interface("org.freedesktop.ScreenSaver"));
        assert!(is_screen_saver_interface("org.gnome.ScreenSaver"));
        assert!(is_screen_saver_interface("org.cinnamon.ScreenSaver"));
        assert!(is_screen_saver_interface("org.kde.screensaver"));
        assert!(!is_screen_saver_interface("org.freedesktop.login1.Manager"));
        assert!(!is_screen_saver_interface("org.freedesktop.Notifications"));
    }
}
