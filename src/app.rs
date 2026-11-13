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
    App, Bounds, Context, Entity, Pixels, SharedString, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    WindowOptions, point, prelude::*, px, size,
};
use tracing::{debug, error, info, warn};
use zbus::MatchRule;
use zbus::blocking::MessageIterator;
use zbus::message::Type as MessageType;

use crate::autostart;
use crate::badge::Badge;
use crate::diagnostics::{self, DiagnosticLevel, DiagnosticsContext, ErrorLog, RuntimeSummary};
use crate::hotkey::{HotkeyAction, HotkeyBindings, HotkeyHandle};
use crate::links;
use crate::lock;
use crate::notify;
use crate::paths::{APP_ID, Paths};
use crate::settings::performance::{
    POWER_POLL_INTERVAL, PerformanceCadence, PerformanceStatus, PowerState, detect_power_state,
    effective_memory,
};
use crate::settings::storage::{self, StorageOperation, StorageSnapshot};
use crate::settings::{
    Account, AccountIdentity, AccountTheme, AppearanceSettings, DoNotDisturbSettings, LockSettings,
    MemorySettings, MonitorArea, ONBOARDING_VERSION, ProxySettings, SETTINGS_SAVE_DEBOUNCE,
    SaveDebounce, SavedBounds, Settings, SettingsWriter, WritingSettings, monitor_for_bounds,
    restore_account_bounds, should_show_onboarding, validate_custom_css,
};
use crate::shortcut::Shortcut;
use crate::single_instance::{
    ChatTarget, DndMode, InstanceListener, InstanceMessage, InstanceReply, InstanceRequest,
};
use crate::theme::Theme;
use crate::tray::{TrayAction, TrayDndState, TrayHandle, TrayWindowState};
use crate::ui::account_window::AccountWindow;
use crate::ui::command_palette::{
    COMMAND_PALETTE_HEIGHT, COMMAND_PALETTE_TOP_OFFSET, COMMAND_PALETTE_WIDTH, CommandAccount,
    CommandAction, CommandContext, CommandPalette, account_at, zoom_after,
};
use crate::ui::downloads_window::DownloadsWindow;
use crate::ui::lock_window::LockWindow;
use crate::ui::onboarding_window::OnboardingWindow;
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
const DEFAULT_WINDOW_WIDTH: f32 = 1180.0;
const DEFAULT_WINDOW_HEIGHT: f32 = 780.0;
const NORMAL_MIN_WIDTH: f32 = 760.0;
const NORMAL_MIN_HEIGHT: f32 = 520.0;
const MINI_WINDOW_WIDTH: f32 = 420.0;
const MINI_WINDOW_HEIGHT: f32 = 620.0;
const MINI_MIN_WIDTH: f32 = 360.0;
const MINI_MIN_HEIGHT: f32 = 420.0;

enum AccountTag {
    Tagged,
    Pending,
    Closed,
}

fn saved_bounds(bounds: Bounds<Pixels>) -> SavedBounds {
    SavedBounds {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

fn window_bounds(bounds: &SavedBounds) -> WindowBounds {
    WindowBounds::Windowed(Bounds::new(
        point(px(bounds.x), px(bounds.y)),
        size(px(bounds.width), px(bounds.height)),
    ))
}

fn default_saved_bounds(monitors: &[MonitorArea]) -> SavedBounds {
    let area = monitors
        .first()
        .map(|monitor| monitor.bounds.clone())
        .unwrap_or(SavedBounds {
            x: 0.0,
            y: 0.0,
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        });
    let width = DEFAULT_WINDOW_WIDTH.min(area.width);
    let height = DEFAULT_WINDOW_HEIGHT.min(area.height);
    SavedBounds {
        x: area.x + (area.width - width).max(0.0) / 2.0,
        y: area.y + (area.height - height).max(0.0) / 2.0,
        width,
        height,
    }
}

fn mini_bounds(source: Option<&SavedBounds>, monitors: &[MonitorArea]) -> SavedBounds {
    let area = source
        .and_then(|bounds| monitor_for_bounds(bounds, monitors))
        .map(|monitor| monitor.bounds.clone())
        .or_else(|| monitors.first().map(|monitor| monitor.bounds.clone()))
        .unwrap_or(SavedBounds {
            x: 0.0,
            y: 0.0,
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
        });
    SavedBounds {
        x: source.map(|bounds| bounds.x).unwrap_or(area.x),
        y: source.map(|bounds| bounds.y).unwrap_or(area.y),
        width: MINI_WINDOW_WIDTH,
        height: MINI_WINDOW_HEIGHT,
    }
    .clamp_to(&area)
}

fn clamp_to_available_monitors(bounds: &SavedBounds, monitors: &[MonitorArea]) -> SavedBounds {
    if monitors.is_empty() {
        return bounds.clone();
    }
    monitor_for_bounds(bounds, monitors)
        .map(|monitor| bounds.clamp_to(&monitor.bounds))
        .unwrap_or_else(|| bounds.clamp_to(&monitors[0].bounds))
}

/// The monitor list GPUI exposes. X11 reports one screen for the whole virtual
/// desktop, so `restore_account_bounds` treats it as a single monitor there and
/// still clamps windows into it; platforms with several displays get one entry
/// per display and are restored per monitor.
fn monitor_areas(cx: &App) -> Vec<MonitorArea> {
    cx.displays()
        .iter()
        .enumerate()
        .map(|(index, display)| {
            let key = display
                .uuid()
                .map(|uuid| uuid.to_string())
                .unwrap_or_else(|_| format!("display-{index}"));
            MonitorArea {
                key,
                bounds: saved_bounds(display.bounds()),
            }
        })
        .collect()
}

/// Sends an EWMH `_NET_WM_STATE` client message to add or remove up to two
/// states. It is used for always-on-top and to leave maximized mode before a
/// mini window is resized.
fn set_wm_state(
    x11: &X11,
    window: u32,
    enabled: bool,
    first: &[u8],
    second: Option<&[u8]>,
) -> Result<()> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xproto::{ClientMessageEvent, EventMask};

    let state_atom = x11.conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
    let first_atom = x11.conn.intern_atom(false, first)?.reply()?.atom;
    let second_atom = match second {
        Some(name) => x11.conn.intern_atom(false, name)?.reply()?.atom,
        None => 0,
    };
    // 0 removes, 1 adds, 2 toggles. Source indication 1 is a normal application.
    let action = u32::from(enabled);
    let event = ClientMessageEvent::new(
        32,
        window,
        state_atom,
        [action, first_atom, second_atom, 1, 0],
    );
    x11.conn.send_event(
        false,
        x11.root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    x11.conn.flush()?;
    Ok(())
}

fn set_always_on_top_state(x11: &X11, window: u32, enabled: bool) -> Result<()> {
    set_wm_state(x11, window, enabled, b"_NET_WM_STATE_ABOVE", None)
}

fn leave_maximized_state(x11: &X11, window: u32) -> Result<()> {
    set_wm_state(
        x11,
        window,
        false,
        b"_NET_WM_STATE_MAXIMIZED_VERT",
        Some(b"_NET_WM_STATE_MAXIMIZED_HORZ"),
    )
}

fn set_window_min_size(x11: &X11, window: u32, width: f32, height: f32) -> Result<()> {
    use x11rb::connection::Connection as _;
    use x11rb::properties::WmSizeHints;

    let mut hints = WmSizeHints::get_normal_hints(&x11.conn, window)?
        .reply()?
        .unwrap_or_default();
    hints.min_size = Some((
        width.round().max(1.0) as i32,
        height.round().max(1.0) as i32,
    ));
    hints.set_normal_hints(&x11.conn, window)?.check()?;
    x11.conn.flush()?;
    Ok(())
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

fn hotkey_bindings(toggle: Shortcut, boss_key: &str, command_palette: &str) -> HotkeyBindings {
    HotkeyBindings::new(
        toggle,
        Shortcut::parse(boss_key).ok(),
        Shortcut::parse(command_palette).ok(),
    )
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

/// Conservative estimate of the memory a single suspended WebKit web process
/// releases. WebKit does not expose per-view RSS, so the Settings window labels
/// this as an estimate instead of pretending it is a measurement.
pub const ESTIMATED_SUSPENDED_MEMORY_BYTES: u64 = 150 * 1024 * 1024;

/// Whether an account currently has a live WebKit view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccountViewState {
    #[default]
    Loaded,
    Suspended,
    Resuming,
}

impl AccountViewState {
    pub fn is_suspended(self) -> bool {
        self == AccountViewState::Suspended
    }

    pub fn is_loaded(self) -> bool {
        self == AccountViewState::Loaded
    }
}

/// Per-account view lifecycle. A missing entry is `Loaded`, which matches the
/// state of an account whose window has just been opened and whose `Create`
/// command is still in flight.
#[derive(Default)]
struct ViewLifecycle {
    states: HashMap<String, AccountViewState>,
}

impl ViewLifecycle {
    fn state(&self, account_id: &str) -> AccountViewState {
        self.states.get(account_id).copied().unwrap_or_default()
    }

    /// Marks a loaded view as suspended. Returns `false` when the account is
    /// already suspended or is mid-restore, so a sweep cannot queue a second
    /// `Suspend` command for the same account.
    fn begin_suspend(&mut self, account_id: &str) -> bool {
        if self.state(account_id) != AccountViewState::Loaded {
            return false;
        }
        self.states
            .insert(account_id.to_string(), AccountViewState::Suspended);
        true
    }

    /// Starts a restore. Returns `false` when there is nothing to restore or a
    /// restore is already in flight, which keeps a double click from creating a
    /// duplicate WebKit view.
    fn begin_resume(&mut self, account_id: &str) -> bool {
        if self.state(account_id) != AccountViewState::Suspended {
            return false;
        }
        self.states
            .insert(account_id.to_string(), AccountViewState::Resuming);
        true
    }

    /// The GTK thread created the view (or finished loading it). Only a
    /// pending restore changes state; a stale `Ready`/`Finished` event for a
    /// view that was already suspended must not revive it.
    fn mark_loaded(&mut self, account_id: &str) {
        if self.state(account_id) == AccountViewState::Resuming {
            self.states.remove(account_id);
        }
    }

    /// The view is known to be alive even though the application marked it
    /// suspended (for example after a refused suspend command).
    fn mark_active(&mut self, account_id: &str) {
        self.states.remove(account_id);
    }

    /// The view could not be recreated. Return to the suspended placeholder so
    /// the user can retry instead of being stuck in a loading state.
    fn mark_failed(&mut self, account_id: &str) {
        if self.state(account_id) == AccountViewState::Resuming {
            self.states
                .insert(account_id.to_string(), AccountViewState::Suspended);
        }
    }

    fn forget(&mut self, account_id: &str) {
        self.states.remove(account_id);
    }

    fn suspended_ids(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|(_, state)| **state == AccountViewState::Suspended)
            .map(|(account_id, _)| account_id.clone())
            .collect()
    }

    fn suspended_count(&self) -> usize {
        self.states
            .values()
            .filter(|state| **state == AccountViewState::Suspended)
            .count()
    }
}

/// When each account was hidden. Re-hiding an already hidden account keeps the
/// original timestamp so the lock screen, a tray toggle, or a repeated hide-all
/// does not restart the idle period.
#[derive(Default)]
struct HiddenSince {
    since: HashMap<String, Instant>,
}

impl HiddenSince {
    fn mark_hidden(&mut self, account_id: &str, now: Instant) {
        self.since.entry(account_id.to_string()).or_insert(now);
    }

    fn mark_visible(&mut self, account_id: &str) {
        self.since.remove(account_id);
    }

    fn forget(&mut self, account_id: &str) {
        self.since.remove(account_id);
    }

    fn hidden_for(&self, account_id: &str, now: Instant) -> Option<Duration> {
        self.since
            .get(account_id)
            .map(|since| now.saturating_duration_since(*since))
    }
}

fn memory_sweep_due(last_sweep: Instant, now: Instant, interval: Duration) -> bool {
    now.saturating_duration_since(last_sweep) >= interval
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuspendBlocker {
    Quitting,
    Locked,
    Disabled,
    AlreadySuspended,
    Resuming,
    NoView,
    Visible,
    ActiveAccount,
    DownloadActive,
    CaptureActive,
    NotIdle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuspendDecision {
    Suspend,
    Keep(SuspendBlocker),
}

#[derive(Clone, Copy)]
struct SuspendContext<'a> {
    memory: &'a MemorySettings,
    state: AccountViewState,
    hidden: bool,
    active_account: bool,
    idle: Duration,
    downloads_active: bool,
    capture_active: bool,
    locked: bool,
    quitting: bool,
    view_open: bool,
    /// A manual "Release hidden accounts now" bypasses the automatic switch and
    /// the delay, but never the safety guards.
    manual: bool,
}

/// The low-memory policy. It is pure so every guard can be tested without a
/// GTK display, a WebKit process, or a running GPUI application.
fn suspend_decision(context: &SuspendContext) -> SuspendDecision {
    if context.quitting {
        return SuspendDecision::Keep(SuspendBlocker::Quitting);
    }
    if context.locked {
        return SuspendDecision::Keep(SuspendBlocker::Locked);
    }
    let required = context.memory.suspend_after();
    if !context.manual && required.is_none() {
        return SuspendDecision::Keep(SuspendBlocker::Disabled);
    }
    match context.state {
        AccountViewState::Suspended => {
            return SuspendDecision::Keep(SuspendBlocker::AlreadySuspended);
        }
        AccountViewState::Resuming => {
            return SuspendDecision::Keep(SuspendBlocker::Resuming);
        }
        AccountViewState::Loaded => {}
    }
    if !context.view_open {
        return SuspendDecision::Keep(SuspendBlocker::NoView);
    }
    // A visible account is the one the user is looking at, including the last
    // visible/active account. It is never released.
    if !context.hidden {
        return SuspendDecision::Keep(SuspendBlocker::Visible);
    }
    if context.active_account && context.memory.keep_active_account {
        return SuspendDecision::Keep(SuspendBlocker::ActiveAccount);
    }
    if context.downloads_active {
        return SuspendDecision::Keep(SuspendBlocker::DownloadActive);
    }
    if context.capture_active {
        return SuspendDecision::Keep(SuspendBlocker::CaptureActive);
    }
    if !context.manual && required.is_some_and(|required| context.idle < required) {
        return SuspendDecision::Keep(SuspendBlocker::NotIdle);
    }
    SuspendDecision::Suspend
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
            ConnectionState::Connecting => t!("Connecting…"),
            ConnectionState::Connected => t!("Connected"),
            ConnectionState::Offline => t!("Offline"),
            ConnectionState::Reconnecting => t!("Reconnecting…"),
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
    composition_xids: HashMap<String, u32>,
    crash_recovery: HashMap<String, CrashRecovery>,
    connection: HashMap<String, ConnectionTracker>,
    active_downloads: HashMap<String, u32>,
    scales: HashMap<String, f32>,
    settings_window: Option<WindowHandle<SettingsWindow>>,
    onboarding_window: Option<WindowHandle<OnboardingWindow>>,
    lock_window: Option<WindowHandle<LockWindow>>,
    downloads_window: Option<WindowHandle<DownloadsWindow>>,
    command_palette: Option<WindowHandle<CommandPalette>>,
    downloads: DownloadManager,
    downloads_writer: DownloadWriter,
    pending_downloads: bool,
    pending_command_palette: bool,
    /// Webview to focus once the palette window is gone. The X11 destroy and
    /// WM focus changes are asynchronous, so the next poll tick applies it.
    pending_focus: Option<String>,
    smoke: Option<SmokeState>,
    startup_html: Option<String>,
    startup_url: Option<String>,
    initial_action: Option<InstanceMessage>,
    pending_urls: HashMap<String, String>,
    /// Files forwarded by `--share` or the instance socket before the account
    /// page finished loading. They are drained on `LoadState::Finished`.
    pending_shares: HashMap<String, Vec<PathBuf>>,
    /// Accounts whose page finished loading at least once. A share sent before
    /// this point waits instead of racing the WhatsApp Web attachment inputs.
    page_ready: HashSet<String>,
    start_hidden: bool,
    first_run: bool,
    pending_accounts: HashSet<String>,
    pending_settings: bool,
    pending_onboarding: bool,
    pending_lock: bool,
    hidden_accounts: HashSet<String>,
    views: ViewLifecycle,
    hidden_since: HiddenSince,
    capture_active: HashSet<String>,
    settings_writer: SettingsWriter,
    save_debounce: SaveDebounce,
    pub update_state: UpdateState,
    pub storage: StorageState,
    update_tx: Sender<UpdateState>,
    update_rx: Receiver<UpdateState>,
    update_check_running: bool,
    last_idle_check: Instant,
    last_scale_check: Instant,
    last_dnd_check: Instant,
    last_dnd_active: bool,
    last_dnd_status: String,
    /// Last kernel power-source read. Battery changes resolve on the next poll.
    last_power_check: Instant,
    power: PowerState,
    /// Performance policy last pushed to the GTK/WebKit thread.
    performance: PerformanceStatus,
    last_memory_sweep: Instant,
    last_notifications: Vec<(String, Instant)>,
    lock_events: Receiver<SystemLockEvent>,
    errors: ErrorLog,
    boss_hidden: Option<HashSet<String>>,
    boss_settings_hidden: bool,
    boss_onboarding_hidden: bool,
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
        first_run: bool,
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
        let last_dnd_active = settings.do_not_disturb.is_active(SystemTime::now());
        crate::i18n::set_language(settings.language);
        let power = detect_power_state();
        let performance = settings.performance.resolve(power);
        gtk.set_performance(performance.battery_saver, performance.cadence());
        gtk.set_accessibility(settings.accessibility.reduce_motion);
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
            composition_xids: HashMap::new(),
            crash_recovery: HashMap::new(),
            connection: HashMap::new(),
            active_downloads: HashMap::new(),
            scales: HashMap::new(),
            settings_window: None,
            onboarding_window: None,
            lock_window: None,
            downloads_window: None,
            command_palette: None,
            downloads,
            downloads_writer: DownloadWriter::start(),
            pending_downloads: false,
            pending_command_palette: false,
            pending_focus: None,
            smoke,
            startup_html,
            startup_url,
            initial_action,
            pending_urls: HashMap::new(),
            pending_shares: HashMap::new(),
            page_ready: HashSet::new(),
            start_hidden,
            first_run,
            pending_accounts: HashSet::new(),
            pending_settings: false,
            pending_onboarding: false,
            pending_lock: false,
            hidden_accounts: HashSet::new(),
            views: ViewLifecycle::default(),
            hidden_since: HiddenSince::default(),
            capture_active: HashSet::new(),
            settings_writer: SettingsWriter::start(),
            save_debounce: SaveDebounce::default(),
            update_state,
            storage: StorageState::default(),
            update_tx,
            update_rx,
            update_check_running: false,
            last_idle_check: Instant::now(),
            last_scale_check: Instant::now(),
            last_dnd_check: Instant::now(),
            last_dnd_active,
            last_dnd_status: String::new(),
            last_power_check: Instant::now(),
            power,
            performance,
            last_memory_sweep: Instant::now(),
            last_notifications: Vec::new(),
            lock_events: lock_event_rx,
            errors: ErrorLog::default(),
            boss_hidden: None,
            boss_settings_hidden: false,
            boss_onboarding_hidden: false,
        }
    }

    pub fn start_pump(&mut self, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            loop {
                let interval = weak
                    .update(cx, |model, _| model.poll_interval())
                    .unwrap_or(PerformanceCadence::NORMAL.pump);
                smol::Timer::after(interval).await;
                if weak.update(cx, |model, cx| model.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Performance status last applied from the persisted mode and power
    /// source. The settings window uses it for the status and reason.
    pub fn performance_status(&self) -> PerformanceStatus {
        self.performance
    }

    fn poll_interval(&self) -> Duration {
        self.performance.cadence().pump
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(smoke) = &self.smoke {
            let account_id = smoke.account_id.clone();
            self.open_account_window(&account_id, cx);
            return;
        }

        // A timer may have expired while the app was closed or the machine was
        // suspended. Re-evaluate before opening windows so the tray never shows
        // a stale DND indicator.
        if self.settings.do_not_disturb.expire(SystemTime::now()) {
            self.save_settings(cx);
        }
        self.update_tray();

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
        if let Some(InstanceMessage::Share { account, path }) = &initial_action {
            let account_id = match account {
                Some(query) => resolve_account(&self.settings, query).ok(),
                None => self.settings.active_account_id(),
            };
            if let Some(account_id) = account_id {
                self.settings.active_account = Some(account_id.clone());
                self.pending_shares
                    .entry(account_id)
                    .or_default()
                    .push(path.clone());
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
        // The guide is a separate, non-modal window. The account window opens
        // first so its QR/login page is already loading behind it, and closing
        // the guide never leaves the user without a window. Autostart launches
        // and locked sessions skip it: both are explicitly "start quietly".
        if !self.start_hidden
            && !self.locked
            && should_show_onboarding(&self.settings, self.first_run)
        {
            self.open_onboarding_window(cx);
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
        Theme::resolve(
            self.settings.theme,
            window.appearance(),
            &self.settings.accessibility,
        )
    }

    /// Theme for one account window. The account override wins; otherwise the
    /// global theme applies. Other windows keep using [`Self::theme`].
    pub fn set_composition_xid(&mut self, account_id: &str, xid: u32) {
        self.composition_xids.insert(account_id.to_string(), xid);
    }

    pub fn set_embedded_bounds(
        &mut self,
        account_id: &str,
        width: i32,
        height: i32,
        _cx: &mut Context<Self>,
    ) {
        self.gtk.set_embedded_bounds(account_id, width, height);
    }

    pub fn account_theme(&self, account_id: &str, window: &Window) -> Theme {
        Theme::resolve(
            self.settings.effective_theme(account_id),
            window.appearance(),
            &self.settings.accessibility,
        )
    }

    pub fn account_theme_mode(&self, account_id: &str) -> crate::settings::ThemeMode {
        self.settings.effective_theme(account_id)
    }

    pub fn account_zoom(&self, account_id: &str) -> f64 {
        self.settings.effective_zoom(account_id)
    }

    /// Changes one account's theme override. Only that account window is
    /// repainted; the other accounts keep their own resolved appearance.
    pub fn set_account_theme(
        &mut self,
        account_id: &str,
        theme: AccountTheme,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.set_account_theme(account_id, theme) {
            return;
        }
        self.save_settings(cx);
        self.refresh_account_window(account_id, cx);
    }

    /// Changes one account's WhatsApp zoom override and pushes the resolved
    /// zoom to that account's WebKit view only.
    pub fn set_account_zoom(
        &mut self,
        account_id: &str,
        zoom: Option<f64>,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.set_account_zoom(account_id, zoom) {
            return;
        }
        self.gtk
            .set_zoom(account_id, self.settings.effective_zoom(account_id));
        self.save_settings(cx);
        self.refresh_account_window(account_id, cx);
    }

    /// Explicit bulk action: copies one account's appearance overrides to every
    /// other account. Sessions, unread counts, and window bounds are untouched.
    pub fn apply_account_appearance_to_all(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if !self.settings.apply_account_appearance_to_all(account_id) {
            return;
        }
        let account_ids: Vec<String> = self
            .settings
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        for id in account_ids {
            self.gtk.set_zoom(&id, self.settings.effective_zoom(&id));
        }
        self.save_settings(cx);
        cx.refresh_windows();
    }

    /// Snapshot of the runtime state that appears in the diagnostics report.
    /// Account ids and names are reduced to counts before they leave the
    /// model, so the report cannot include them.
    pub fn diagnostics_context(&self) -> DiagnosticsContext {
        let mut runtime = RuntimeSummary {
            locked: self.locked,
            active_downloads: self.active_downloads.values().copied().sum::<u32>() as usize,
            ..RuntimeSummary::default()
        };
        runtime.loaded_views = self
            .account_windows
            .keys()
            .filter(|account_id| self.views.state(account_id.as_str()).is_loaded())
            .count();
        for state in self.views.states.values() {
            match state {
                AccountViewState::Loaded => {}
                AccountViewState::Suspended => runtime.suspended_views += 1,
                AccountViewState::Resuming => runtime.resuming_views += 1,
            }
        }
        for connection in self.connection.values() {
            match connection.state() {
                ConnectionState::Connecting => runtime.connecting += 1,
                ConnectionState::Connected => runtime.connected += 1,
                ConnectionState::Offline => runtime.offline += 1,
                ConnectionState::Reconnecting => runtime.reconnecting += 1,
            }
        }
        for recovery in self.crash_recovery.values() {
            runtime.crash_tracking += 1;
            runtime.crash_attempts = runtime.crash_attempts.saturating_add(recovery.attempts);
            if recovery.failed {
                runtime.crash_failed += 1;
            }
        }
        DiagnosticsContext {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            settings: self.settings.clone(),
            paths: self.paths.clone(),
            home_dir: diagnostics::current_home(),
            runtime,
            errors: self.errors.to_vec(),
        }
    }

    pub fn account_name(&self, id: &str) -> String {
        self.settings
            .account(id)
            .map(|account| account.name.clone())
            .unwrap_or_else(|| t!("Account").to_string())
    }

    pub fn account_unread(&self, id: &str) -> u32 {
        self.settings
            .account(id)
            .map(|account| account.unread)
            .unwrap_or(0)
    }

    /// The avatar label and accent shared by the titlebar, Settings, the tray,
    /// and notifications.
    pub fn account_identity(&self, id: &str) -> AccountIdentity {
        self.settings
            .account(id)
            .map(Account::identity)
            .unwrap_or_default()
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
                HotkeyAction::CommandPalette => self.open_palette_from_hotkey(cx),
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

        if self.last_power_check.elapsed() >= POWER_POLL_INTERVAL {
            self.last_power_check = Instant::now();
            let power = detect_power_state();
            if power != self.power {
                debug!(
                    previous = self.power.label(),
                    current = power.label(),
                    "power source changed"
                );
                self.power = power;
                self.apply_performance(cx);
            }
        }

        let cadence = self.performance_status().cadence();
        if memory_sweep_due(self.last_memory_sweep, Instant::now(), cadence.memory_sweep) {
            self.last_memory_sweep = Instant::now();
            self.suspend_idle_accounts(false, cx);
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

        if self.last_scale_check.elapsed() >= cadence.scale_check {
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

        if let Some(account_id) = self.pending_focus.take() {
            self.focus_account_content(&account_id);
        }

        if self.smoke.is_some() {
            self.poll_smoke(cx);
        }

        if self.last_dnd_check.elapsed() >= cadence.dnd_check {
            self.last_dnd_check = Instant::now();
            self.refresh_do_not_disturb(cx);
        }
    }

    /// Re-evaluates the DND schedule on every pump cadence. The pump also runs
    /// after resume from suspend, so a long wall-clock jump cannot leave the
    /// mode stuck or the tray stale.
    fn refresh_do_not_disturb(&mut self, cx: &mut Context<Self>) {
        let now = SystemTime::now();
        let expired = self.settings.do_not_disturb.expire(now);
        if expired {
            self.save_settings(cx);
        }
        let active = self.settings.do_not_disturb.is_active(now);
        let status = self.settings.do_not_disturb.status_text(now);
        let active_changed = expired || active != self.last_dnd_active;
        if active_changed || status != self.last_dnd_status {
            debug!(
                active,
                next_transition = ?self.settings.do_not_disturb.next_transition(now),
                "Do not Disturb state changed"
            );
            self.update_tray();
            if let Some(handle) = self.settings_window {
                let _ = handle.update(cx, |_, _, cx| cx.notify());
            }
            if active_changed {
                cx.refresh_windows();
            }
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
            if !self.views.state(&account_id).is_loaded() {
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

    /// Whether a download is still streaming for this account. The persisted
    /// download history is authoritative; the live counter catches a start
    /// event that arrived before the record was written.
    fn account_downloads_active(&self, account_id: &str) -> bool {
        self.downloads_active(account_id)
            || self
                .downloads
                .records()
                .iter()
                .any(|record| record.account_id == account_id && record.is_active())
    }

    pub fn account_view_state(&self, account_id: &str) -> AccountViewState {
        self.views.state(account_id)
    }

    pub fn suspended_account_count(&self) -> usize {
        self.views.suspended_count()
    }

    /// Accounts whose WebKit view is currently loaded and has a GPUI shell.
    pub fn active_webview_count(&self) -> usize {
        self.account_windows
            .keys()
            .filter(|account_id| self.views.state(account_id.as_str()).is_loaded())
            .count()
    }

    /// Applies the automatic policy. Returns the number of accounts that were
    /// suspended, which the manual action reports back to the settings window.
    fn suspend_idle_accounts(&mut self, manual: bool, cx: &mut Context<Self>) -> usize {
        if self.quitting {
            return 0;
        }
        let now = Instant::now();
        let active_account = self.settings.active_account_id();
        let account_ids: Vec<String> = self.account_windows.keys().cloned().collect();
        // Battery saver only shortens an already enabled low-memory delay; the
        // master switch and every guard in `suspend_decision` stay in charge.
        let memory = effective_memory(&self.settings.memory, self.performance.battery_saver);
        let mut suspended = 0;
        for account_id in account_ids {
            let context = SuspendContext {
                memory: &memory,
                state: self.views.state(&account_id),
                hidden: self.hidden_accounts.contains(&account_id),
                active_account: active_account.as_deref() == Some(account_id.as_str()),
                idle: self
                    .hidden_since
                    .hidden_for(&account_id, now)
                    .unwrap_or_default(),
                downloads_active: self.account_downloads_active(&account_id),
                capture_active: self.capture_active.contains(&account_id),
                locked: self.locked,
                quitting: self.quitting,
                view_open: true,
                manual,
            };
            if suspend_decision(&context) != SuspendDecision::Suspend {
                continue;
            }
            if self.suspend_account(&account_id, cx) {
                suspended += 1;
            }
        }
        suspended
    }

    /// Releases a hidden account's WebKit view. The GPUI shell stays alive so
    /// the placeholder can offer Reactivate; the profile and session on disk
    /// are untouched.
    fn suspend_account(&mut self, account_id: &str, cx: &mut Context<Self>) -> bool {
        if !self.views.begin_suspend(account_id) {
            return false;
        }
        self.connection.remove(account_id);
        self.crash_recovery.remove(account_id);
        self.active_downloads.remove(account_id);
        self.capture_active.remove(account_id);
        self.page_ready.remove(account_id);
        self.pending_shares.remove(account_id);
        self.gtk.suspend(account_id);
        info!(account_id = %account_id, "releasing the WebKit view of a hidden account");
        self.refresh_account_window(account_id, cx);
        self.update_tray();
        cx.refresh_windows();
        true
    }

    /// Manual "Release hidden accounts now". It ignores the delay and the
    /// master switch, but keeps every safety guard: the visible and active
    /// accounts, active downloads, calls/recordings, the app lock, and quit.
    pub fn release_hidden_accounts_now(&mut self, cx: &mut Context<Self>) -> usize {
        let released = self.suspend_idle_accounts(true, cx);
        self.last_memory_sweep = Instant::now();
        self.update_tray();
        released
    }

    pub fn resume_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if !self.account_windows.contains_key(account_id) {
            self.open_account_window(account_id, cx);
            return;
        }
        self.show_account_window(account_id, cx);
    }

    pub fn resume_all_suspended(&mut self, cx: &mut Context<Self>) {
        for account_id in self.views.suspended_ids() {
            self.resume_account(&account_id, cx);
        }
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
            // A suspended or restoring account has no live page to time out or
            // reconnect. Its tracker was cleared on suspend and is recreated by
            // the load events of the new view.
            if !self.views.state(&account_id).is_loaded() {
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
                        t!("WhatsApp Web is offline"),
                        &tr!(
                            "{name} lost its connection. OxideZap will reconnect when the network returns.",
                            name = name
                        ),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::Reconnected => {
                if notify_enabled {
                    notify::notify(
                        t!("Connection restored"),
                        &tr!("{name} is connected again.", name = name),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::LoginPageBlocked => {
                if notify_enabled {
                    notify::notify(
                        t!("Connection restored"),
                        &tr!(
                            "{name} is on the WhatsApp login page. OxideZap did not reload it automatically; use Retry now if the page is stuck.",
                            name = name
                        ),
                        Some(&icon),
                    );
                }
            }
            ConnectionDecision::Exhausted => {
                self.errors.record(
                    DiagnosticLevel::Warning,
                    format!(
                        "WhatsApp Web could not reconnect after {MAX_RECONNECT_ATTEMPTS} attempts"
                    ),
                );
                if !notify::do_not_disturb_suppressed(&self.settings, SystemTime::now()) {
                    notify::notify(
                        t!("Could not reconnect WhatsApp Web"),
                        &tr!(
                            "OxideZap stopped automatic reconnects for {name} after {attempts} attempts. Use Retry now in the account window.",
                            name = name,
                            attempts = MAX_RECONNECT_ATTEMPTS
                        ),
                        Some(&icon),
                    );
                }
            }
        }
    }

    pub fn retry_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        if self.account_downloads_active(account_id) {
            warn!(
                account_id = %account_id,
                "manual WhatsApp Web reload skipped while a download is active"
            );
            if notify::notifications_allowed(&self.settings, SystemTime::now()) {
                let content = self.notification_policy().system(
                    self.locked,
                    t!("Download in progress"),
                    t!(
                        "OxideZap did not reload WhatsApp Web while a download is active. Retry when the download finishes."
                    ),
                );
                notify::notify_content(content, Some(&self.paths.runtime_icon()));
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

    pub fn account_always_on_top(&self, account_id: &str) -> bool {
        self.settings
            .account(account_id)
            .is_some_and(|account| account.always_on_top)
    }

    pub fn account_mini_mode(&self, account_id: &str) -> bool {
        self.settings
            .account(account_id)
            .is_some_and(|account| account.mini_mode)
    }

    /// Applies the persisted window modes after the GPUI window is tagged. Until
    /// then there is no X11 id to configure, so the modes wait for this call.
    fn apply_window_modes(&self, account_id: &str, xid: u32) {
        let Some(account) = self.settings.account(account_id) else {
            return;
        };
        if let Err(error) = set_always_on_top_state(&self.x11, xid, account.always_on_top) {
            warn!(%error, "could not update the always-on-top state");
        }
        self.gtk
            .set_always_on_top(account_id, account.always_on_top);
        let (min_width, min_height) = if account.mini_mode {
            (MINI_MIN_WIDTH, MINI_MIN_HEIGHT)
        } else {
            (NORMAL_MIN_WIDTH, NORMAL_MIN_HEIGHT)
        };
        if let Err(error) = set_window_min_size(&self.x11, xid, min_width, min_height) {
            warn!(%error, "could not update the window minimum size");
        }
    }

    pub fn set_always_on_top(&mut self, account_id: &str, enabled: bool, cx: &mut Context<Self>) {
        let Some(account) = self.settings.account_mut(account_id) else {
            return;
        };
        if account.always_on_top == enabled {
            return;
        }
        account.always_on_top = enabled;
        if let Some(xid) = self.account_xids.get(account_id).copied()
            && let Err(error) = set_always_on_top_state(&self.x11, xid, enabled)
        {
            warn!(%error, "could not update the always-on-top state");
        }
        self.gtk.set_always_on_top(account_id, enabled);
        self.save_settings(cx);
        self.update_tray();
        self.refresh_account_window(account_id, cx);
        cx.refresh_windows();
    }

    /// Switches between the normal window and a compact single-pane window.
    /// The normal bounds are saved first so disabling mini mode restores the
    /// previous size and position.
    pub fn set_mini_mode(&mut self, account_id: &str, enabled: bool, cx: &mut Context<Self>) {
        let Some(account) = self.settings.account(account_id).cloned() else {
            return;
        };
        if account.mini_mode == enabled {
            return;
        }
        let monitors = monitor_areas(cx);
        let handle = self.account_windows.get(account_id).copied();
        let current = handle.and_then(|handle| {
            handle
                .update(cx, |_, window, _| saved_bounds(window.bounds()))
                .ok()
        });

        if enabled {
            let normal = current
                .clone()
                .filter(SavedBounds::is_valid)
                .or(account.bounds.clone());
            let mini = mini_bounds(current.as_ref().or(normal.as_ref()), &monitors);
            if let Some(account) = self.settings.account_mut(account_id) {
                account.mini_mode = true;
                account.maximized = false;
                account.mini_bounds = Some(mini.clone());
                if let Some(normal) = &normal {
                    account.bounds = Some(normal.clone());
                    account.normal_bounds = Some(normal.clone());
                    account.remember_bounds_for_monitor(&monitors, normal);
                }
            }
            if let Some(xid) = self.account_xids.get(account_id).copied() {
                let _ = leave_maximized_state(&self.x11, xid);
                if let Err(error) =
                    set_window_min_size(&self.x11, xid, MINI_MIN_WIDTH, MINI_MIN_HEIGHT)
                {
                    warn!(%error, "could not shrink the window minimum size");
                }
                self.configure_window(account_id, &mini);
            }
            if let Some(handle) = handle {
                let _ = handle.update(cx, |_, window, _| {
                    window.resize(size(px(mini.width), px(mini.height)));
                });
            }
        } else {
            let fallback = default_saved_bounds(&monitors);
            let restore = restore_account_bounds(&account, &monitors, &fallback);
            if let Some(account) = self.settings.account_mut(account_id) {
                account.mini_mode = false;
            }
            if let Some(xid) = self.account_xids.get(account_id).copied() {
                if let Err(error) =
                    set_window_min_size(&self.x11, xid, NORMAL_MIN_WIDTH, NORMAL_MIN_HEIGHT)
                {
                    warn!(%error, "could not restore the window minimum size");
                }
                self.configure_window(account_id, &restore);
            }
            if let Some(handle) = handle {
                let _ = handle.update(cx, |_, window, _| {
                    window.resize(size(px(restore.width), px(restore.height)));
                });
            }
        }

        self.save_settings(cx);
        self.update_tray();
        self.refresh_account_window(account_id, cx);
        cx.refresh_windows();
    }

    fn configure_window(&self, account_id: &str, bounds: &SavedBounds) {
        let Some(xid) = self.account_xids.get(account_id).copied() else {
            return;
        };
        // Saved and GPUI bounds are logical pixels; X11 configure requests use
        // physical pixels.
        let scale = self.scales.get(account_id).copied().unwrap_or(1.0).max(1.0);
        if let Err(error) = self.x11.configure(
            xid,
            (bounds.x * scale).round() as i32,
            (bounds.y * scale).round() as i32,
            (bounds.width * scale).round().max(1.0) as u32,
            (bounds.height * scale).round().max(1.0) as u32,
        ) {
            warn!(%error, "could not configure the account window");
        }
    }

    /// Persists the live window geometry. Mini windows keep their own size so
    /// toggling mini mode off restores the normal bounds instead of a tiny
    /// window.
    fn remember_window_bounds(
        &mut self,
        account_id: &str,
        bounds: Bounds<Pixels>,
        maximized: bool,
        monitors: &[MonitorArea],
    ) {
        let saved = saved_bounds(bounds);
        let Some(account) = self.settings.account_mut(account_id) else {
            return;
        };
        if account.mini_mode {
            account.mini_bounds = Some(saved);
        } else {
            account.bounds = Some(saved.clone());
            account.normal_bounds = Some(saved.clone());
            account.maximized = maximized;
            account.remember_bounds_for_monitor(monitors, &saved);
        }
    }

    fn notification_policy(&self) -> notify::NotificationPrivacyPolicy {
        notify::NotificationPrivacyPolicy::new(
            self.settings.notification_privacy,
            self.settings.hide_notification_previews_when_locked,
        )
    }

    fn handle_gtk_event(&mut self, event: GtkEvent, cx: &mut Context<Self>) {
        match event {
            GtkEvent::Ready { account_id, xid } => {
                self.views.mark_loaded(&account_id);
                if let Some(smoke) = &mut self.smoke {
                    if smoke.account_id == account_id {
                        smoke.gtk_xid = Some(xid);
                    }
                }
                self.refresh_account_window(&account_id, cx);
            }
            GtkEvent::LoadState { account_id, state } => match state {
                LoadState::Started => {
                    self.page_ready.remove(&account_id);
                    self.connection
                        .entry(account_id.clone())
                        .or_default()
                        .on_load_started(Instant::now());
                    self.refresh_account_window(&account_id, cx);
                }
                LoadState::Finished => {
                    self.views.mark_loaded(&account_id);
                    self.page_ready.insert(account_id.clone());
                    let recovered = self
                        .crash_recovery
                        .remove(&account_id)
                        .is_some_and(|recovery| recovery.failed);
                    self.gtk
                        .set_zoom(&account_id, self.settings.effective_zoom(&account_id));
                    if recovered {
                        self.gtk.show(&account_id);
                    }
                    let decision = self
                        .connection
                        .entry(account_id.clone())
                        .or_default()
                        .on_load_finished(Instant::now());
                    self.announce_connection(&account_id, decision);
                    if let Some(paths) = self.pending_shares.remove(&account_id) {
                        self.gtk.paste_paths(&account_id, paths);
                    }
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
                    self.errors.record(
                        DiagnosticLevel::Error,
                        format!("WhatsApp Web load failed: {message}"),
                    );
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
                self.errors.record(
                    DiagnosticLevel::Warning,
                    "A WhatsApp Web reload was blocked by an active download",
                );
                if let Some(tracker) = self.connection.get_mut(&account_id) {
                    tracker.on_reload_blocked();
                }
                if notify::notifications_allowed(&self.settings, SystemTime::now()) {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Download in progress"),
                        t!("OxideZap did not reload WhatsApp Web while a download is active."),
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
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
                    && self.should_notify(&title, &body)
                {
                    let sound = self.settings.notification_sound_enabled(&account_id);
                    let avatar = self.account_identity(&account_id).label;
                    let mut content =
                        self.notification_policy()
                            .message(self.locked, &title, &body);
                    content.summary = notify::account_summary(&avatar, &content.summary);
                    notify::notify_content_with_sound(
                        content,
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
                *self.active_downloads.entry(account_id.clone()).or_default() += 1;
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
                if notify::notifications_allowed(&self.settings, SystemTime::now()) {
                    let folder = path
                        .parent()
                        .map(|parent| parent.display().to_string())
                        .unwrap_or_else(|| self.downloads_base_dir().display().to_string());
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Downloading"),
                        &tr!(
                            "{filename}\nSaving to {folder}",
                            filename = filename,
                            folder = folder
                        ),
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
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
                    if let Some(count) = self.active_downloads.get_mut(&record.account_id) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            self.active_downloads.remove(&record.account_id);
                        }
                    }
                    debug!(
                        %id,
                        filename = %record.filename,
                        success,
                        cancelled,
                        "download finished"
                    );
                    // A media writer may finish after the user already
                    // cancelled the row. Keep the explicit cancellation
                    // instead of flipping it back to completed.
                    let late_cancel = success && record.status == DownloadStatus::Cancelled;
                    if late_cancel {
                        debug!(%id, "ignored a late completion for a cancelled download");
                    } else if self.downloads.finish(&id, success, cancelled, now_millis()) {
                        self.save_downloads();
                        if notify::notifications_allowed(&self.settings, SystemTime::now()) {
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
                self.errors.record(
                    DiagnosticLevel::Warning,
                    format!("A download could not start: {reason}"),
                );
                if notify::notifications_allowed(&self.settings, SystemTime::now()) {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Download could not start"),
                        &tr!("{filename}\n{reason}", filename = filename, reason = reason),
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
                }
            }
            GtkEvent::DownloadRetryFailed { id, message } => {
                debug!(%id, %message, "download retry failed");
                self.errors.record(
                    DiagnosticLevel::Warning,
                    format!("A download retry failed: {message}"),
                );
                if notify::notifications_allowed(&self.settings, SystemTime::now()) {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Download could not be retried"),
                        &message,
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
                }
            }
            GtkEvent::MediaFailed {
                account_id,
                message,
            } => {
                warn!(%account_id, %message, "a media action failed");
                self.errors.record(
                    DiagnosticLevel::Warning,
                    format!("A media action failed: {message}"),
                );
                if self.settings.notifications {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Media action failed"),
                        &message,
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
                }
            }
            GtkEvent::MediaCopied { account_id } => {
                debug!(%account_id, "copied a WhatsApp image to the clipboard");
                if self.settings.notifications {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("Image copied"),
                        t!("The image is on the clipboard."),
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
                }
            }
            GtkEvent::ShareAttached { account_id, count } => {
                let name = self.account_name(&account_id);
                let noun = if count == 1 { "file" } else { "files" };
                let content = self.notification_policy().system(
                    self.locked,
                    "Attached for review",
                    &format!(
                        "{count} {noun} attached to {name}. Check the WhatsApp preview before sending."
                    ),
                );
                notify::notify_content(content, Some(&self.paths.runtime_icon()));
                debug!(%account_id, count, "queued shared files for the attachment preview");
            }
            GtkEvent::ShareRejected {
                account_id,
                message,
            } => {
                warn!(%account_id, "a shared file was rejected");
                let content = self.notification_policy().system(
                    self.locked,
                    "Could not attach the file",
                    &message,
                );
                notify::notify_content(content, Some(&self.paths.runtime_icon()));
            }
            GtkEvent::OpenExternal { url } => {
                Self::open_external(cx, &url);
            }
            GtkEvent::WhatsAppUrl { account_id, url } => {
                self.open_url_in_account(&account_id, &url, cx);
            }
            GtkEvent::Crashed { account_id } => {
                warn!(account_id = %account_id, "the WebKit web process crashed");
                self.errors
                    .record(DiagnosticLevel::Error, "The WebKit web process crashed");
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
                        self.errors.record(
                            DiagnosticLevel::Error,
                            format!(
                                "WebKit crash recovery exhausted after {MAX_CRASH_ATTEMPTS} attempts"
                            ),
                        );
                        self.gtk.hide(&account_id);
                        if !notify::do_not_disturb_suppressed(&self.settings, SystemTime::now()) {
                            notify::notify(
                                t!("WhatsApp Web crashed"),
                                &tr!(
                                    "Automatic recovery failed after {attempts} attempts. Use “Retry now” in the account window.",
                                    attempts = MAX_CRASH_ATTEMPTS
                                ),
                                Some(&self.paths.runtime_icon()),
                            );
                        }
                        self.refresh_account_window(&account_id, cx);
                    }
                    CrashDecision::Ignored => {}
                }
            }
            GtkEvent::CaptureState { account_id, active } => {
                if self.account_windows.contains_key(&account_id) {
                    if active {
                        self.capture_active.insert(account_id.clone());
                    } else {
                        self.capture_active.remove(&account_id);
                    }
                    debug!(%account_id, active, "media capture state changed");
                }
            }
            GtkEvent::Suspended { account_id } => {
                // The command path already marked the account suspended. A
                // newer resume may have been queued after the suspend command,
                // so only refresh the placeholder instead of overwriting the
                // lifecycle state here.
                if self.account_windows.contains_key(&account_id) {
                    self.refresh_account_window(&account_id, cx);
                }
            }
            GtkEvent::SuspendRefused { account_id, reason } => {
                warn!(
                    %account_id,
                    reason = reason.label(),
                    "kept a WebKit view that could not be suspended safely"
                );
                self.errors.record(
                    DiagnosticLevel::Warning,
                    format!("A WebKit view could not be suspended: {}", reason.label()),
                );
                self.views.mark_active(&account_id);
                self.refresh_account_window(&account_id, cx);
                self.update_tray();
                cx.refresh_windows();
            }
            GtkEvent::Error {
                account_id,
                message,
            } => {
                self.views.mark_failed(&account_id);
                error!(account_id = %account_id, message = %message, "WebKit error");
                self.errors
                    .record(DiagnosticLevel::Error, format!("WebKit error: {message}"));
                if !notify::do_not_disturb_suppressed(&self.settings, SystemTime::now()) {
                    let content = self.notification_policy().system(
                        self.locked,
                        t!("WhatsApp Web could not start"),
                        &message,
                    );
                    notify::notify_content(content, Some(&self.paths.runtime_icon()));
                }
                self.refresh_account_window(&account_id, cx);
                self.update_tray();
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
            InstanceMessage::Share { account, path } => {
                self.share_paths(account, path, cx).map(Some)
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
            return Err(t!("the application lock is not configured").to_string());
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
            return Err(t!("the application is locked; unlock it first").to_string());
        }
        let account_id = match account {
            Some(query) => resolve_account(&self.settings, &query)?,
            None => self
                .settings
                .active_account_id()
                .ok_or_else(|| t!("no active account; pass --account").to_string())?,
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

    /// Validates files dropped on the native titlebar or window margins. Drops
    /// over the WebKit content take the GTK path; both end in the same paste
    /// queue and keep WhatsApp's unsent attachment preview.
    pub fn attach_dropped_paths(
        &mut self,
        account_id: &str,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        match crate::webview::validate_share_paths(&paths) {
            Ok(_) => {
                self.show_account_window(account_id, cx);
                if self.page_ready.contains(account_id) {
                    self.gtk.paste_paths(account_id, paths);
                } else {
                    self.pending_shares
                        .entry(account_id.to_string())
                        .or_default()
                        .extend(paths);
                }
            }
            Err(message) => {
                let content = self.notification_policy().system(
                    self.locked,
                    "Could not attach the file",
                    &message,
                );
                notify::notify_content(content, Some(&self.paths.runtime_icon()));
            }
        }
    }

    /// Queues a validated local file for the account's WhatsApp attachment
    /// preview. Nothing is sent: WhatsApp keeps the normal composer preview,
    /// and the user has to press send.
    fn share_paths(
        &mut self,
        account: Option<String>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
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
        let files = crate::webview::validate_share_paths(std::slice::from_ref(&path))?;
        let name = self.account_name(&account_id);
        if self.settings.active_account.as_deref() != Some(account_id.as_str()) {
            self.settings.active_account = Some(account_id.clone());
            self.save_settings(cx);
        }
        self.show_account_window(&account_id, cx);
        if self.account_windows.contains_key(&account_id) && self.page_ready.contains(&account_id) {
            self.gtk.paste_paths(&account_id, vec![path]);
        } else {
            self.pending_shares
                .entry(account_id.clone())
                .or_default()
                .push(path);
            self.open_account_window(&account_id, cx);
        }
        let count = files.len();
        Ok(format!(
            "Attached {count} file{} to {name}. Check the WhatsApp preview before sending.",
            if count == 1 { "" } else { "s" }
        ))
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
            if !notify::do_not_disturb_suppressed(&self.settings, SystemTime::now()) {
                let content = self.notification_policy().system(
                    self.locked,
                    t!("Retry unavailable"),
                    t!(
                        "This download has no reusable source URL. Start it again from WhatsApp Web."
                    ),
                );
                notify::notify_content(content, Some(&self.paths.runtime_icon()));
            }
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
        let content = if success {
            if let Some(path) = record.path.as_deref() {
                self.notification_policy().system(
                    self.locked,
                    t!("Download complete"),
                    &tr!(
                        "{filename}\nSaved to {destination}",
                        filename = record.filename,
                        destination = path.display()
                    ),
                )
            } else {
                self.notification_policy().system(
                    self.locked,
                    t!("Download complete"),
                    &record.filename,
                )
            }
        } else if cancelled {
            self.notification_policy().system(
                self.locked,
                t!("Download cancelled"),
                &record.filename,
            )
        } else {
            let hint = if record.retry_url().is_some() {
                t!("Open the Download Center to retry.")
            } else {
                t!("Reopen the media and use the save action to retry.")
            };
            self.notification_policy().system(
                self.locked,
                t!("Download failed"),
                &tr!(
                    "{filename}\n{hint}",
                    filename = record.filename,
                    hint = hint
                ),
            )
        };
        notify::notify_content(content, Some(&icon));
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
        if !mode.is_enabled() {
            self.update_do_not_disturb(cx, |dnd| {
                dnd.set_off();
            });
            return;
        }
        let now = SystemTime::now();
        self.update_do_not_disturb(cx, |dnd| match mode {
            DndMode::Off => {
                dnd.set_off();
            }
            DndMode::On => {
                dnd.set_indefinite();
            }
            DndMode::OneHour => {
                dnd.set_until(now + Duration::from_secs(60 * 60));
            }
            DndMode::Tomorrow => {
                let until = dnd.until_tomorrow(now);
                dnd.set_until(until);
            }
        });
    }

    /// Applies a DND change and persists it only when something actually
    /// changed. Every settings surface (tray, CLI, and Settings window) uses
    /// this path so saving, tray refresh, and window redraws stay consistent.
    fn update_do_not_disturb(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut DoNotDisturbSettings),
    ) -> bool {
        let mut next = self.settings.do_not_disturb.clone();
        update(&mut next);
        next.normalize();
        if next == self.settings.do_not_disturb {
            return false;
        }
        self.update_settings(cx, |settings| settings.do_not_disturb = next);
        true
    }

    fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::ShowHide => self.toggle_visibility(cx),
            TrayAction::OpenAccount(id) => self.open_account_window(&id, cx),
            TrayAction::ResumeAll => self.resume_all_suspended(cx),
            TrayAction::AddAccount => self.add_account(cx),
            TrayAction::Settings => self.open_settings_window(cx),
            TrayAction::Downloads => self.open_downloads_window(cx),
            TrayAction::Lock => self.lock(cx),
            TrayAction::DoNotDisturbOff => {
                self.update_do_not_disturb(cx, |dnd| {
                    dnd.set_off();
                });
            }
            TrayAction::DoNotDisturbOneHour => {
                let until = SystemTime::now() + Duration::from_secs(60 * 60);
                self.update_do_not_disturb(cx, |dnd| {
                    dnd.set_until(until);
                });
            }
            TrayAction::DoNotDisturbTomorrow => {
                let now = SystemTime::now();
                self.update_do_not_disturb(cx, |dnd| {
                    let until = dnd.until_tomorrow(now);
                    dnd.set_until(until);
                });
            }
            TrayAction::DoNotDisturbIndefinite => {
                self.update_do_not_disturb(cx, |dnd| {
                    dnd.set_indefinite();
                });
            }
            TrayAction::DoNotDisturbSchedule => {
                self.update_do_not_disturb(cx, |dnd| {
                    dnd.set_scheduled();
                });
            }
            TrayAction::DoNotDisturbSkip => {
                let now = SystemTime::now();
                self.update_do_not_disturb(cx, |dnd| {
                    dnd.override_current_period(now);
                });
            }
            TrayAction::ToggleAlwaysOnTop(account_id) => {
                let enabled = !self.account_always_on_top(&account_id);
                self.set_always_on_top(&account_id, enabled, cx);
            }
            TrayAction::ToggleMiniMode(account_id) => {
                let enabled = !self.account_mini_mode(&account_id);
                self.set_mini_mode(&account_id, enabled, cx);
            }
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
        self.views.mark_active(&account.id);
        self.hidden_since.mark_visible(&account.id);
        let scale = handle
            .update(cx, |_, window, _| window.scale_factor())
            .unwrap_or(1.0);
        self.scales.insert(account.id.clone(), scale);

        let title = account_window_title(&account.name);
        match self.try_tag_account(&account.id, &title, &known_windows) {
            Ok(AccountTag::Tagged) => {}
            Ok(AccountTag::Closed) => {}
            Ok(AccountTag::Pending) => {
                self.schedule_account_tag(&account.id, &title, &known_windows, cx);
            }
            Err(error) => {
                warn!(account_id = %account.id, %error, "could not tag the GPUI window");
            }
        }

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
        let show = !(self.start_hidden || self.locked);
        self.send_create(&account.id, url, html, show);

        self.settings.active_account = Some(account.id.clone());
        self.save_settings(cx);
        self.update_tray();

        let _ = handle.update(cx, |_, window, _| window.activate_window());
        if self.start_hidden || self.locked {
            self.hide_account_window(&account.id, cx);
        }
    }

    /// Sends one `Create` command with the account's persisted profile and the
    /// current preferences. Every call replaces a stale WebKit entry on the GTK
    /// thread, so a resume can never leave two views for the same account.
    fn send_create(&self, account_id: &str, url: String, html: Option<String>, show: bool) {
        let scale = self.scales.get(account_id).copied().unwrap_or(1.0);
        let parent_xid = self.account_xids.get(account_id).copied();
        self.gtk.send(GtkCommand::Create {
            account_id: account_id.to_string(),
            parent_xid,
            composition_xid: self.composition_xids.get(account_id).copied(),
            data_dir: self.paths.account_data_dir(account_id),
            cache_dir: self.paths.account_cache_dir(account_id),
            url,
            html,
            show,
            scale,
            zoom: self.settings.effective_zoom(account_id),
            writing: self.settings.writing.clone(),
            proxy: self.settings.network.proxy.clone(),
            appearance: self.settings.appearance.clone(),
            media_actions: self.settings.media_actions,
            reduce_motion: self.settings.accessibility.reduce_motion,
        });
        // A recreated WebKit window loses GTK-only state such as keep-above.
        // Reapply it when the GPUI window is already tagged.
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            self.apply_window_modes(account_id, xid);
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

    /// Opens the first-run guide as its own window. It is never modal, so the
    /// settings window, the tray, and the account window stay usable while it
    /// is open. Reopening focuses the existing guide instead of stacking a
    /// second one.
    pub fn open_onboarding_window(&mut self, cx: &mut Context<Self>) {
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(handle) = self.onboarding_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.onboarding_window = None;
        }
        if self.pending_onboarding {
            return;
        }
        self.pending_onboarding = true;
        let model = cx.entity().clone();
        cx.defer(move |cx| open_onboarding_window_deferred(model, cx));
    }

    /// Ends the guide from its final "Open WhatsApp Web" button. Records
    /// completion and returns focus to the account window.
    pub fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        self.dismiss_onboarding(cx);
        if let Some(account_id) = self.settings.active_account_id() {
            self.open_account_window(&account_id, cx);
        }
    }

    /// Ends the guide from Skip or Escape without pulling the account window
    /// forward; the user may have opened the guide from Settings.
    pub fn skip_onboarding(&mut self, cx: &mut Context<Self>) {
        self.dismiss_onboarding(cx);
    }

    fn dismiss_onboarding(&mut self, cx: &mut Context<Self>) {
        self.record_onboarding_completed(cx);
        self.close_onboarding_window(cx);
    }

    /// Called from the guide's window-close hook. Closing it with the titlebar
    /// button counts as "seen"; the user can still reopen it from Settings.
    /// Programmatic removals (lock, boss key, Skip) clear the field first, so
    /// this hook only records completion for a user-initiated close.
    pub fn onboarding_closed(&mut self, cx: &mut Context<Self>) {
        self.pending_onboarding = false;
        if self.onboarding_window.take().is_none() {
            return;
        }
        self.record_onboarding_completed(cx);
    }

    fn record_onboarding_completed(&mut self, cx: &mut Context<Self>) {
        if self.settings.onboarding_completed
            && self.settings.onboarding_version == ONBOARDING_VERSION
        {
            return;
        }
        self.settings.mark_onboarding_completed();
        self.save_settings(cx);
    }

    fn close_onboarding_window(&mut self, cx: &mut Context<Self>) {
        self.pending_onboarding = false;
        if let Some(handle) = self.onboarding_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    pub fn command_palette_context(&self, account_id: &str) -> CommandContext {
        CommandContext {
            accounts: self.palette_accounts(),
            active_account: self.settings.active_account_id(),
            lock_available: lock_is_configured(&self.settings.lock),
            do_not_disturb: self.settings.do_not_disturb.is_active(SystemTime::now()),
            theme: self.settings.theme,
            zoom: self.settings.zoom,
            retry_account: self
                .settings
                .account(account_id)
                .map(|account| account.id.clone()),
            retry_available: self.settings.account(account_id).is_some()
                && self.account_connection(account_id).retry_available(),
        }
    }

    fn palette_accounts(&self) -> Vec<CommandAccount> {
        self.settings
            .accounts
            .iter()
            .map(|account| CommandAccount {
                id: account.id.clone(),
                name: account.name.clone(),
            })
            .collect()
    }

    pub fn open_command_palette(
        &mut self,
        account_id: Option<String>,
        anchor: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.quitting {
            return;
        }
        if self.locked {
            self.open_lock_window(cx);
            return;
        }
        if let Some(handle) = self.command_palette {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.command_palette = None;
        }
        if self.pending_command_palette {
            return;
        }
        let Some(account_id) = account_id.or_else(|| self.settings.active_account_id()) else {
            return;
        };
        self.pending_command_palette = true;
        let model = cx.entity().clone();
        cx.defer(move |cx| {
            open_command_palette_deferred(model, account_id, anchor, cx);
        });
    }

    fn open_palette_from_hotkey(&mut self, cx: &mut Context<Self>) {
        let account_id = self.settings.active_account_id();
        let anchor = account_id.as_ref().and_then(|account_id| {
            self.account_windows
                .get(account_id)
                .copied()
                .and_then(|handle| handle.update(cx, |_, window, _| window.bounds()).ok())
        });
        self.open_command_palette(account_id, anchor, cx);
    }

    pub fn activate_account_at(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        if self.quitting {
            return false;
        }
        let accounts = self.palette_accounts();
        let Some(account) = account_at(&accounts, index) else {
            return false;
        };
        let account_id = account.id.clone();
        self.open_account_window(&account_id, cx);
        true
    }

    pub fn execute_command(&mut self, action: CommandAction, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.pending_command_palette = false;
        match &action {
            CommandAction::ToggleVisibility => self.toggle_visibility(cx),
            CommandAction::ShowAll => self.show_all(cx),
            CommandAction::HideAll => self.hide_all(cx),
            CommandAction::OpenSettings => self.open_settings_window(cx),
            CommandAction::AddAccount => self.add_account(cx),
            CommandAction::OpenAccount { index, account_id } => {
                debug!(
                    index,
                    account_id = %account_id,
                    "switching accounts from the command palette"
                );
                self.open_account_window(account_id, cx);
            }
            CommandAction::OpenDownloadsWindow => self.open_downloads_window(cx),
            CommandAction::OpenDownloadsFolder => self.open_downloads_folder(cx),
            CommandAction::OpenStorageFolder => self.open_storage_folder(cx),
            CommandAction::Lock => self.lock(cx),
            CommandAction::SetDoNotDisturb(enabled) => {
                let mode = if *enabled { DndMode::On } else { DndMode::Off };
                self.apply_do_not_disturb(mode, cx);
            }
            CommandAction::SetTheme(theme) => self.set_theme(*theme, cx),
            CommandAction::ZoomIn => {
                let zoom = zoom_after(self.settings.zoom, 1);
                self.set_zoom(zoom, cx);
            }
            CommandAction::ZoomOut => {
                let zoom = zoom_after(self.settings.zoom, -1);
                self.set_zoom(zoom, cx);
            }
            CommandAction::ZoomReset => self.set_zoom(1.0, cx),
            CommandAction::RetryConnection { account_id } => self.retry_account(account_id, cx),
            CommandAction::Quit => self.quit(cx),
        }
        if let Some(account_id) = action.focus_account_after(self.settings.active_account_id()) {
            self.pending_focus = Some(account_id);
        }
    }

    pub fn command_palette_dismissed(&mut self, account_id: &str) {
        let was_open = self.command_palette.take().is_some();
        self.pending_command_palette = false;
        if was_open {
            self.pending_focus = Some(account_id.to_string());
        }
    }

    /// Returns keyboard focus to the WebKit overlay that sits above the account
    /// window. The overlay is a separate top-level X11 window, so activating the
    /// GPUI wrapper alone would leave typing in the native shell.
    fn focus_account_content(&self, account_id: &str) {
        if self.hidden_accounts.contains(account_id) {
            return;
        }
        let Some(parent) = self.account_xids.get(account_id).copied() else {
            return;
        };
        let Some(content) = self.find_account_content(parent) else {
            let _ = self.x11.activate(parent);
            return;
        };
        let _ = self.x11.activate(content);
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, InputFocus};
        let _ = self
            .x11
            .conn
            .set_input_focus(InputFocus::PARENT, content, x11rb::CURRENT_TIME);
        let _ = self.x11.conn.flush();
    }

    fn find_account_content(&self, parent: u32) -> Option<u32> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let tree = self.x11.conn.query_tree(self.x11.root).ok()?.reply().ok()?;
        if let Some(window) = tree
            .children
            .iter()
            .copied()
            .find_map(|window| self.content_window(window, parent))
        {
            return Some(window);
        }
        if self.account_windows.len() == 1 {
            let pid = std::process::id();
            return tree.children.iter().copied().find(|window| {
                self.x11.pid_of(*window) == Some(pid)
                    && self.x11.title_of(*window).as_deref() == Some("OxideZap content")
            });
        }
        None
    }

    /// Finds the WebKit overlay for a parent window. Reparenting window managers
    /// keep the client under a frame, so the transient hint is checked on the
    /// root child and on one level of children.
    fn content_window(&self, window: u32, parent: u32) -> Option<u32> {
        if self.x11.transient_for(window) == Some(parent)
            && self.x11.title_of(window).as_deref() == Some("OxideZap content")
        {
            return Some(window);
        }
        self.direct_children(window)
            .into_iter()
            .find(|child| self.x11.transient_for(*child) == Some(parent))
    }

    fn direct_children(&self, window: u32) -> Vec<u32> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        self.x11
            .conn
            .query_tree(window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.children)
            .unwrap_or_default()
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
        self.boss_onboarding_hidden = false;
        self.pending_settings = false;
        self.pending_onboarding = false;
        // The privacy screen shows only the lock window. Closing the settings
        // window does not discard preferences: every change is already saved.
        if let Some(handle) = self.settings_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        if let Some(handle) = self.onboarding_window.take() {
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
            if std::mem::take(&mut self.boss_onboarding_hidden) {
                self.open_onboarding_window(cx);
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
        self.boss_onboarding_hidden = if let Some(handle) = self.onboarding_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            true
        } else {
            let pending = self.pending_onboarding;
            self.pending_onboarding = false;
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
        self.boss_onboarding_hidden = false;
        let ids: Vec<String> = self.account_windows.keys().cloned().collect();
        for id in ids {
            self.hide_account_window(&id, cx);
        }
        self.update_tray();
    }

    pub fn show_all(&mut self, cx: &mut Context<Self>) {
        self.boss_hidden = None;
        self.boss_settings_hidden = false;
        self.boss_onboarding_hidden = false;
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
        self.hidden_since.mark_hidden(account_id, Instant::now());
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
        self.hidden_since.mark_visible(account_id);
        if let Some(xid) = self.account_xids.get(account_id).copied() {
            let _ = self.x11.map(xid);
            let _ = self.x11.activate(xid);
        }
        match self.views.state(account_id) {
            AccountViewState::Suspended => self.begin_resume(account_id, cx),
            AccountViewState::Resuming => {}
            AccountViewState::Loaded => {
                if !self.account_crash_failed(account_id) {
                    self.gtk.show(account_id);
                }
            }
        }
        if let Some(handle) = self.account_windows.get(account_id).copied() {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        self.update_tray();
    }

    /// Recreates the WebKit view of a suspended account. The same profile
    /// directories and therefore the same WhatsApp login are reused.
    fn begin_resume(&mut self, account_id: &str, cx: &mut Context<Self>) {
        // A storage worker is rewriting this profile. Creating a view now would
        // race the deletion, so the account stays suspended until the operation
        // finishes and can be reactivated from the placeholder.
        if self
            .storage
            .running
            .as_ref()
            .is_some_and(|running| running.account_id == account_id)
        {
            warn!(
                account_id = %account_id,
                "postponed a resume while a storage operation is running"
            );
            return;
        }
        if !self.views.begin_resume(account_id) {
            return;
        }
        let url = self
            .pending_urls
            .remove(account_id)
            .unwrap_or_else(|| WHATSAPP_URL.to_string());
        self.send_create(account_id, url, None, true);
        self.refresh_account_window(account_id, cx);
        self.update_tray();
        cx.refresh_windows();
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
        let monitors = monitor_areas(cx);
        self.remember_window_bounds(account_id, bounds, maximized, &monitors);
        self.save_settings(cx);
        self.pending_shares.remove(account_id);
        self.page_ready.remove(account_id);

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
        self.views.forget(account_id);
        self.hidden_since.forget(account_id);
        self.capture_active.remove(account_id);
        self.scales.remove(account_id);
        self.page_ready.remove(account_id);
        self.pending_shares.remove(account_id);
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
        self.views.forget(account_id);
        self.hidden_since.forget(account_id);
        self.capture_active.remove(account_id);
        self.scales.remove(account_id);
        self.page_ready.remove(account_id);
        self.pending_shares.remove(account_id);
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
            self.storage.notice = Some(StorageNotice::error(t!(
                "Another storage operation is already running."
            )));
            cx.refresh_windows();
            return;
        }
        if let Err(error) = storage::validate_operation(
            operation,
            self.settings.accounts.len(),
            self.settings.account(account_id).is_some(),
        ) {
            self.storage.notice = Some(StorageNotice::error(crate::i18n::translate_owned(
                &error.to_string(),
            )));
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
                        StorageNotice::success(tr!(
                            "Deleted the account data and removed the account ({freed} freed).",
                            freed = storage::format_bytes(freed)
                        ))
                    }
                    StorageOperation::ClearCache => {
                        self.recreate_account_webview(&account_id);
                        StorageNotice::success(tr!(
                            "Cache cleared ({freed} freed). The WhatsApp session was kept.",
                            freed = storage::format_bytes(freed)
                        ))
                    }
                    StorageOperation::ClearSession => {
                        self.recreate_account_webview(&account_id);
                        StorageNotice::success(t!(
                            "Signed out. The account was kept; open it and scan the QR code again."
                        ))
                    }
                }
            }
            Err(error) => {
                self.recreate_account_webview(&account_id);
                self.errors.record(
                    DiagnosticLevel::Error,
                    format!("A storage operation failed: {error}"),
                );
                StorageNotice::error(tr!(
                    "Could not {operation}: {error}",
                    operation = crate::i18n::translate_dynamic(operation.label()),
                    error = error
                ))
            }
        };
        self.storage.notice = Some(notice);
        self.save_settings(cx);
        self.update_tray();
        self.refresh_storage(cx);
        cx.refresh_windows();
    }

    /// Recreates the GTK overlay after its profile was cleared. A closed GPUI
    /// window is left alone: the next open creates a fresh webview anyway. A
    /// suspended account stays suspended; its next Reactivate creates the fresh
    /// view from the cleared profile.
    fn recreate_account_webview(&mut self, account_id: &str) {
        if !self.account_windows.contains_key(account_id)
            || self.settings.account(account_id).is_none()
        {
            return;
        }
        if self.views.state(account_id).is_suspended() {
            return;
        }
        self.views.forget(account_id);
        let show = !self.hidden_accounts.contains(account_id) && !self.locked;
        let url = self
            .pending_urls
            .remove(account_id)
            .unwrap_or_else(|| WHATSAPP_URL.to_string());
        self.crash_recovery.remove(account_id);
        self.connection.remove(account_id);
        self.page_ready.remove(account_id);
        self.send_create(account_id, url, None, show);
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

    /// Stores a validated experimental stylesheet. It is applied immediately
    /// when the global toggle is on; otherwise it waits for the toggle.
    pub fn set_custom_css(&mut self, css: String, cx: &mut Context<Self>) -> Result<(), String> {
        validate_custom_css(&css)?;
        if self.settings.appearance.custom_css == css {
            return Ok(());
        }
        self.settings.appearance.custom_css = css;
        self.apply_settings(cx);
        Ok(())
    }

    /// Enables or disables experimental CSS. Enabling requires a valid
    /// stylesheet so the toggle can never claim to apply an empty file.
    pub fn set_custom_css_enabled(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if enabled {
            validate_custom_css(&self.settings.appearance.custom_css)?;
        }
        if self.settings.appearance.experimental_css == enabled {
            return Ok(());
        }
        self.settings.appearance.experimental_css = enabled;
        self.apply_settings(cx);
        Ok(())
    }

    pub fn reset_custom_css(&mut self, cx: &mut Context<Self>) {
        if self.settings.appearance == AppearanceSettings::default() {
            return;
        }
        self.settings.appearance = AppearanceSettings::default();
        self.apply_settings(cx);
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
            return Err(t!("The show/hide shortcut must differ from the boss key.").to_string());
        }
        if Shortcut::parse(&self.settings.command_palette_shortcut)
            .ok()
            .as_ref()
            == Some(&shortcut)
        {
            return Err(t!(
                "The show/hide shortcut must differ from the command palette shortcut."
            )
            .to_string());
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
            return Err(t!("The boss key must differ from the show/hide shortcut.").to_string());
        }
        if Shortcut::parse(&self.settings.command_palette_shortcut)
            .ok()
            .as_ref()
            == Some(&shortcut)
        {
            return Err(
                t!("The boss key must differ from the command palette shortcut.").to_string(),
            );
        }
        self.settings.boss_key = shortcut.to_string();
        self.apply_settings(cx);
        Ok(())
    }

    pub fn set_command_palette_shortcut(
        &mut self,
        shortcut: Option<Shortcut>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(shortcut) = shortcut {
            if Shortcut::parse(&self.settings.shortcut).ok().as_ref() == Some(&shortcut) {
                return Err(t!(
                    "The command palette shortcut must differ from the show/hide shortcut."
                )
                .to_string());
            }
            if Shortcut::parse(&self.settings.boss_key).ok().as_ref() == Some(&shortcut) {
                return Err(
                    t!("The command palette shortcut must differ from the boss key.").to_string(),
                );
            }
            self.settings.command_palette_shortcut = shortcut.to_string();
        } else {
            self.settings.command_palette_shortcut.clear();
        }
        self.apply_settings(cx);
        Ok(())
    }

    pub fn set_password(&mut self, password: &str, cx: &mut Context<Self>) -> Result<(), String> {
        lock::validate_password(password).map_err(|error| {
            if error
                == format!(
                    "Use at least {} characters.",
                    crate::lock::MIN_PASSWORD_LENGTH
                )
            {
                tr!(
                    "Use at least {count} characters.",
                    count = crate::lock::MIN_PASSWORD_LENGTH
                )
            } else {
                crate::i18n::translate_owned(&error)
            }
        })?;
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
            self.views.forget(&account_id);
            self.hidden_since.forget(&account_id);
            self.capture_active.remove(&account_id);
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
        // Defaults include an uncompleted guide, so a preferences reset makes
        // the first-run tour available again and opens it for review.
        self.open_onboarding_window(cx);
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
        crate::i18n::set_language(self.settings.language);
        if let Err(error) = autostart::apply(self.settings.start_at_login, &self.paths) {
            warn!(%error, "could not update the autostart entry");
        }
        for account_id in self
            .settings
            .accounts
            .iter()
            .map(|account| account.id.clone())
        {
            self.gtk
                .set_zoom(&account_id, self.settings.effective_zoom(&account_id));
        }
        self.apply_hotkeys();
        self.gtk.set_proxy(self.settings.network.proxy.clone());
        self.gtk.set_appearance(self.settings.appearance.clone());
        self.gtk
            .set_accessibility(self.settings.accessibility.reduce_motion);
        self.gtk.set_media_actions(self.settings.media_actions);
        self.apply_performance(cx);
        // Turning low-memory mode off must not strand accounts in the
        // suspended placeholder. Re-enabling it never suspends immediately:
        // each account has to stay hidden for the configured delay first.
        if !self.settings.memory.suspend_hidden {
            self.resume_all_suspended(cx);
        }
        self.update_tray();
        cx.refresh_windows();
    }

    /// Pushes the resolved policy to the GTK/WebKit thread. Calling this on
    /// every settings change is cheap: the thread applies the stylesheet only
    /// when the saver state actually changes.
    fn apply_performance(&mut self, cx: &mut Context<Self>) {
        let status = self.settings.performance.resolve(self.power);
        self.gtk
            .set_performance(status.battery_saver, status.cadence());
        if status == self.performance {
            return;
        }
        self.performance = status;
        debug!(
            battery_saver = status.battery_saver,
            power = status.power.label(),
            reason = status.status_text(),
            "performance mode changed"
        );
        if let Some(handle) = self.settings_window {
            let _ = handle.update(cx, |_, _, cx| cx.notify());
        }
    }

    fn apply_hotkeys(&self) {
        if let Ok(shortcut) = Shortcut::parse(&self.settings.shortcut) {
            self.hotkey.set(hotkey_bindings(
                shortcut,
                &self.settings.boss_key,
                &self.settings.command_palette_shortcut,
            ));
        }
    }

    /// Coalesces a settings change and writes it after a short quiet period.
    /// No disk I/O happens on the GPUI thread.
    fn save_settings(&mut self, cx: &mut Context<Self>) {
        let Some(generation) = self.save_debounce.mark_dirty() else {
            return;
        };
        cx.spawn(async move |model, cx| {
            smol::Timer::after(SETTINGS_SAVE_DEBOUNCE).await;
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

    fn update_tray(&mut self) {
        let now = SystemTime::now();
        let dnd = TrayDndState {
            active: self.settings.do_not_disturb.is_active(now),
            status: self.settings.do_not_disturb.status_text(now),
            follows_schedule: self.settings.do_not_disturb.follows_schedule(),
            schedule_active: self.settings.do_not_disturb.schedule_is_active(now),
            wake_time: self.settings.do_not_disturb.wake_time().to_string(),
        };
        self.last_dnd_active = dnd.active;
        self.last_dnd_status = dnd.status.clone();
        let unread = self.settings.total_unread();
        let visible = self.any_account_visible();
        let window = self.settings.active_account_id().and_then(|account_id| {
            self.settings
                .account(&account_id)
                .map(|account| TrayWindowState {
                    account_id: account.id.clone(),
                    name: account.name.clone(),
                    always_on_top: account.always_on_top,
                    mini_mode: account.mini_mode,
                })
        });
        if let Some(tray) = &self.tray {
            let suspended: HashSet<String> = self.views.suspended_ids().into_iter().collect();
            tray.update(
                &self.settings.accounts,
                unread,
                visible,
                dnd,
                &suspended,
                window,
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
        self.apply_window_modes(account_id, xid);
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
                smol::Timer::after(ACCOUNT_TAG_RETRY_INTERVAL).await;
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
        let monitors = monitor_areas(cx);
        for (id, handle) in windows {
            if let Ok((bounds, maximized)) =
                handle.update(cx, |_, window, _| (window.bounds(), window.is_maximized()))
            {
                self.remember_window_bounds(&id, bounds, maximized, &monitors);
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
        let composition = self.composition_xids.contains_key(&account_id);
        let ok = if composition {
            viewable
                && i32::from(gtk_geometry.x) == 0
                && i32::from(gtk_geometry.y) == 0
                && u32::from(gtk_geometry.width) >= expected_width.saturating_sub(2)
                && u32::from(gtk_geometry.height) >= expected_height.saturating_sub(2)
        } else {
            transient == Some(gpui_xid)
                && viewable
                && i32::from(gtk_geometry.x) == expected_x
                && i32::from(gtk_geometry.y) == expected_y
                && u32::from(gtk_geometry.width) == expected_width
                && u32::from(gtk_geometry.height) == expected_height
        };

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

fn open_command_palette_deferred(
    model: Entity<AppModel>,
    account_id: String,
    anchor: Option<Bounds<Pixels>>,
    cx: &mut App,
) {
    if model.read(cx).locked {
        model.update(cx, |model, _| model.pending_command_palette = false);
        return;
    }

    let window_bounds = match anchor {
        Some(anchor) => {
            let width = f32::from(anchor.size.width);
            let x = f32::from(anchor.origin.x) + (width - COMMAND_PALETTE_WIDTH) / 2.0;
            let y = f32::from(anchor.origin.y) + COMMAND_PALETTE_TOP_OFFSET;
            WindowBounds::Windowed(Bounds::new(
                point(px(x.max(0.0)), px(y.max(0.0))),
                size(px(COMMAND_PALETTE_WIDTH), px(COMMAND_PALETTE_HEIGHT)),
            ))
        }
        None => WindowBounds::centered(
            size(px(COMMAND_PALETTE_WIDTH), px(COMMAND_PALETTE_HEIGHT)),
            cx,
        ),
    };

    let options = WindowOptions {
        window_bounds: Some(window_bounds),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        display_id: None,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("oxidezap-palette".to_string()),
        window_min_size: None,
        window_decorations: Some(WindowDecorations::Server),
        tabbing_identifier: None,
        ..Default::default()
    };

    let model_for_view = model.clone();
    let account_for_view = account_id.clone();
    let model_for_close = model.clone();
    let account_for_close = account_id.clone();
    match cx.open_window(options, move |window, cx| {
        window.set_app_id("oxidezap-palette");
        let palette =
            cx.new(|cx| CommandPalette::new(model_for_view.clone(), account_for_view.clone(), cx));
        window.on_window_should_close(cx, move |window, cx| {
            model_for_close.update(cx, |model, _| {
                model.command_palette_dismissed(&account_for_close);
            });
            window.remove_window();
            false
        });
        palette
    }) {
        Ok(handle) => {
            model.update(cx, |model, _| {
                model.pending_command_palette = false;
                model.command_palette = Some(handle);
            });
        }
        Err(error) => {
            error!(%error, "could not open the command palette");
            model.update(cx, |model, _| model.pending_command_palette = false);
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

    let monitors = monitor_areas(cx);
    let window_bounds = if account.mini_mode {
        let mini = account
            .mini_bounds
            .clone()
            .filter(SavedBounds::is_usable)
            .map(|bounds| clamp_to_available_monitors(&bounds, &monitors))
            .unwrap_or_else(|| mini_bounds(account.bounds.as_ref(), &monitors));
        window_bounds(&mini)
    } else if remember_bounds {
        let fallback = default_saved_bounds(&monitors);
        let saved = restore_account_bounds(&account, &monitors, &fallback);
        let bounds = Bounds::new(
            point(px(saved.x), px(saved.y)),
            size(px(saved.width), px(saved.height)),
        );
        if account.maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }
    } else {
        WindowBounds::centered(
            size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
            cx,
        )
    };
    let (min_width, min_height) = if account.mini_mode {
        (MINI_MIN_WIDTH, MINI_MIN_HEIGHT)
    } else {
        (NORMAL_MIN_WIDTH, NORMAL_MIN_HEIGHT)
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
        window_min_size: Some(size(px(min_width), px(min_height))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
        ..Default::default()
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
            let notify_allowed = {
                let model = model.read(cx);
                !notify::do_not_disturb_suppressed(&model.settings, SystemTime::now())
            };
            if notify_allowed {
                let content = model.read(cx).notification_policy().system(
                    model.read(cx).locked,
                    t!("Could not open the account window"),
                    &error.to_string(),
                );
                notify::notify_content(content, Some(&icon));
            }
            model.update(cx, |model, _| {
                model.pending_accounts.remove(&account_id);
            });
            return;
        }
    };

    let _ = handle.update(cx, |view, window, cx| view.ensure_composition(window, cx));
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
            title: Some(SharedString::from(t!("OxideZap Settings"))),
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
        ..Default::default()
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

fn open_onboarding_window_deferred(model: Entity<AppModel>, cx: &mut App) {
    let hidden_or_locked = {
        let model = model.read(cx);
        model.locked || model.boss_hidden.is_some()
    };
    if hidden_or_locked {
        model.update(cx, |model, _| model.pending_onboarding = false);
        return;
    }
    info!("opening first-run guide");
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(700.0), px(620.0)), cx)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from(t!("Welcome to OxideZap"))),
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
        app_id: Some("oxidezap-onboarding".to_string()),
        window_min_size: Some(size(px(620.0), px(520.0))),
        window_decorations: Some(WindowDecorations::Client),
        tabbing_identifier: None,
        ..Default::default()
    };

    let model_for_view = model.clone();
    let model_for_close = model.clone();
    match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id("oxidezap-onboarding");
        window.on_window_should_close(cx, move |window, cx| {
            model_for_close.update(cx, |model, cx| model.onboarding_closed(cx));
            window.remove_window();
            false
        });
        cx.new(|cx| OnboardingWindow::new(model_for_view.clone(), cx))
    }) {
        Ok(handle) => {
            model.update(cx, |model, _| {
                model.pending_onboarding = false;
                model.onboarding_window = Some(handle);
            });
        }
        Err(error) => {
            error!(%error, "could not open the first-run guide");
            model.update(cx, |model, _| model.pending_onboarding = false);
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
            title: Some(SharedString::from(t!("OxideZap Downloads"))),
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
        ..Default::default()
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
            title: Some(SharedString::from(t!("OxideZap Locked"))),
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
        ..Default::default()
    };

    let model_for_view = model.clone();
    match cx.open_window(options, move |window, cx| {
        window.request_decorations(WindowDecorations::Client);
        window.set_app_id("oxidezap-lock");
        let view = cx.new(|cx| LockWindow::new(model_for_view.clone(), cx));
        let focus = view.read(cx).password_focus(cx);
        window.focus(&focus, cx);
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
/// scripts can read it with `sed`, `grep`, or `cut`. `dnd` reports the effective
/// state, so an active quiet-hours window prints `on` too.
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
        if settings.do_not_disturb.is_active(SystemTime::now()) {
            "on"
        } else {
            "off"
        },
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

    fn memory_mode() -> MemorySettings {
        MemorySettings {
            suspend_hidden: true,
            suspend_after_minutes: 30,
            keep_active_account: true,
        }
    }

    fn suspend_context(memory: &MemorySettings) -> SuspendContext<'_> {
        SuspendContext {
            memory,
            state: AccountViewState::Loaded,
            hidden: true,
            active_account: false,
            idle: Duration::from_secs(30 * 60),
            downloads_active: false,
            capture_active: false,
            locked: false,
            quitting: false,
            view_open: true,
            manual: false,
        }
    }

    #[test]
    fn low_memory_policy_requires_a_hidden_idle_loaded_view() {
        let memory = memory_mode();
        assert_eq!(
            suspend_decision(&suspend_context(&memory)),
            SuspendDecision::Suspend
        );

        let mut visible = suspend_context(&memory);
        visible.hidden = false;
        assert_eq!(
            suspend_decision(&visible),
            SuspendDecision::Keep(SuspendBlocker::Visible)
        );

        let mut early = suspend_context(&memory);
        early.idle = Duration::from_secs(30 * 60 - 1);
        assert_eq!(
            suspend_decision(&early),
            SuspendDecision::Keep(SuspendBlocker::NotIdle)
        );

        let mut missing_view = suspend_context(&memory);
        missing_view.view_open = false;
        assert_eq!(
            suspend_decision(&missing_view),
            SuspendDecision::Keep(SuspendBlocker::NoView)
        );

        let mut disabled = memory_mode();
        disabled.suspend_hidden = false;
        assert_eq!(
            suspend_decision(&suspend_context(&disabled)),
            SuspendDecision::Keep(SuspendBlocker::Disabled)
        );

        let mut already = suspend_context(&memory);
        already.state = AccountViewState::Suspended;
        assert_eq!(
            suspend_decision(&already),
            SuspendDecision::Keep(SuspendBlocker::AlreadySuspended)
        );

        let mut resuming = suspend_context(&memory);
        resuming.state = AccountViewState::Resuming;
        assert_eq!(
            suspend_decision(&resuming),
            SuspendDecision::Keep(SuspendBlocker::Resuming)
        );
    }

    #[test]
    fn low_memory_policy_never_interrupts_busy_locked_or_quitting_accounts() {
        let memory = memory_mode();

        let mut downloading = suspend_context(&memory);
        downloading.downloads_active = true;
        assert_eq!(
            suspend_decision(&downloading),
            SuspendDecision::Keep(SuspendBlocker::DownloadActive)
        );

        let mut recording = suspend_context(&memory);
        recording.capture_active = true;
        assert_eq!(
            suspend_decision(&recording),
            SuspendDecision::Keep(SuspendBlocker::CaptureActive)
        );

        let mut locked = suspend_context(&memory);
        locked.locked = true;
        assert_eq!(
            suspend_decision(&locked),
            SuspendDecision::Keep(SuspendBlocker::Locked)
        );

        let mut quitting = suspend_context(&memory);
        quitting.quitting = true;
        assert_eq!(
            suspend_decision(&quitting),
            SuspendDecision::Keep(SuspendBlocker::Quitting)
        );
    }

    #[test]
    fn an_active_download_blocks_low_memory_suspension() {
        let memory = memory_mode();
        let mut context = suspend_context(&memory);
        context.downloads_active = true;
        assert_eq!(
            suspend_decision(&context),
            SuspendDecision::Keep(SuspendBlocker::DownloadActive)
        );

        context.manual = true;
        assert_eq!(
            suspend_decision(&context),
            SuspendDecision::Keep(SuspendBlocker::DownloadActive)
        );
    }

    #[test]
    fn low_memory_policy_keeps_the_active_account_only_when_configured() {
        let memory = memory_mode();
        let mut active = suspend_context(&memory);
        active.active_account = true;
        assert_eq!(
            suspend_decision(&active),
            SuspendDecision::Keep(SuspendBlocker::ActiveAccount)
        );

        let mut release_active = memory_mode();
        release_active.keep_active_account = false;
        assert_eq!(
            suspend_decision(&SuspendContext {
                memory: &release_active,
                ..active
            }),
            SuspendDecision::Suspend
        );
    }

    #[test]
    fn manual_release_ignores_the_delay_but_keeps_the_safety_guards() {
        let mut disabled = memory_mode();
        disabled.suspend_hidden = false;
        let mut manual = suspend_context(&disabled);
        manual.manual = true;
        manual.idle = Duration::ZERO;
        assert_eq!(suspend_decision(&manual), SuspendDecision::Suspend);

        let mut downloading = manual;
        downloading.downloads_active = true;
        assert_eq!(
            suspend_decision(&downloading),
            SuspendDecision::Keep(SuspendBlocker::DownloadActive)
        );

        let mut recording = manual;
        recording.capture_active = true;
        assert_eq!(
            suspend_decision(&recording),
            SuspendDecision::Keep(SuspendBlocker::CaptureActive)
        );

        let mut locked = manual;
        locked.locked = true;
        assert_eq!(
            suspend_decision(&locked),
            SuspendDecision::Keep(SuspendBlocker::Locked)
        );

        let mut active = manual;
        active.active_account = true;
        assert_eq!(
            suspend_decision(&active),
            SuspendDecision::Keep(SuspendBlocker::ActiveAccount)
        );
    }

    #[test]
    fn the_sweep_runs_at_most_once_per_interval() {
        let interval = PerformanceCadence::NORMAL.memory_sweep;
        let start = Instant::now();
        assert!(!memory_sweep_due(start, start, interval));
        assert!(!memory_sweep_due(
            start,
            start + interval - Duration::from_millis(1),
            interval
        ));
        assert!(memory_sweep_due(start, start + interval, interval));
        assert!(memory_sweep_due(
            start,
            start + interval + Duration::from_secs(1),
            interval
        ));
    }

    #[test]
    fn battery_saver_shortens_the_delay_but_keeps_visible_and_active_guards() {
        let memory = memory_mode();
        let saver_memory = effective_memory(&memory, true);
        assert_eq!(
            saver_memory.suspend_after_minutes,
            crate::settings::performance::BATTERY_SUSPEND_MINUTES
        );

        let mut hidden = suspend_context(&saver_memory);
        hidden.idle = Duration::from_secs(20 * 60);
        assert_eq!(suspend_decision(&hidden), SuspendDecision::Suspend);

        let mut visible = hidden;
        visible.hidden = false;
        assert_eq!(
            suspend_decision(&visible),
            SuspendDecision::Keep(SuspendBlocker::Visible)
        );

        let mut active = hidden;
        active.active_account = true;
        assert_eq!(
            suspend_decision(&active),
            SuspendDecision::Keep(SuspendBlocker::ActiveAccount)
        );

        let disabled = effective_memory(
            &MemorySettings {
                suspend_hidden: false,
                ..memory
            },
            true,
        );
        assert_eq!(
            suspend_decision(&suspend_context(&disabled)),
            SuspendDecision::Keep(SuspendBlocker::Disabled)
        );
    }

    #[test]
    fn repeated_hides_keep_the_original_idle_start() {
        let start = Instant::now();
        let mut hidden = HiddenSince::default();

        hidden.mark_hidden("a", start);
        hidden.mark_hidden("a", start + Duration::from_secs(50));

        assert_eq!(
            hidden.hidden_for("a", start + Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );

        hidden.mark_visible("a");
        assert_eq!(
            hidden.hidden_for("a", start + Duration::from_secs(90)),
            None
        );

        hidden.mark_hidden("a", start + Duration::from_secs(90));
        assert_eq!(
            hidden.hidden_for("a", start + Duration::from_secs(100)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn view_lifecycle_maps_suspend_and_resume_without_duplicates() {
        let mut views = ViewLifecycle::default();
        assert_eq!(views.state("a"), AccountViewState::Loaded);

        assert!(views.begin_suspend("a"));
        assert!(!views.begin_suspend("a"));
        assert_eq!(views.state("a"), AccountViewState::Suspended);
        assert_eq!(views.suspended_ids(), vec!["a".to_string()]);

        assert!(views.begin_resume("a"));
        assert!(!views.begin_resume("a"));
        assert_eq!(views.state("a"), AccountViewState::Resuming);
        views.mark_loaded("a");
        assert_eq!(views.state("a"), AccountViewState::Loaded);

        assert!(views.begin_suspend("a"));
        views.forget("a");
        assert_eq!(views.state("a"), AccountViewState::Loaded);
        assert_eq!(views.suspended_count(), 0);
    }

    #[test]
    fn stale_view_events_do_not_revive_a_suspended_account() {
        let mut views = ViewLifecycle::default();
        assert!(views.begin_suspend("a"));

        views.mark_loaded("a");
        assert_eq!(views.state("a"), AccountViewState::Suspended);

        views.mark_active("a");
        assert_eq!(views.state("a"), AccountViewState::Loaded);
    }

    #[test]
    fn a_failed_resume_returns_to_the_suspended_placeholder() {
        let mut views = ViewLifecycle::default();
        assert!(views.begin_suspend("a"));
        assert!(views.begin_resume("a"));

        views.mark_failed("a");

        assert_eq!(views.state("a"), AccountViewState::Suspended);
        assert!(views.begin_resume("a"));
    }

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
            do_not_disturb: DoNotDisturbSettings {
                mode: crate::settings::DoNotDisturbMode::On,
                ..DoNotDisturbSettings::default()
            },
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
    fn status_output_reports_off_by_default() {
        let settings = Settings::default();
        assert!(
            status_text(&settings, false, 0).contains("dnd: off"),
            "the existing status format must keep reporting an inactive DND"
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

    fn test_monitors() -> Vec<MonitorArea> {
        vec![MonitorArea {
            key: "primary".to_string(),
            bounds: SavedBounds {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
        }]
    }

    #[test]
    fn the_default_window_is_centered_on_the_primary_monitor() {
        let bounds = default_saved_bounds(&test_monitors());

        assert_eq!(
            bounds,
            SavedBounds {
                x: 370.0,
                y: 150.0,
                width: 1180.0,
                height: 780.0,
            }
        );
    }

    #[test]
    fn mini_bounds_are_compact_and_clamped_to_the_available_monitor() {
        let source = SavedBounds {
            x: 5000.0,
            y: 0.0,
            width: 1180.0,
            height: 780.0,
        };

        let mini = mini_bounds(Some(&source), &test_monitors());

        assert_eq!(
            mini,
            SavedBounds {
                x: 1500.0,
                y: 0.0,
                width: MINI_WINDOW_WIDTH,
                height: MINI_WINDOW_HEIGHT,
            }
        );
        assert_eq!(
            clamp_to_available_monitors(&source, &test_monitors()).x,
            740.0
        );
    }
}
