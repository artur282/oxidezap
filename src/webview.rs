use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use gtk::prelude::*;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webkit2gtk::{
    DownloadExt, MediaCaptureState, NavigationPolicyDecision, NavigationPolicyDecisionExt,
    NetworkError, NetworkProxyMode, NetworkProxySettings, NotificationExt,
    NotificationPermissionRequest, PermissionRequest, PermissionRequestExt, PolicyDecisionExt,
    PolicyDecisionType, PolicyError, SettingsExt, URIRequestExt, URIResponseExt,
    UserContentInjectedFrames, UserContentManager, UserContentManagerExt,
    UserMediaPermissionRequest, UserMediaPermissionRequestExt, UserScript, UserScriptInjectionTime,
    UserStyleLevel, UserStyleSheet, WebContextExt, WebProcessTerminationReason, WebViewExt,
    WebViewExtManual, WebsiteDataManagerExt,
};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};

use crate::app::downloads::{
    DOWNLOAD_PROGRESS_INTERVAL, DownloadLayout, discard_empty_reservation, sanitize_filename,
};
use crate::links::{self, LinkAction};
use crate::paths::Paths;
use crate::settings::performance::PerformanceCadence;
use crate::settings::storage::{self, StorageOperation};
use crate::settings::{
    AppearanceSettings, ProxyMode, ProxySettings, WritingSettings, validate_custom_css,
};
use crate::unread::parse_unread;
use crate::x11::{Geometry, X11};

/// The height of the native GPUI titlebar, in logical pixels. The WebKit
/// overlay is positioned below it using the window scale factor.
pub const TITLEBAR_HEIGHT: f32 = 42.0;
/// Exposed GPUI pixels around the WebKit overlay so the window edge-resize
/// handles remain reachable instead of being covered by the GTK window.
pub const OVERLAY_MARGIN: f32 = 4.0;
pub const WHATSAPP_URL: &str = "https://web.whatsapp.com/";
/// URL patterns passed to WebKit's user stylesheet allow list. WebKit applies
/// the stylesheet only when the committed document URL matches one of them,
/// so non-WhatsApp pages never receive the experimental CSS.
const CUSTOM_CSS_ALLOW_PATTERNS: &[&str] = &["https://web.whatsapp.com/*"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadState {
    Started,
    Redirected,
    Committed,
    Finished,
}

/// What the browser itself reports through `navigator.onLine`. This is only
/// the primary signal; load failures and the native load timeout in the
/// application layer are the fallback when the page cannot report at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Online,
    Offline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionReport {
    pub state: ConnectionState,
    pub login_page: bool,
}

#[derive(Clone, Debug)]
pub enum GtkEvent {
    Ready {
        account_id: String,
        xid: u32,
    },
    LoadState {
        account_id: String,
        state: LoadState,
    },
    LoadFailed {
        account_id: String,
        uri: String,
        message: String,
    },
    ConnectionState {
        account_id: String,
        report: ConnectionReport,
    },
    ReloadBlocked {
        account_id: String,
    },
    Title {
        account_id: String,
        title: String,
        unread: u32,
    },
    Notification {
        account_id: String,
        title: String,
        body: String,
    },
    DownloadStarted {
        id: String,
        account_id: String,
        filename: String,
        path: PathBuf,
        source_url: Option<String>,
        total_bytes: Option<u64>,
    },
    DownloadProgress {
        id: String,
        received: u64,
        total: Option<u64>,
    },
    DownloadFinished {
        id: String,
        success: bool,
        cancelled: bool,
    },
    DownloadRejected {
        account_id: String,
        filename: String,
        reason: String,
    },
    DownloadRetryFailed {
        id: String,
        message: String,
    },
    OpenExternal {
        url: String,
    },
    WhatsAppUrl {
        account_id: String,
        url: String,
    },
    Crashed {
        account_id: String,
    },
    /// Any of the camera, microphone, or display capture states changed. A
    /// muted stream still counts as an active call or recording.
    CaptureState {
        account_id: String,
        active: bool,
    },
    /// The GTK thread confirmed that the view was destroyed and its web process
    /// released.
    Suspended {
        account_id: String,
    },
    /// The GTK thread refused to release a view that started a download or a
    /// call between the policy check and the command.
    SuspendRefused {
        account_id: String,
        reason: SuspendRefusal,
    },
    Error {
        account_id: String,
        message: String,
    },
    /// A storage operation finished after the GTK thread closed the account's
    /// webview. `result` is the number of freed bytes or a user-facing error.
    StorageFinished {
        account_id: String,
        operation: StorageOperation,
        result: Result<u64, String>,
    },
    /// An explicit media action could not be completed. `message` is a short,
    /// user-facing explanation; the media URL and payload are never included.
    MediaFailed {
        account_id: String,
        message: String,
    },
    /// An image was placed on the clipboard after an explicit copy action.
    MediaCopied {
        account_id: String,
    },
    /// One or more shared or dropped files were queued for the attachment
    /// preview. The payload never contains paths or file names.
    ShareAttached {
        account_id: String,
        count: usize,
    },
    /// A shared or dropped file was rejected before it reached WhatsApp.
    ShareRejected {
        account_id: String,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendRefusal {
    DownloadActive,
    CaptureActive,
}

impl SuspendRefusal {
    pub fn label(self) -> &'static str {
        match self {
            SuspendRefusal::DownloadActive => "a download is active",
            SuspendRefusal::CaptureActive => "a call or recording is active",
        }
    }
}

#[derive(Clone, Debug)]
pub enum GtkCommand {
    Create {
        account_id: String,
        /// GPUI window the overlay is anchored to. `None` means discovery
        /// failed and the account tag will be used once as a fallback.
        parent_xid: Option<u32>,
        /// GPUI Fast composition surface XID. When set, the GTK window is
        /// reparented into this child surface instead of using WM transients.
        composition_xid: Option<u32>,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        url: String,
        html: Option<String>,
        show: bool,
        scale: f32,
        zoom: f64,
        writing: WritingSettings,
        proxy: ProxySettings,
        appearance: AppearanceSettings,
        /// Whether the injected media action menu is enabled for this account.
        media_actions: bool,
        /// Whether the independent reduced-motion preference is active. The
        /// GTK thread also knows the battery-saver state.
        reduce_motion: bool,
    },
    Show {
        account_id: String,
    },
    Hide {
        account_id: String,
    },
    SetEmbeddedBounds {
        account_id: String,
        width: i32,
        height: i32,
    },
    SetZoom {
        account_id: String,
        zoom: f64,
    },
    SetWriting {
        writing: WritingSettings,
    },
    SetProxy {
        proxy: ProxySettings,
    },
    SetAppearance {
        appearance: AppearanceSettings,
    },
    /// Enables or disables the injected media action menu in every open page.
    SetMediaActions {
        enabled: bool,
    },
    /// Applies the resolved performance policy. The stylesheet and the injected
    /// connection poll change immediately; no restart or reload is needed.
    SetPerformance {
        battery_saver: bool,
        cadence: PerformanceCadence,
    },
    /// Applies the native accessibility preference that affects web content.
    /// Reduce motion is independent of battery saver: either source activates
    /// the same stylesheet, but turning one off never changes the other.
    SetAccessibility {
        reduce_motion: bool,
    },
    SetDownloadLayout(DownloadLayout),
    CancelDownload {
        id: String,
    },
    RetryDownload {
        id: String,
        account_id: String,
        url: String,
    },
    SetScale {
        account_id: String,
        scale: f32,
    },
    LoadUrl {
        account_id: String,
        url: String,
    },
    Reload {
        account_id: String,
    },
    /// Closes the account's webview and then deletes or clears its profile
    /// directories on a worker thread.
    ClearStorage {
        account_id: String,
        operation: StorageOperation,
        paths: Paths,
    },
    /// Destroys the account's GTK window and WebKit view, releasing the web
    /// process. The profile directories and the WhatsApp session stay on disk.
    Suspend {
        account_id: String,
    },
    /// Validates and queues local files for the account's attachment preview.
    /// Used by the CLI, the desktop drop target, and the native titlebar drop.
    PastePaths {
        account_id: String,
        paths: Vec<PathBuf>,
    },
    /// Keeps the WebKit overlay above its parent when the account window is
    /// always-on-top.
    SetAlwaysOnTop {
        account_id: String,
        enabled: bool,
    },
    Close {
        account_id: String,
    },
    Quit,
}

#[derive(Clone)]
pub struct GtkHandle {
    tx: Sender<GtkCommand>,
}

impl GtkHandle {
    pub fn send(&self, command: GtkCommand) {
        let _ = self.tx.send(command);
    }

    pub fn show(&self, account_id: &str) {
        self.send(GtkCommand::Show {
            account_id: account_id.to_string(),
        });
    }

    pub fn hide(&self, account_id: &str) {
        self.send(GtkCommand::Hide {
            account_id: account_id.to_string(),
        });
    }

    pub fn set_embedded_bounds(&self, account_id: &str, width: i32, height: i32) {
        self.send(GtkCommand::SetEmbeddedBounds {
            account_id: account_id.to_string(),
            width,
            height,
        });
    }

    pub fn set_zoom(&self, account_id: &str, zoom: f64) {
        self.send(GtkCommand::SetZoom {
            account_id: account_id.to_string(),
            zoom,
        });
    }

    pub fn set_writing(&self, writing: WritingSettings) {
        self.send(GtkCommand::SetWriting { writing });
    }

    pub fn set_proxy(&self, proxy: ProxySettings) {
        self.send(GtkCommand::SetProxy { proxy });
    }

    pub fn set_appearance(&self, appearance: AppearanceSettings) {
        self.send(GtkCommand::SetAppearance { appearance });
    }

    pub fn set_media_actions(&self, enabled: bool) {
        self.send(GtkCommand::SetMediaActions { enabled });
    }

    pub fn set_performance(&self, battery_saver: bool, cadence: PerformanceCadence) {
        self.send(GtkCommand::SetPerformance {
            battery_saver,
            cadence,
        });
    }

    pub fn set_accessibility(&self, reduce_motion: bool) {
        self.send(GtkCommand::SetAccessibility { reduce_motion });
    }

    pub fn set_download_layout(&self, layout: DownloadLayout) {
        self.send(GtkCommand::SetDownloadLayout(layout));
    }

    pub fn cancel_download(&self, id: &str) {
        self.send(GtkCommand::CancelDownload { id: id.to_string() });
    }

    pub fn retry_download(&self, id: &str, account_id: &str, url: &str) {
        self.send(GtkCommand::RetryDownload {
            id: id.to_string(),
            account_id: account_id.to_string(),
            url: url.to_string(),
        });
    }

    pub fn set_scale(&self, account_id: &str, scale: f32) {
        self.send(GtkCommand::SetScale {
            account_id: account_id.to_string(),
            scale,
        });
    }

    pub fn load_url(&self, account_id: &str, url: &str) {
        self.send(GtkCommand::LoadUrl {
            account_id: account_id.to_string(),
            url: url.to_string(),
        });
    }

    pub fn reload(&self, account_id: &str) {
        self.send(GtkCommand::Reload {
            account_id: account_id.to_string(),
        });
    }

    pub fn clear_storage(&self, account_id: &str, operation: StorageOperation, paths: Paths) {
        self.send(GtkCommand::ClearStorage {
            account_id: account_id.to_string(),
            operation,
            paths,
        });
    }

    pub fn close(&self, account_id: &str) {
        self.send(GtkCommand::Close {
            account_id: account_id.to_string(),
        });
    }

    pub fn suspend(&self, account_id: &str) {
        self.send(GtkCommand::Suspend {
            account_id: account_id.to_string(),
        });
    }

    pub fn paste_paths(&self, account_id: &str, paths: Vec<PathBuf>) {
        self.send(GtkCommand::PastePaths {
            account_id: account_id.to_string(),
            paths,
        });
    }

    pub fn set_always_on_top(&self, account_id: &str, enabled: bool) {
        self.send(GtkCommand::SetAlwaysOnTop {
            account_id: account_id.to_string(),
            enabled,
        });
    }

    pub fn quit(&self) {
        self.send(GtkCommand::Quit);
    }
}

pub fn spawn(
    events: Sender<GtkEvent>,
    downloads_dir: PathBuf,
    notifications_enabled: bool,
) -> Result<GtkHandle> {
    let (command_tx, command_rx) = unbounded::<GtkCommand>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    std::thread::Builder::new()
        .name("oxidezap-webkit".into())
        .spawn(move || {
            if let Err(error) = gtk::init() {
                let _ = ready_tx.send(Err(anyhow!("could not initialize GTK: {error}")));
                return;
            }
            install_css();
            let x11 = match X11::connect() {
                Ok(x11) => x11,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let paste = Rc::new(RefCell::new(PasteQueue::start()));
            let state = Rc::new(RefCell::new(GtkState {
                events,
                download_layout: Rc::new(RefCell::new(DownloadLayout::new(downloads_dir))),
                downloads: Rc::new(RefCell::new(HashMap::new())),
                cancelled_downloads: Rc::new(RefCell::new(HashSet::new())),
                notifications_enabled,
                entries: HashMap::new(),
                x11,
                paste: paste.clone(),
                proxy: ProxySettings::default(),
                appearance: AppearanceSettings::default(),
                media_actions: true,
                cadence: PerformanceCadence::NORMAL,
                battery_saver: false,
                reduce_motion: false,
                last_overlay_sync: Instant::now(),
                connection_poll: Rc::new(Cell::new(PerformanceCadence::NORMAL.connection_report)),
                media_transfers: Rc::new(RefCell::new(HashMap::new())),
            }));

            let command_state = state.clone();
            glib::timeout_add_local(Duration::from_millis(20), move || {
                let Ok(mut state) = command_state.try_borrow_mut() else {
                    return glib::ControlFlow::Continue;
                };
                while let Ok(command) = command_rx.try_recv() {
                    handle_command(command, &mut state);
                }
                glib::ControlFlow::Continue
            });

            let paste_state = state.clone();
            glib::timeout_add_local(Duration::from_millis(20), move || {
                dispatch_paste(&paste_state, &paste);
                glib::ControlFlow::Continue
            });

            let sync_state = state.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                if let Ok(mut state) = sync_state.try_borrow_mut() {
                    if state.last_overlay_sync.elapsed() >= state.cadence.overlay_sync {
                        state.last_overlay_sync = Instant::now();
                        sync_overlays(&mut state);
                    }
                }
                glib::ControlFlow::Continue
            });

            let _ = ready_tx.send(Ok(()));
            info!("GTK/WebKit thread ready");
            gtk::main();
        })
        .context("could not start the GTK/WebKit thread")?;

    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .context("GTK/WebKit thread did not start in time")??;

    Ok(GtkHandle { tx: command_tx })
}

struct GtkState {
    events: Sender<GtkEvent>,
    download_layout: Rc<RefCell<DownloadLayout>>,
    downloads: Rc<RefCell<HashMap<String, webkit2gtk::Download>>>,
    cancelled_downloads: Rc<RefCell<HashSet<String>>>,
    notifications_enabled: bool,
    entries: HashMap<String, WebEntry>,
    x11: X11,
    paste: Rc<RefCell<PasteQueue>>,
    /// Proxy settings already applied to every open account. Reapplying them
    /// on unrelated settings changes would reset live connections.
    proxy: ProxySettings,
    /// Experimental stylesheet already applied to every open account. The
    /// command handler ignores redundant updates, so unrelated settings
    /// changes do not remove and re-add the user stylesheet.
    appearance: AppearanceSettings,
    /// Whether the injected media action menu is enabled for new accounts.
    media_actions: bool,
    /// Resolved performance policy. `overlay_sync` gates the native
    /// synchronization timer; `connection_report` is injected into open pages.
    cadence: PerformanceCadence,
    /// Whether the reduced-motion stylesheet is currently applied by battery
    /// saver. Accessibility's independent reduce-motion flag is separate.
    battery_saver: bool,
    /// Whether the user asked for reduced motion in Settings > Accessibility.
    reduce_motion: bool,
    /// Last time the overlay geometry was synchronized, so the fastest timer
    /// can serve both cadences without rebuilding the GLib source.
    last_overlay_sync: Instant,
    /// Current in-page connection poll, shared with every view so a reload
    /// picks up a change made while the previous document was loading.
    connection_poll: Rc<Cell<Duration>>,
    /// Media transfers currently streaming from a page into the download
    /// pipeline. Keyed by the page-generated transfer id.
    media_transfers: Rc<RefCell<HashMap<String, MediaTransfer>>>,
}

struct WebEntry {
    window: gtk::Window,
    webview: webkit2gtk::WebView,
    xid: u32,
    /// Cached GPUI window XID. Synchronization never scans the X11 tree while
    /// this XID is valid.
    parent_xid: Option<u32>,
    composition_xid: Option<u32>,
    visible: bool,
    scale: f32,
    zoom: f64,
    transient_for: Option<u32>,
    last_geometry: Option<(i32, i32, u32, u32)>,
    /// Number of downloads that have started but not finished. Reloading the
    /// page while a download is streaming would cancel it, so the command
    /// handler refuses to do that.
    downloads: Rc<Cell<u32>>,
    /// The media action setting for the current page. The injected script is
    /// re-evaluated after every full load so a setting change survives a
    /// reload, not only the live document.
    media_enabled: Rc<Cell<bool>>,
}

impl WebEntry {
    fn hide(&mut self) {
        if self.window.is_visible() {
            self.window.hide();
        }
        self.last_geometry = None;
    }

    fn set_parent_xid(&mut self, xid: u32) {
        if self.parent_xid != Some(xid) {
            self.parent_xid = Some(xid);
            self.transient_for = None;
            self.last_geometry = None;
        }
    }

    fn forget_parent_xid(&mut self) {
        self.parent_xid = None;
        self.transient_for = None;
        self.last_geometry = None;
    }

    /// A muted microphone or camera is still an active call or recording, so
    /// only `MediaCaptureState::None` counts as idle.
    fn is_capturing(&self) -> bool {
        capture_state_is_active((
            self.webview.camera_capture_state(),
            self.webview.microphone_capture_state(),
            self.webview.display_capture_state(),
        ))
    }
}

fn capture_state_is_active(
    (camera, microphone, display): (MediaCaptureState, MediaCaptureState, MediaCaptureState),
) -> bool {
    [camera, microphone, display]
        .into_iter()
        .any(|state| !matches!(state, MediaCaptureState::None))
}

fn report_capture_state(
    webview: &webkit2gtk::WebView,
    account_id: &str,
    events: &Sender<GtkEvent>,
) {
    let active = capture_state_is_active((
        webview.camera_capture_state(),
        webview.microphone_capture_state(),
        webview.display_capture_state(),
    ));
    let _ = events.send(GtkEvent::CaptureState {
        account_id: account_id.to_string(),
        active,
    });
}

/// The GTK thread rechecks the two conditions that can change between the
/// GPUI policy decision and command handling. A download or a call always wins
/// over a suspend request.
fn suspend_refusal(downloads: u32, capture_active: bool) -> Option<SuspendRefusal> {
    if downloads > 0 {
        Some(SuspendRefusal::DownloadActive)
    } else if capture_active {
        Some(SuspendRefusal::CaptureActive)
    } else {
        None
    }
}

fn handle_command(command: GtkCommand, state: &mut GtkState) {
    match command {
        GtkCommand::Create {
            account_id,
            parent_xid,
            composition_xid,
            data_dir,
            cache_dir,
            url,
            html,
            show,
            scale,
            zoom,
            writing,
            proxy,
            appearance,
            media_actions,
            reduce_motion,
        } => {
            match create_entry(
                &account_id,
                &data_dir,
                &cache_dir,
                &url,
                html.as_deref(),
                show,
                scale,
                zoom,
                &writing,
                &proxy,
                &appearance,
                composition_xid,
                state.events.clone(),
                state.download_layout.clone(),
                state.downloads.clone(),
                state.cancelled_downloads.clone(),
                state.notifications_enabled,
                state.paste.clone(),
                media_actions,
                state.battery_saver,
                reduce_motion,
                state.connection_poll.clone(),
                state.media_transfers.clone(),
            ) {
                Ok(mut entry) => {
                    if let Some(parent_xid) = parent_xid {
                        entry.set_parent_xid(parent_xid);
                    }
                    if let Some(previous) = state.entries.remove(&account_id) {
                        previous.window.close();
                    }
                    let xid = entry.xid;
                    state.entries.insert(account_id.clone(), entry);
                    state.proxy = proxy;
                    state.appearance = appearance;
                    state.media_actions = media_actions;
                    state.reduce_motion = reduce_motion;
                    let _ = state.events.send(GtkEvent::Ready { account_id, xid });
                }
                Err(error) => {
                    warn!(%error, "could not create a WebKit view");
                    let _ = state.events.send(GtkEvent::Error {
                        account_id,
                        message: error.to_string(),
                    });
                }
            }
        }
        GtkCommand::Show { account_id } => {
            if let Some(entry) = state.entries.get_mut(&account_id) {
                entry.visible = true;
                entry.last_geometry = None;
            }
        }
        GtkCommand::Hide { account_id } => {
            if let Some(entry) = state.entries.get_mut(&account_id) {
                entry.visible = false;
                entry.hide();
            }
        }
        GtkCommand::SetEmbeddedBounds {
            account_id,
            width,
            height,
        } => {
            if let Some(entry) = state.entries.get(&account_id) {
                if entry.composition_xid.is_some() {
                    entry.window.resize(width, height);
                    if let Some(gdk_window) = entry.window.window() {
                        gdk_window.move_resize(0, 0, width, height);
                    }
                }
            }
        }
        GtkCommand::SetZoom { account_id, zoom } => {
            if let Some(entry) = state.entries.get_mut(&account_id) {
                entry.zoom = zoom;
                entry.webview.set_zoom_level(zoom);
            }
        }
        GtkCommand::SetWriting { writing } => {
            for entry in state.entries.values() {
                if let Some(context) = entry.webview.context() {
                    apply_spell_checking(&context, &writing);
                }
            }
        }
        GtkCommand::SetProxy { proxy } => {
            if state.proxy == proxy {
                return;
            }
            for entry in state.entries.values() {
                if let Some(manager) = entry.webview.website_data_manager() {
                    apply_proxy(&manager, &proxy);
                }
            }
            state.proxy = proxy;
        }
        GtkCommand::SetAppearance { appearance } => {
            if state.appearance == appearance {
                return;
            }
            for entry in state.entries.values() {
                if let Some(manager) = entry.webview.user_content_manager() {
                    apply_user_stylesheets(
                        &manager,
                        &appearance,
                        crate::settings::reduce_motion_active(
                            state.battery_saver,
                            state.reduce_motion,
                        ),
                    );
                }
            }
            state.appearance = appearance;
        }
        GtkCommand::SetPerformance {
            battery_saver,
            cadence,
        } => {
            let saver_changed = state.battery_saver != battery_saver;
            let poll_changed = state.connection_poll.get() != cadence.connection_report;
            state.battery_saver = battery_saver;
            state.cadence = cadence;
            state.connection_poll.set(cadence.connection_report);
            if saver_changed || poll_changed {
                let poll_script = connection_poll_script(cadence.connection_report);
                for entry in state.entries.values() {
                    if saver_changed {
                        if let Some(manager) = entry.webview.user_content_manager() {
                            apply_user_stylesheets(
                                &manager,
                                &state.appearance,
                                crate::settings::reduce_motion_active(
                                    battery_saver,
                                    state.reduce_motion,
                                ),
                            );
                        }
                    }
                    entry.webview.evaluate_javascript(
                        &poll_script,
                        None,
                        None,
                        None::<&gtk::gio::Cancellable>,
                        |_result| {},
                    );
                }
            }
        }
        GtkCommand::SetAccessibility { reduce_motion } => {
            if state.reduce_motion == reduce_motion {
                return;
            }
            state.reduce_motion = reduce_motion;
            let reduce_motion =
                crate::settings::reduce_motion_active(state.battery_saver, state.reduce_motion);
            for entry in state.entries.values() {
                if let Some(manager) = entry.webview.user_content_manager() {
                    apply_user_stylesheets(&manager, &state.appearance, reduce_motion);
                }
            }
        }
        GtkCommand::SetMediaActions { enabled } => {
            if state.media_actions == enabled {
                return;
            }
            state.media_actions = enabled;
            let script = media_toggle_script(enabled);
            for entry in state.entries.values() {
                entry.media_enabled.set(enabled);
                entry.webview.evaluate_javascript(
                    &script,
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    |_result| {},
                );
            }
        }
        GtkCommand::SetDownloadLayout(layout) => {
            *state.download_layout.borrow_mut() = layout;
        }
        GtkCommand::CancelDownload { id } => {
            // Media actions write their own bytes, so the WebKit download
            // registry does not know their ids. Cancel the in-memory transfer
            // first and remove its empty reservation; the page stops sending
            // when its `fetch` is rejected or the transfer is dropped.
            if let Some(transfer) = state.media_transfers.borrow_mut().remove(&id) {
                transfer
                    .active_downloads
                    .set(transfer.active_downloads.get().saturating_sub(1));
                if let Some(path) = &transfer.path {
                    discard_empty_reservation(path);
                }
                let _ = state.events.send(GtkEvent::DownloadFinished {
                    id,
                    success: false,
                    cancelled: true,
                });
                return;
            }
            let download = state.downloads.borrow().get(&id).cloned();
            if let Some(download) = download {
                state.cancelled_downloads.borrow_mut().insert(id);
                download.cancel();
            }
        }
        GtkCommand::RetryDownload {
            id,
            account_id,
            url,
        } => {
            let context = state
                .entries
                .get(&account_id)
                .and_then(|entry| entry.webview.context());
            let started = context
                .as_ref()
                .and_then(|context| context.download_uri(&url))
                .is_some();
            if started {
                info!(account_id = %account_id, url = %url, "retrying a download");
            } else {
                let _ = state.events.send(GtkEvent::DownloadRetryFailed {
                    id,
                    message: "The account window is not available; open that account and start the download again."
                        .to_string(),
                });
            }
        }
        GtkCommand::SetScale { account_id, scale } => {
            if let Some(entry) = state.entries.get_mut(&account_id) {
                if (entry.scale - scale).abs() > f32::EPSILON {
                    entry.scale = scale;
                    entry.last_geometry = None;
                }
            }
        }
        GtkCommand::LoadUrl { account_id, url } => {
            if let Some(entry) = state.entries.get(&account_id) {
                entry.webview.load_uri(&url);
            }
        }
        GtkCommand::Reload { account_id } => {
            if let Some(entry) = state.entries.get(&account_id) {
                if entry.downloads.get() > 0 {
                    warn!(
                        %account_id,
                        "skipped a WhatsApp Web reload while a download is active"
                    );
                    let _ = state.events.send(GtkEvent::ReloadBlocked { account_id });
                } else {
                    entry.webview.reload();
                }
            }
        }
        GtkCommand::ClearStorage {
            account_id,
            operation,
            paths,
        } => {
            // Close the view on the GTK thread before the profile is touched.
            // WebKitGTK 2.0.2 does not expose a coordinated
            // `WebsiteDataManager::clear`, so removing the view and letting the
            // web process release its files is the supported path.
            if let Some(entry) = state.entries.remove(&account_id) {
                if let Some(context) = entry.webview.context() {
                    context.clear_cache();
                }
                entry.window.close();
            }
            let events = state.events.clone();
            let worker_account = account_id.clone();
            let spawned = std::thread::Builder::new()
                .name("oxidezap-storage".to_string())
                .spawn(move || {
                    // The web process releases its profile handles shortly
                    // after the view is destroyed. Deleting immediately can
                    // race SQLite and leave the profile half recreated.
                    std::thread::sleep(Duration::from_millis(400));
                    let result = storage::perform(&paths, &worker_account, operation)
                        .map(|outcome| outcome.freed_bytes)
                        .map_err(|error| error.to_string());
                    let _ = events.send(GtkEvent::StorageFinished {
                        account_id: worker_account,
                        operation,
                        result,
                    });
                });
            if let Err(error) = spawned {
                warn!(%error, "could not start the storage worker");
                let _ = state.events.send(GtkEvent::StorageFinished {
                    account_id,
                    operation,
                    result: Err(error.to_string()),
                });
            }
        }
        GtkCommand::Close { account_id } => {
            if let Some(entry) = state.entries.remove(&account_id) {
                entry.window.close();
            }
        }
        GtkCommand::Suspend { account_id } => {
            let refusal = state
                .entries
                .get(&account_id)
                .and_then(|entry| suspend_refusal(entry.downloads.get(), entry.is_capturing()));
            match refusal {
                Some(reason) => {
                    warn!(%account_id, reason = reason.label(), "refused to suspend an account");
                    let _ = state
                        .events
                        .send(GtkEvent::SuspendRefused { account_id, reason });
                }
                None => {
                    // Destroying the GTK window destroys the WebView and lets
                    // WebKit terminate the web process, which is what releases
                    // the memory. The WebsiteDataManager profile stays on disk
                    // and a later Create command reuses it.
                    if let Some(entry) = state.entries.remove(&account_id) {
                        entry.window.close();
                    }
                    info!(%account_id, "suspended the WebKit view");
                    let _ = state.events.send(GtkEvent::Suspended { account_id });
                }
            }
        }
        GtkCommand::PastePaths { account_id, paths } => {
            queue_share_paths(
                &account_id,
                &paths,
                &state.paste,
                &state.events,
                state.entries.contains_key(&account_id),
            );
        }
        GtkCommand::SetAlwaysOnTop {
            account_id,
            enabled,
        } => {
            if let Some(entry) = state.entries.get(&account_id) {
                entry.window.set_keep_above(enabled);
            }
        }
        GtkCommand::Quit => {
            for (_, entry) in state.entries.drain() {
                entry.window.close();
            }
            gtk::main_quit();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_composition_container(parent_xid: u32) -> Result<u32> {
    let (connection, screen_num) =
        x11rb::connect(None).context("could not connect to X11 for composition")?;
    let _screen = &connection.setup().roots[screen_num];
    let parent_depth = connection
        .get_geometry(parent_xid)
        .context("could not inspect the composition surface")?
        .reply()
        .context("could not read the composition surface geometry")?
        .depth;
    let parent_visual = connection
        .get_window_attributes(parent_xid)
        .context("could not inspect the composition surface attributes")?
        .reply()
        .context("could not read the composition surface attributes")?
        .visual;
    let window = connection
        .generate_id()
        .context("could not allocate a composition container id")?;
    connection
        .create_window(
            parent_depth,
            window,
            parent_xid,
            0,
            0,
            800,
            600,
            0,
            WindowClass::INPUT_OUTPUT,
            parent_visual,
            &CreateWindowAux::new(),
        )
        .context("could not create the composition container")?
        .check()
        .context("X11 rejected the composition container")?;
    connection
        .map_window(window)
        .context("could not map the composition container")?
        .check()
        .context("X11 rejected mapping the composition container")?;
    connection
        .flush()
        .context("could not flush the composition container")?;
    Ok(window)
}

#[allow(clippy::too_many_arguments)]
fn create_entry(
    account_id: &str,
    data_dir: &Path,
    cache_dir: &Path,
    url: &str,
    html: Option<&str>,
    show: bool,
    scale: f32,
    zoom: f64,
    writing: &WritingSettings,
    proxy: &ProxySettings,
    appearance: &AppearanceSettings,
    composition_xid: Option<u32>,
    events: Sender<GtkEvent>,
    download_layout: Rc<RefCell<DownloadLayout>>,
    download_registry: Rc<RefCell<HashMap<String, webkit2gtk::Download>>>,
    cancelled_downloads: Rc<RefCell<HashSet<String>>>,
    notifications_enabled: bool,
    paste: Rc<RefCell<PasteQueue>>,
    media_actions: bool,
    battery_saver: bool,
    reduce_motion: bool,
    connection_poll: Rc<Cell<Duration>>,
    media_transfers: Rc<RefCell<HashMap<String, MediaTransfer>>>,
) -> Result<WebEntry> {
    crate::paths::create_private_dir(data_dir)?;
    crate::paths::create_private_dir(cache_dir)?;
    if let Some(account_dir) = data_dir.parent() {
        crate::paths::create_private_dir(account_dir)?;
    }

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("OxideZap content");
    window.set_decorated(false);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_resizable(true);
    if composition_xid.is_none() {
        window.set_type_hint(gtk::gdk::WindowTypeHint::Utility);
    }
    window.set_default_size(800, 600);

    let manager = webkit2gtk::WebsiteDataManager::builder()
        .base_data_directory(data_dir.to_string_lossy().as_ref())
        .base_cache_directory(cache_dir.to_string_lossy().as_ref())
        .build();
    apply_proxy(&manager, proxy);
    let context = webkit2gtk::WebContext::with_website_data_manager(&manager);
    apply_spell_checking(&context, writing);
    let user_content = UserContentManager::new();
    connect_connection_script(
        &user_content,
        account_id,
        events.clone(),
        connection_poll.get(),
    );
    apply_user_stylesheets(
        &user_content,
        appearance,
        crate::settings::reduce_motion_active(battery_saver, reduce_motion),
    );
    let downloads = Rc::new(Cell::new(0));
    let media_enabled = Rc::new(Cell::new(media_actions));
    connect_media_script(
        &user_content,
        account_id,
        events.clone(),
        download_layout.clone(),
        media_transfers,
        downloads.clone(),
        media_actions,
    );
    let webview =
        webkit2gtk::WebView::new_with_context_and_user_content_manager(&context, &user_content);
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    if let Some(settings) = WebViewExt::settings(&webview) {
        settings.set_enable_javascript(true);
        settings.set_enable_webrtc(true);
        settings.set_enable_media_stream(true);
        settings.set_enable_mediasource(true);
        settings.set_enable_smooth_scrolling(true);
        settings.set_enable_developer_extras(cfg!(debug_assertions));
        settings.set_enable_write_console_messages_to_stdout(cfg!(debug_assertions));
    }
    webview.set_zoom_level(zoom);

    connect_webview_signals(
        &webview,
        account_id,
        events.clone(),
        notifications_enabled,
        window.clone(),
        media_enabled.clone(),
        connection_poll,
    );
    let drop_events = events.clone();
    connect_downloads(
        &context,
        account_id,
        events,
        download_layout,
        download_registry,
        cancelled_downloads,
        downloads.clone(),
    );
    let paste_account = account_id.to_string();
    let drop_paste = paste.clone();
    webview.connect_key_press_event(move |_webview, event| {
        if try_paste_clipboard(&paste_account, event, &paste) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    let (overlay, drop_hint) = build_drop_overlay(&webview);
    setup_drop_target(&webview, account_id, drop_paste, drop_hint, drop_events);

    let embedded_xid = if let Some(parent_xid) = composition_xid {
        let container_xid = create_composition_container(parent_xid)?;
        let display =
            gtk::gdk::Display::default().context("no GDK display for the composition surface")?;
        let display = display
            .downcast::<gdkx11::X11Display>()
            .map_err(|_| anyhow!("the GDK display is not X11"))?;
        let parent = gdkx11::X11Window::foreign_new_for_display(&display, container_xid as u64);
        window.set_has_window(true);
        window.connect_realize(move |window| window.set_window(parent.clone().into()));
        window.realize();
        window.add(&overlay);
        window.show_all();
        Some(container_xid)
    } else {
        window.add(&overlay);
        window.realize();
        None
    };

    let xid = window
        .window()
        .map(|gdk_window| {
            gdk_window
                .downcast::<gdkx11::X11Window>()
                .map(|x11| x11.xid() as u32)
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if composition_xid.is_some() {
        window.show_all();
    }

    if let Some(html) = html {
        webview.load_html(html, Some(WHATSAPP_URL));
    } else {
        webview.load_uri(url);
    }

    Ok(WebEntry {
        window,
        webview,
        xid,
        parent_xid: None,
        composition_xid: embedded_xid,
        visible: show,
        scale,
        zoom,
        transient_for: None,
        last_geometry: None,
        downloads,
        media_enabled,
    })
}

/// Pauses decorative motion when battery saver or the Reduce motion preference
/// is active. Media playback and layout are untouched; only CSS animation and
/// transition timing is collapsed so WhatsApp's interface stops repainting for
/// effects it no longer shows.
const REDUCED_MOTION_STYLESHEET: &str = r#"
*, *::before, *::after {
  animation-duration: 0.001ms !important;
  animation-delay: 0ms !important;
  animation-iteration-count: 1 !important;
  transition-duration: 0.001ms !important;
  transition-delay: 0ms !important;
}
"#;

/// Replaces the user stylesheets on a content manager. Experimental CSS and
/// the reduced-motion stylesheet share the manager, so both are rebuilt
/// together; removing only one would also detach the other.
///
/// WebKit injects user stylesheets at document start and keeps them attached
/// to the manager, so they survive WhatsApp Web's in-page SPA navigation. The
/// allow list is the origin gate: WebKit never applies the CSS to a document
/// outside `https://web.whatsapp.com`. Stylesheets are CSS only; this function
/// never evaluates the file as JavaScript.
fn apply_user_stylesheets(
    manager: &UserContentManager,
    appearance: &AppearanceSettings,
    reduce_motion: bool,
) {
    manager.remove_all_style_sheets();
    if let Some(css) = appearance.active_css() {
        if let Err(error) = validate_custom_css(css) {
            warn!(%error, "refusing to inject invalid experimental CSS");
        } else {
            manager.add_style_sheet(&UserStyleSheet::new(
                css,
                UserContentInjectedFrames::TopFrame,
                UserStyleLevel::User,
                CUSTOM_CSS_ALLOW_PATTERNS,
                &[],
            ));
            info!(bytes = css.len(), "applied experimental custom CSS");
        }
    }
    if reduce_motion {
        manager.add_style_sheet(&UserStyleSheet::new(
            REDUCED_MOTION_STYLESHEET,
            UserContentInjectedFrames::TopFrame,
            UserStyleLevel::User,
            CUSTOM_CSS_ALLOW_PATTERNS,
            &[],
        ));
    }
}

const CONNECTION_MESSAGE_HANDLER: &str = "oxidezapConnection";
const CONNECTION_POLL_PLACEHOLDER: &str = "__OXIDEZAP_CONNECTION_POLL_MS__";

/// Reports the browser's online state and whether the QR login screen is
/// showing. User scripts are injected by WebKit itself, so the page CSP does
/// not block them. The script deliberately reads only `navigator.onLine` and
/// a short list of known QR containers; it does not inspect chat content.
///
/// The interval is a recursive timeout instead of `setInterval` so the native
/// side can change it without reloading the page. Battery saver raises it from
/// 2 s to 5 s; `online`/`offline` events still report immediately.
const CONNECTION_SCRIPT: &str = r#"
(function () {
  if (location.hostname !== "web.whatsapp.com") return;
  if (window.__oxidezapConnection) return;
  window.__oxidezapConnection = true;

  var qrSelectors = [
    '[data-testid="qrcode"]',
    'canvas[aria-label*="QR"]',
    'canvas[aria-label*="Scan"]',
    '[aria-label*="Scan this QR"]',
    'div[data-ref] canvas'
  ];

  function loginPage() {
    for (var i = 0; i < qrSelectors.length; i++) {
      if (document.querySelector(qrSelectors[i])) return true;
    }
    return false;
  }

  var last = "";
  function report() {
    var online = navigator.onLine === false ? "0" : "1";
    var login = loginPage() ? "1" : "0";
    var payload = "online=" + online + "&login=" + login;
    if (payload === last) return;
    last = payload;
    try {
      window.webkit.messageHandlers.oxidezapConnection.postMessage(payload);
    } catch (error) {
      last = "";
    }
  }

  var defaultPollMs = __OXIDEZAP_CONNECTION_POLL_MS__;
  function schedule() {
    setTimeout(function () {
      report();
      schedule();
    }, window.__oxidezapConnectionPoll || defaultPollMs);
  }

  window.addEventListener("online", report);
  window.addEventListener("offline", report);
  schedule();
  report();
})();
"#;

/// Script injected after a page finishes loading and when the policy changes.
/// The connection script reads the variable on its next tick, so this takes
/// effect without reloading WhatsApp Web.
fn connection_poll_script(poll: Duration) -> String {
    format!(
        "window.__oxidezapConnectionPoll = {};",
        poll.as_millis().max(1)
    )
}

fn connect_connection_script(
    manager: &UserContentManager,
    account_id: &str,
    events: Sender<GtkEvent>,
    poll: Duration,
) {
    if !manager.register_script_message_handler(CONNECTION_MESSAGE_HANDLER) {
        warn!(account_id = %account_id, "could not register the connection message handler");
        return;
    }
    let script = CONNECTION_SCRIPT.replace(
        CONNECTION_POLL_PLACEHOLDER,
        &poll.as_millis().max(1).to_string(),
    );
    manager.add_script(&UserScript::new(
        &script,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));

    let script_account = account_id.to_string();
    manager.connect_script_message_received(
        Some(CONNECTION_MESSAGE_HANDLER),
        move |_manager, result| {
            let Some(value) = result.js_value() else {
                return;
            };
            let message = value.to_string();
            let Some(report) = parse_connection_report(&message) else {
                debug!(
                    account_id = %script_account,
                    %message,
                    "ignored an unknown connection report"
                );
                return;
            };
            let _ = events.send(GtkEvent::ConnectionState {
                account_id: script_account.clone(),
                report,
            });
        },
    );
}

fn parse_connection_report(message: &str) -> Option<ConnectionReport> {
    let mut state = None;
    let mut login_page = None;
    for part in message.split('&') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match (key, value) {
            ("online", "0") => state = Some(ConnectionState::Offline),
            ("online", "1") => state = Some(ConnectionState::Online),
            ("login", "0") => login_page = Some(false),
            ("login", "1") => login_page = Some(true),
            _ => {}
        }
    }
    Some(ConnectionReport {
        state: state?,
        login_page: login_page?,
    })
}

const MEDIA_MESSAGE_HANDLER: &str = "oxidezapMedia";
const MEDIA_ENABLED_PLACEHOLDER: &str = "__OXIDEZAP_MEDIA_ENABLED__";
/// Media actions are explicit and bounded: a user-initiated save or copy is
/// transferred through the page, where WhatsApp has already decrypted the
/// bytes for the signed-in account. The cap keeps a single IPC transfer from
/// pinning hundreds of megabytes; larger media must use WhatsApp's own
/// download control.
const MEDIA_MAX_BYTES: u64 = 64 * 1024 * 1024;
const MEDIA_CHUNK_BYTES: usize = 384 * 1024;
/// Base64 expands three bytes to four characters and adds padding.
const MEDIA_MAX_CHUNK_BASE64: usize = MEDIA_CHUNK_BYTES.div_ceil(3) * 4 + 8;
const MEDIA_MESSAGE_MAX_BYTES: usize = MEDIA_MAX_CHUNK_BASE64 + 4096;
const MEDIA_TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaAction {
    Save,
    Open,
    Copy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaRequest {
    action: MediaAction,
    kind: MediaKind,
    url: String,
    filename: String,
    total_bytes: u64,
    view_once: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaRejection {
    ViewOnce,
    UnsupportedAction,
    UnsupportedScheme,
    UntrustedOrigin,
    TooLarge,
    Empty,
    Malformed,
}

impl MediaRejection {
    fn message(self) -> &'static str {
        match self {
            MediaRejection::ViewOnce => "View-once media cannot be saved, opened, or copied.",
            MediaRejection::UnsupportedAction => {
                "Only images can be copied. Use Save or Open for this media."
            }
            MediaRejection::UnsupportedScheme => {
                "Only WhatsApp media exposed as blob:, data:, or a WhatsApp host can be saved."
            }
            MediaRejection::UntrustedOrigin => {
                "Media from another origin was rejected. Only WhatsApp media can be saved."
            }
            MediaRejection::TooLarge => {
                "This media is larger than the 64 MB limit. Use WhatsApp's own download control."
            }
            MediaRejection::Empty => "WhatsApp did not expose any data for this media.",
            MediaRejection::Malformed => "The media link is not valid.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaFailureReason {
    Fetch,
    Expired,
    TooLarge,
    Unsupported,
}

impl MediaFailureReason {
    fn message(self) -> &'static str {
        match self {
            MediaFailureReason::Fetch => {
                "OxideZap could not read this media. Reopen it and try again."
            }
            MediaFailureReason::Expired => {
                "This media expired or was revoked by WhatsApp. Reopen it and try again."
            }
            MediaFailureReason::TooLarge => {
                "This media is larger than the 64 MB limit. Use WhatsApp's own download control."
            }
            MediaFailureReason::Unsupported => "This media type is not supported by OxideZap.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MediaMessage {
    Begin {
        id: String,
        request: MediaRequest,
    },
    Chunk {
        id: String,
        data: String,
    },
    End {
        id: String,
    },
    Failed {
        id: Option<String>,
        reason: MediaFailureReason,
    },
}

/// Returns `Ok` when a media URL may be transferred. Only data the page
/// already exposed for the signed-in WhatsApp account is accepted: decrypted
/// `blob:` URLs whose origin is WhatsApp, media `data:` URLs, and plain
/// WhatsApp hosts. No other scheme, host, or lookalike domain is allowed.
fn allowed_media_url(raw: &str) -> Result<(), MediaRejection> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 16 * 1024 {
        return Err(MediaRejection::Malformed);
    }

    if let Some(inner) = raw.strip_prefix("blob:") {
        let Ok(url) = url::Url::parse(inner) else {
            return Err(MediaRejection::Malformed);
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Err(MediaRejection::UnsupportedScheme);
        }
        let Some(host) = url.host_str() else {
            return Err(MediaRejection::Malformed);
        };
        return if links::is_whatsapp_host(host) || links::is_internal_url(inner) {
            Ok(())
        } else {
            Err(MediaRejection::UntrustedOrigin)
        };
    }

    if let Some(rest) = raw.strip_prefix("data:") {
        let header = rest.split(',').next().unwrap_or_default();
        let mime = header
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if mime == "image/svg+xml" || mime == "image/svg" || mime.starts_with("text/") {
            return Err(MediaRejection::UnsupportedScheme);
        }
        return if mime.starts_with("image/")
            || mime.starts_with("video/")
            || mime.starts_with("audio/")
            || mime == "application/pdf"
        {
            Ok(())
        } else {
            Err(MediaRejection::UnsupportedScheme)
        };
    }

    let Ok(url) = url::Url::parse(raw) else {
        return Err(MediaRejection::Malformed);
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(MediaRejection::UnsupportedScheme);
    }
    let Some(host) = url.host_str() else {
        return Err(MediaRejection::Malformed);
    };
    if links::is_whatsapp_host(host) || links::is_internal_url(raw) {
        Ok(())
    } else {
        Err(MediaRejection::UntrustedOrigin)
    }
}

fn validate_media_request(request: &MediaRequest) -> Result<(), MediaRejection> {
    if request.view_once {
        return Err(MediaRejection::ViewOnce);
    }
    if request.action == MediaAction::Copy && request.kind != MediaKind::Image {
        return Err(MediaRejection::UnsupportedAction);
    }
    if request.total_bytes == 0 {
        return Err(MediaRejection::Empty);
    }
    if request.total_bytes > MEDIA_MAX_BYTES {
        return Err(MediaRejection::TooLarge);
    }
    allowed_media_url(&request.url)
}

fn media_id(value: Option<&serde_json::Value>) -> Option<String> {
    let id = value?.as_str()?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(id.to_string())
}

fn parse_media_message(message: &str) -> Option<MediaMessage> {
    if message.len() > MEDIA_MESSAGE_MAX_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(message).ok()?;
    let object = value.as_object()?;
    match object.get("type")?.as_str()? {
        "begin" => {
            let id = media_id(object.get("id"))?;
            let action = match object.get("action")?.as_str()? {
                "save" => MediaAction::Save,
                "open" => MediaAction::Open,
                "copy" => MediaAction::Copy,
                _ => return None,
            };
            let kind = match object.get("kind")?.as_str()? {
                "image" => MediaKind::Image,
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                "document" => MediaKind::Document,
                _ => return None,
            };
            let url = object.get("url")?.as_str()?.to_string();
            let filename = object
                .get("filename")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let total_bytes = object.get("size")?.as_u64()?;
            let view_once = object
                .get("viewOnce")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Some(MediaMessage::Begin {
                id,
                request: MediaRequest {
                    action,
                    kind,
                    url,
                    filename,
                    total_bytes,
                    view_once,
                },
            })
        }
        "chunk" => Some(MediaMessage::Chunk {
            id: media_id(object.get("id"))?,
            data: object.get("data")?.as_str()?.to_string(),
        }),
        "end" => Some(MediaMessage::End {
            id: media_id(object.get("id"))?,
        }),
        "error" => Some(MediaMessage::Failed {
            id: media_id(object.get("id")),
            reason: match object.get("reason")?.as_str()? {
                "fetch" => MediaFailureReason::Fetch,
                "expired" => MediaFailureReason::Expired,
                "too-large" => MediaFailureReason::TooLarge,
                "unsupported" => MediaFailureReason::Unsupported,
                _ => return None,
            },
        }),
        _ => None,
    }
}

/// Decodes one base64 chunk and enforces the declared transfer size. The size
/// is declared by the page before any bytes are sent and was already bounded
/// by `validate_media_request`, so a lying page cannot grow the buffer beyond
/// the user-visible limit.
fn decode_media_chunk(data: &str, received: u64, expected: u64) -> Result<Vec<u8>, MediaRejection> {
    if data.len() > MEDIA_MAX_CHUNK_BASE64 {
        return Err(MediaRejection::TooLarge);
    }
    if data.len() % 4 != 0
        || !data
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return Err(MediaRejection::Malformed);
    }
    let decoded = glib::base64_decode(data);
    if decoded.is_empty() && !data.is_empty() {
        return Err(MediaRejection::Malformed);
    }
    if received.saturating_add(decoded.len() as u64) > expected {
        return Err(MediaRejection::TooLarge);
    }
    Ok(decoded)
}

/// A small, self-contained context menu for media the user explicitly
/// right-clicks. It never scans chats, observes mutations, or reads message
/// text: the handler only inspects the element under the pointer and walks a
/// bounded number of ancestors to reject view-once, sticker, and avatar
/// surfaces. All bytes are fetched in the page context (where WhatsApp has
/// already decrypted them) and streamed to Rust, which revalidates the origin,
/// size, and action before touching the filesystem or clipboard.
const MEDIA_SCRIPT: &str = r#"
(function () {
  if (window.top !== window.self) return;
  if (location.hostname !== "web.whatsapp.com") return;
  if (window.__oxidezapMediaActions) return;

  var DEFAULT_ENABLED = __OXIDEZAP_MEDIA_ENABLED__;
  var MENU_ID = "oxidezap-media-menu";
  var MAX_BYTES = 67108864;
  var CHUNK_BYTES = 393216;
  var menu = null;

  function enabled() {
    return window.__oxidezapMediaEnabled === undefined
      ? DEFAULT_ENABLED
      : !!window.__oxidezapMediaEnabled;
  }

  function post(payload) {
    try {
      window.webkit.messageHandlers.oxidezapMedia.postMessage(JSON.stringify(payload));
    } catch (error) {}
  }

  function closeMenu() {
    if (menu && menu.parentNode) menu.parentNode.removeChild(menu);
    menu = null;
  }

  function attr(node, name) {
    return (node && node.getAttribute && node.getAttribute(name)) || "";
  }

  function marker(node) {
    return (attr(node, "data-testid") + " " + attr(node, "aria-label") + " " + attr(node, "title")).toLowerCase();
  }

  function viewOnce(node) {
    for (var depth = 0; node && depth < 12; depth += 1, node = node.parentElement) {
      var text = marker(node);
      if (text.indexOf("view-once") >= 0 || text.indexOf("viewonce") >= 0) return true;
      if (/view[\s-]?once|ver una vez|visualizar una vez|visualiza\u00e7\u00e3o \u00fanica|vu une fois|einmalig/.test(text)) return true;
    }
    return false;
  }

  function ignored(node) {
    for (var depth = 0; node && depth < 8; depth += 1, node = node.parentElement) {
      var text = marker(node);
      if (text.indexOf("sticker") >= 0 || text.indexOf("avatar") >= 0 || text.indexOf("emoji") >= 0) return true;
    }
    return false;
  }

  function whatsappUrl(value) {
    try {
      var parsed = new URL(value);
      if (parsed.protocol === "blob:") return true;
      if (parsed.protocol === "data:") {
        return /^data:(image|video|audio)\//i.test(value) || /^data:application\/pdf/i.test(value);
      }
      return /(^|\.)whatsapp\.com$/.test(parsed.hostname) || /(^|\.)whatsapp\.net$/.test(parsed.hostname);
    } catch (error) {
      return false;
    }
  }

  function nameFrom(node) {
    var name = attr(node, "download") || attr(node, "title") || "";
    if (!name) {
      var image = node.querySelector && node.querySelector("img[alt]");
      name = image ? attr(image, "alt") : "";
    }
    name = name.trim();
    if (!name || name.indexOf("/") >= 0 || name.indexOf("\\") >= 0 || name.length > 160) return "";
    return name;
  }

  function targetFor(event) {
    var element = event.target;
    if (!element || !element.closest) return null;
    var documentLink = element.closest("a[download][href]");
    if (documentLink && whatsappUrl(documentLink.href) && !ignored(documentLink) && !viewOnce(documentLink)) {
      return { url: documentLink.href, kind: "document", filename: nameFrom(documentLink) };
    }
    var media = element.closest("img, picture, video, audio");
    if (media) {
      var source = media.currentSrc || media.src || "";
      if (!source) {
        var fallback = media.querySelector && media.querySelector("source[src]");
        source = fallback ? fallback.src : "";
      }
      if (!source || ignored(media) || viewOnce(media)) return null;
      var tag = media.tagName;
      var kind = tag === "VIDEO" ? "video" : tag === "AUDIO" ? "audio" : "image";
      return { url: source, kind: kind, filename: nameFrom(media) };
    }
    var link = element.closest("a[href]");
    if (!link) return null;
    var href = link.href || "";
    if (!/^(blob:|data:)/i.test(href) && !link.hasAttribute("download")) return null;
    if (!whatsappUrl(href) || ignored(link) || viewOnce(link)) return null;
    return { url: href, kind: "document", filename: nameFrom(link) };
  }

  function item(box, textValue, action) {
    var row = document.createElement("div");
    row.setAttribute("role", "menuitem");
    row.setAttribute("tabindex", "-1");
    row.textContent = textValue;
    row.style.cssText = 'padding: 7px 10px; border-radius: 6px; cursor: pointer; white-space: nowrap; outline: none;';
    row.addEventListener("mouseenter", function () { row.style.background = '#2a3942'; });
    row.addEventListener("mouseleave", function () { row.style.background = 'transparent'; });
    row.addEventListener("focus", function () { row.style.background = '#2a3942'; });
    row.addEventListener("blur", function () { row.style.background = 'transparent'; });
    row.addEventListener("click", function (event) {
      event.preventDefault();
      event.stopPropagation();
      closeMenu();
      action();
    });
    box.appendChild(row);
    return row;
  }

  function showMenu(x, y, target) {
    closeMenu();
    var box = document.createElement("div");
    box.id = MENU_ID;
    box.setAttribute("role", "menu");
    box.style.cssText = 'position: fixed; z-index: 2147483647; min-width: 196px; padding: 6px; border-radius: 8px; background: #233138; color: #e9edef; box-shadow: 0 8px 24px rgba(0, 0, 0, .45); font: 13px/1.4 system-ui, sans-serif;';
    box.addEventListener("mousedown", function (event) { event.stopPropagation(); });
    box.addEventListener("contextmenu", function (event) { event.preventDefault(); event.stopPropagation(); });
    item(box, "Save to Downloads", function () { transfer(target, "save"); });
    item(box, "Open in system viewer", function () { transfer(target, "open"); });
    if (target.kind === "image") item(box, "Copy image", function () { transfer(target, "copy"); });
    document.body.appendChild(box);
    var rect = box.getBoundingClientRect();
    var left = Math.max(8, Math.min(x, window.innerWidth - rect.width - 8));
    var top = Math.max(8, Math.min(y, window.innerHeight - rect.height - 8));
    box.style.left = left + "px";
    box.style.top = top + "px";
    menu = box;
  }

  function moveFocus(delta) {
    if (!menu) return;
    var rows = Array.prototype.slice.call(menu.querySelectorAll('[role="menuitem"]'));
    if (!rows.length) return;
    var index = rows.indexOf(document.activeElement);
    index = index < 0 ? (delta > 0 ? 0 : rows.length - 1) : (index + delta + rows.length) % rows.length;
    rows[index].setAttribute("tabindex", "0");
    rows[index].focus();
  }

  window.addEventListener("contextmenu", function (event) {
    if (!enabled()) {
      closeMenu();
      return;
    }
    var target = targetFor(event);
    if (!target) {
      closeMenu();
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    showMenu(event.clientX, event.clientY, target);
  }, true);

  window.addEventListener("mousedown", function (event) {
    if (menu && !menu.contains(event.target)) closeMenu();
  }, true);

  window.addEventListener("keydown", function (event) {
    if (!menu) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeMenu();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      event.stopPropagation();
      moveFocus(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if ((event.key === "Enter" || event.key === " ") && menu.contains(document.activeElement)) {
      event.preventDefault();
      event.stopPropagation();
      document.activeElement.click();
    }
  }, true);

  window.addEventListener("blur", closeMenu);
  window.addEventListener("resize", closeMenu);
  window.addEventListener("scroll", closeMenu, true);

  function failure(reason, id) {
    var payload = { type: "error", reason: reason };
    if (id) payload.id = id;
    post(payload);
  }

  function extension(mime, kind) {
    var type = (mime || "").toLowerCase().split(";")[0];
    var known = {
      "image/jpeg": ".jpg",
      "image/png": ".png",
      "image/gif": ".gif",
      "image/webp": ".webp",
      "image/bmp": ".bmp",
      "image/avif": ".avif",
      "video/mp4": ".mp4",
      "video/webm": ".webm",
      "video/quicktime": ".mov",
      "video/3gpp": ".3gp",
      "audio/ogg": ".ogg",
      "audio/mpeg": ".mp3",
      "audio/mp4": ".m4a",
      "audio/wav": ".wav",
      "audio/webm": ".weba",
      "application/pdf": ".pdf"
    };
    if (known[type]) return known[type];
    if (kind === "image") return ".jpg";
    if (kind === "video") return ".mp4";
    if (kind === "audio") return ".ogg";
    return "";
  }

  function timestamp() {
    var now = new Date();
    function pad(value) { return (value < 10 ? "0" : "") + value; }
    return "" + now.getFullYear() + pad(now.getMonth() + 1) + pad(now.getDate()) + "-" + pad(now.getHours()) + pad(now.getMinutes()) + pad(now.getSeconds());
  }

  function suggestName(target, mime) {
    var name = (target.filename || "").trim();
    if (!name) name = "whatsapp-" + target.kind + "-" + timestamp();
    if (!/\.[a-z0-9]{1,6}$/i.test(name)) name += extension(mime, target.kind);
    return name;
  }

  function newId() {
    if (window.crypto && window.crypto.randomUUID) return window.crypto.randomUUID();
    return "media-" + Date.now() + "-" + Math.random().toString(16).slice(2);
  }

  function base64(bytes) {
    var binary = "";
    for (var offset = 0; offset < bytes.length; offset += 32768) {
      binary += String.fromCharCode.apply(null, bytes.subarray(offset, Math.min(offset + 32768, bytes.length)));
    }
    return btoa(binary);
  }

  function transfer(target, action) {
    var activeId = null;
    fetch(target.url, { credentials: "include" })
      .then(function (response) {
        if (!response.ok) throw new Error("http " + response.status);
        return response.blob();
      })
      .then(function (blob) {
        if (!blob || blob.size <= 0) {
          failure("expired");
          return;
        }
        if (blob.size > MAX_BYTES) {
          failure("too-large");
          return;
        }
        var id = newId();
        activeId = id;
        var mime = blob.type || "";
        post({
          type: "begin",
          id: id,
          action: action,
          kind: target.kind,
          url: target.url,
          filename: suggestName(target, mime),
          size: blob.size,
          viewOnce: false
        });
        return blob.arrayBuffer().then(function (buffer) {
          var bytes = new Uint8Array(buffer);
          for (var offset = 0; offset < bytes.length; offset += CHUNK_BYTES) {
            post({
              type: "chunk",
              id: id,
              data: base64(bytes.subarray(offset, Math.min(offset + CHUNK_BYTES, bytes.length)))
            });
          }
          post({ type: "end", id: id });
        });
      })
      .catch(function () { failure("expired", activeId); });
  }

  window.__oxidezapMediaActions = {
    setEnabled: function (value) {
      window.__oxidezapMediaEnabled = !!value;
      if (!value) closeMenu();
    }
  };
})();
"#;

fn media_script(enabled: bool) -> String {
    MEDIA_SCRIPT.replace(
        MEDIA_ENABLED_PLACEHOLDER,
        if enabled { "true" } else { "false" },
    )
}

fn media_toggle_script(enabled: bool) -> String {
    format!(
        "if (window.__oxidezapMediaActions) window.__oxidezapMediaActions.setEnabled({enabled});"
    )
}

struct MediaTransfer {
    id: String,
    account_id: String,
    action: MediaAction,
    path: Option<PathBuf>,
    total_bytes: u64,
    bytes: Vec<u8>,
    last_progress: Instant,
    recorded: bool,
    active_downloads: Rc<Cell<u32>>,
}

enum MediaChunkOutcome {
    Appended {
        recorded: bool,
        received: u64,
        total: u64,
        notify: bool,
    },
    Rejected(MediaRejection),
    Ignored,
}

fn apply_media_chunk(
    transfers: &mut HashMap<String, MediaTransfer>,
    account_id: &str,
    id: &str,
    data: &str,
) -> MediaChunkOutcome {
    let Some(transfer) = transfers.get_mut(id) else {
        return MediaChunkOutcome::Ignored;
    };
    if transfer.account_id != account_id {
        return MediaChunkOutcome::Ignored;
    }
    let received = transfer.bytes.len() as u64;
    match decode_media_chunk(data, received, transfer.total_bytes) {
        Ok(bytes) => {
            transfer.bytes.extend_from_slice(&bytes);
            let notify = transfer.last_progress.elapsed() >= DOWNLOAD_PROGRESS_INTERVAL;
            if notify {
                transfer.last_progress = Instant::now();
            }
            MediaChunkOutcome::Appended {
                recorded: transfer.recorded,
                received: transfer.bytes.len() as u64,
                total: transfer.total_bytes,
                notify,
            }
        }
        Err(rejection) => MediaChunkOutcome::Rejected(rejection),
    }
}

fn handle_media_message(
    message: &str,
    account_id: &str,
    events: &Sender<GtkEvent>,
    layout: &Rc<RefCell<DownloadLayout>>,
    transfers: &Rc<RefCell<HashMap<String, MediaTransfer>>>,
    active_downloads: &Rc<Cell<u32>>,
) {
    let Some(message) = parse_media_message(message) else {
        debug!(account_id = %account_id, "ignored an invalid media message");
        return;
    };
    match message {
        MediaMessage::Begin { id, request } => {
            begin_media_transfer(
                id,
                request,
                account_id,
                events,
                layout,
                transfers,
                active_downloads,
            );
        }
        MediaMessage::Chunk { id, data } => {
            let outcome = apply_media_chunk(&mut transfers.borrow_mut(), account_id, &id, &data);
            match outcome {
                MediaChunkOutcome::Appended {
                    recorded,
                    received,
                    total,
                    notify,
                } => {
                    if recorded && notify {
                        let _ = events.send(GtkEvent::DownloadProgress {
                            id,
                            received,
                            total: Some(total),
                        });
                    }
                }
                MediaChunkOutcome::Rejected(rejection) => {
                    abort_media_transfer(&id, rejection, transfers, events);
                }
                MediaChunkOutcome::Ignored => {}
            }
        }
        MediaMessage::End { id } => finish_media_transfer(&id, transfers, events),
        MediaMessage::Failed { id, reason } => {
            let aborted = id.is_some_and(|id| {
                abort_media_transfer(
                    &id,
                    match reason {
                        MediaFailureReason::TooLarge => MediaRejection::TooLarge,
                        _ => MediaRejection::Empty,
                    },
                    transfers,
                    events,
                )
            });
            if !aborted {
                let _ = events.send(GtkEvent::MediaFailed {
                    account_id: account_id.to_string(),
                    message: reason.message().to_string(),
                });
            }
        }
    }
}

fn begin_media_transfer(
    id: String,
    request: MediaRequest,
    account_id: &str,
    events: &Sender<GtkEvent>,
    layout: &Rc<RefCell<DownloadLayout>>,
    transfers: &Rc<RefCell<HashMap<String, MediaTransfer>>>,
    active_downloads: &Rc<Cell<u32>>,
) {
    if let Err(rejection) = validate_media_request(&request) {
        let _ = events.send(GtkEvent::MediaFailed {
            account_id: account_id.to_string(),
            message: rejection.message().to_string(),
        });
        return;
    }
    if transfers.borrow().contains_key(&id) {
        return;
    }

    let mut path = None;
    let mut filename = sanitize_filename(&request.filename);
    if request.action != MediaAction::Copy {
        match layout
            .borrow()
            .reserve(account_id, &request.filename, SystemTime::now())
        {
            Ok(reserved) => {
                if let Some(name) = reserved.file_name() {
                    filename = name.to_string_lossy().into_owned();
                }
                path = Some(reserved);
            }
            Err(error) => {
                warn!(account_id = %account_id, %error, "could not reserve a media download path");
                let _ = events.send(GtkEvent::MediaFailed {
                    account_id: account_id.to_string(),
                    message: format!("Could not reserve a download path: {error}"),
                });
                return;
            }
        }
    }

    let recorded = path.is_some();
    if let Some(path) = &path {
        let _ = events.send(GtkEvent::DownloadStarted {
            id: id.clone(),
            account_id: account_id.to_string(),
            filename,
            path: path.clone(),
            source_url: Some(request.url.clone()),
            total_bytes: Some(request.total_bytes),
        });
    }

    active_downloads.set(active_downloads.get().saturating_add(1));
    transfers.borrow_mut().insert(
        id.clone(),
        MediaTransfer {
            id,
            account_id: account_id.to_string(),
            action: request.action,
            path,
            total_bytes: request.total_bytes,
            bytes: Vec::with_capacity(request.total_bytes as usize),
            last_progress: Instant::now(),
            recorded,
            active_downloads: active_downloads.clone(),
        },
    );
}

fn abort_media_transfer(
    id: &str,
    rejection: MediaRejection,
    transfers: &Rc<RefCell<HashMap<String, MediaTransfer>>>,
    events: &Sender<GtkEvent>,
) -> bool {
    let Some(transfer) = transfers.borrow_mut().remove(id) else {
        return false;
    };
    transfer
        .active_downloads
        .set(transfer.active_downloads.get().saturating_sub(1));
    if let Some(path) = &transfer.path {
        discard_empty_reservation(path);
    }
    warn!(
        account_id = %transfer.account_id,
        reason = %rejection.message(),
        "aborted a media transfer"
    );
    if transfer.recorded {
        let _ = events.send(GtkEvent::DownloadFinished {
            id: transfer.id.clone(),
            success: false,
            cancelled: false,
        });
    } else {
        let _ = events.send(GtkEvent::MediaFailed {
            account_id: transfer.account_id.clone(),
            message: rejection.message().to_string(),
        });
    }
    true
}

fn finish_media_transfer(
    id: &str,
    transfers: &Rc<RefCell<HashMap<String, MediaTransfer>>>,
    events: &Sender<GtkEvent>,
) {
    let Some(mut transfer) = transfers.borrow_mut().remove(id) else {
        return;
    };
    transfer
        .active_downloads
        .set(transfer.active_downloads.get().saturating_sub(1));

    if transfer.bytes.len() as u64 != transfer.total_bytes {
        if let Some(path) = &transfer.path {
            discard_empty_reservation(path);
        }
        warn!(
            account_id = %transfer.account_id,
            expected = transfer.total_bytes,
            received = transfer.bytes.len(),
            "media transfer ended with an unexpected size"
        );
        if transfer.recorded {
            let _ = events.send(GtkEvent::DownloadFinished {
                id: transfer.id.clone(),
                success: false,
                cancelled: false,
            });
        } else {
            let _ = events.send(GtkEvent::MediaFailed {
                account_id: transfer.account_id.clone(),
                message: MediaRejection::Empty.message().to_string(),
            });
        }
        return;
    }

    if transfer.action == MediaAction::Copy {
        match copy_media_image(&transfer.bytes) {
            Ok(()) => {
                let _ = events.send(GtkEvent::MediaCopied {
                    account_id: transfer.account_id.clone(),
                });
            }
            Err(error) => {
                warn!(account_id = %transfer.account_id, %error, "could not copy an image");
                let _ = events.send(GtkEvent::MediaFailed {
                    account_id: transfer.account_id.clone(),
                    message: format!("Could not copy the image: {error}"),
                });
            }
        }
        return;
    }

    let Some(path) = transfer.path.take() else {
        return;
    };
    let bytes = std::mem::take(&mut transfer.bytes);
    let open_after = transfer.action == MediaAction::Open;
    let draw_id = transfer.id.clone();
    let draw_account = transfer.account_id.clone();
    let draw_events = events.clone();
    let cleanup_path = path.clone();
    let spawned = std::thread::Builder::new()
        .name("oxidezap-media-write".to_string())
        .spawn(move || {
            let result = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .and_then(|mut file| file.write_all(&bytes));
            match result {
                Ok(()) => {
                    let open_result = open_after.then(|| open::that(&path));
                    if let Some(Err(error)) = open_result {
                        let _ = draw_events.send(GtkEvent::MediaFailed {
                            account_id: draw_account.clone(),
                            message: format!(
                                "Saved to {}, but the system viewer could not open it: {error}",
                                path.display()
                            ),
                        });
                    }
                    let _ = draw_events.send(GtkEvent::DownloadFinished {
                        id: draw_id,
                        success: true,
                        cancelled: false,
                    });
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&path);
                    let _ = draw_events.send(GtkEvent::DownloadFinished {
                        id: draw_id,
                        success: false,
                        cancelled: false,
                    });
                    let _ = draw_events.send(GtkEvent::MediaFailed {
                        account_id: draw_account,
                        message: format!("Could not save the media: {error}"),
                    });
                }
            }
        });
    if let Err(error) = spawned {
        let _ = std::fs::remove_file(&cleanup_path);
        let _ = events.send(GtkEvent::DownloadFinished {
            id: transfer.id.clone(),
            success: false,
            cancelled: false,
        });
        let _ = events.send(GtkEvent::MediaFailed {
            account_id: transfer.account_id.clone(),
            message: format!("Could not start the media writer: {error}"),
        });
    }
}

fn copy_media_image(bytes: &[u8]) -> Result<()> {
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream(&stream, None::<&gtk::gio::Cancellable>)
        .map_err(|error| anyhow!("could not decode the image: {error}"))?;
    let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
    clipboard.set_image(&pixbuf);
    Ok(())
}

fn prune_media_transfers(state: &mut GtkState, now: Instant) {
    let stale: Vec<String> = {
        let transfers = state.media_transfers.borrow();
        transfers
            .iter()
            .filter(|(_, transfer)| {
                now.duration_since(transfer.last_progress) > MEDIA_TRANSFER_TIMEOUT
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    for id in stale {
        let Some(transfer) = state.media_transfers.borrow_mut().remove(&id) else {
            continue;
        };
        transfer
            .active_downloads
            .set(transfer.active_downloads.get().saturating_sub(1));
        if let Some(path) = &transfer.path {
            discard_empty_reservation(path);
        }
        warn!(
            account_id = %transfer.account_id,
            "media transfer timed out"
        );
        if transfer.recorded {
            let _ = state.events.send(GtkEvent::DownloadFinished {
                id: transfer.id.clone(),
                success: false,
                cancelled: false,
            });
        } else {
            let _ = state.events.send(GtkEvent::MediaFailed {
                account_id: transfer.account_id.clone(),
                message: "The media transfer timed out. Reopen the media and try again."
                    .to_string(),
            });
        }
    }
}

fn connect_media_script(
    manager: &UserContentManager,
    account_id: &str,
    events: Sender<GtkEvent>,
    layout: Rc<RefCell<DownloadLayout>>,
    transfers: Rc<RefCell<HashMap<String, MediaTransfer>>>,
    active_downloads: Rc<Cell<u32>>,
    enabled: bool,
) {
    if !manager.register_script_message_handler(MEDIA_MESSAGE_HANDLER) {
        warn!(account_id = %account_id, "could not register the media message handler");
        return;
    }
    manager.add_script(&UserScript::new(
        &media_script(enabled),
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));

    let script_account = account_id.to_string();
    manager.connect_script_message_received(
        Some(MEDIA_MESSAGE_HANDLER),
        move |_manager, result| {
            let Some(value) = result.js_value() else {
                return;
            };
            let message = value.to_string();
            handle_media_message(
                &message,
                &script_account,
                &events,
                &layout,
                &transfers,
                &active_downloads,
            );
        },
    );
}

/// Applies the proxy configuration to a per-account WebKit data manager.
///
/// WebKitGTK 2.0.2 exposes this on `WebsiteDataManager` since 2.32
/// (`webkit_website_data_manager_set_network_proxy_settings`); the older
/// `WebContext` method is deprecated. A custom proxy URI is only passed for
/// manual mode, and the URI may contain credentials, so only the redacted
/// summary is ever logged.
fn apply_proxy(manager: &webkit2gtk::WebsiteDataManager, proxy: &ProxySettings) {
    match proxy.uri() {
        Ok(Some(uri)) => {
            let mut settings = NetworkProxySettings::new(Some(&uri), &[]);
            manager.set_network_proxy_settings(NetworkProxyMode::Custom, Some(&mut settings));
            info!(proxy = %proxy.redacted_summary(), "applied the network proxy");
        }
        Ok(None) => {
            let mode = match proxy.mode {
                ProxyMode::System | ProxyMode::Manual => NetworkProxyMode::Default,
                ProxyMode::NoProxy => NetworkProxyMode::NoProxy,
            };
            manager.set_network_proxy_settings(mode, None);
            info!(proxy = %proxy.redacted_summary(), "applied the network proxy");
        }
        Err(error) => {
            warn!(%error, "the manual proxy configuration is invalid; using the system proxy");
            manager.set_network_proxy_settings(NetworkProxyMode::Default, None);
        }
    }
}

/// Applies the composer writing preferences to a WebKit context.
///
/// WebKitGTK 2.0.2 exposes spell checking on `WebContext`, not on `Settings`:
/// `set_spell_checking_enabled` and `set_spell_checking_languages`. The
/// language list is kept globally in the UI process, so the same call reaches
/// every open account while a new context also receives it before its first
/// page load.
fn apply_spell_checking(context: &webkit2gtk::WebContext, writing: &WritingSettings) {
    let enabled = writing.spell_checking_enabled();
    context.set_spell_checking_enabled(enabled);
    if !enabled {
        return;
    }
    let locales: Vec<String> = glib::language_names()
        .iter()
        .map(|locale| locale.to_string())
        .collect();
    let languages = writing.spell_checking_languages(&locales);
    let languages: Vec<&str> = languages.iter().map(String::as_str).collect();
    context.set_spell_checking_languages(&languages);
}

fn connect_webview_signals(
    webview: &webkit2gtk::WebView,
    account_id: &str,
    events: Sender<GtkEvent>,
    notifications_enabled: bool,
    parent_window: gtk::Window,
    media_enabled: Rc<Cell<bool>>,
    connection_poll: Rc<Cell<Duration>>,
) {
    let load_account = account_id.to_string();
    let load_events = events.clone();
    let load_media = media_enabled;
    webview.connect_load_changed(move |webview, event| {
        let state = match event {
            webkit2gtk::LoadEvent::Started => LoadState::Started,
            webkit2gtk::LoadEvent::Redirected => LoadState::Redirected,
            webkit2gtk::LoadEvent::Committed => LoadState::Committed,
            webkit2gtk::LoadEvent::Finished => LoadState::Finished,
            _ => return,
        };
        if state == LoadState::Finished {
            let script = media_toggle_script(load_media.get());
            webview.evaluate_javascript(
                &script,
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                |_result| {},
            );
            // A reload creates a fresh window object. Restore the current
            // policy so battery saver survives WhatsApp's own reloads.
            let poll = connection_poll_script(connection_poll.get());
            webview.evaluate_javascript(
                &poll,
                None,
                None,
                None::<&gtk::gio::Cancellable>,
                |_result| {},
            );
        }
        let _ = load_events.send(GtkEvent::LoadState {
            account_id: load_account.clone(),
            state,
        });
    });

    let failure_account = account_id.to_string();
    let failure_events = events.clone();
    webview.connect_load_failed(move |_webview, _event, uri, error| {
        // Ignoring a navigation in `decide-policy` and replacing an in-flight
        // load are not connection failures.
        if error.matches(NetworkError::Cancelled)
            || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
        {
            return false;
        }
        let uri = uri.to_string();
        let message = error.message().to_string();
        warn!(
            account_id = %failure_account,
            %uri,
            %message,
            "WhatsApp Web load failed"
        );
        let _ = failure_events.send(GtkEvent::LoadFailed {
            account_id: failure_account.clone(),
            uri,
            message,
        });
        false
    });

    let title_account = account_id.to_string();
    let title_events = events.clone();
    webview.connect_title_notify(move |webview| {
        let title = webview
            .title()
            .map(|title| title.to_string())
            .unwrap_or_default();
        let unread = parse_unread(&title);
        let _ = title_events.send(GtkEvent::Title {
            account_id: title_account.clone(),
            title,
            unread,
        });
    });

    let notification_account = account_id.to_string();
    let notification_events = events.clone();
    webview.connect_show_notification(move |_webview, notification| {
        let title = notification
            .title()
            .map(|title| title.to_string())
            .unwrap_or_else(|| "WhatsApp".to_string());
        let body = notification
            .body()
            .map(|body| body.to_string())
            .unwrap_or_default();
        let _ = notification_events.send(GtkEvent::Notification {
            account_id: notification_account.clone(),
            title,
            body,
        });
        true
    });

    let camera_account = account_id.to_string();
    let camera_events = events.clone();
    webview.connect_camera_capture_state_notify(move |webview| {
        report_capture_state(webview, &camera_account, &camera_events);
    });
    let microphone_account = account_id.to_string();
    let microphone_events = events.clone();
    webview.connect_microphone_capture_state_notify(move |webview| {
        report_capture_state(webview, &microphone_account, &microphone_events);
    });
    let display_account = account_id.to_string();
    let display_events = events.clone();
    webview.connect_display_capture_state_notify(move |webview| {
        report_capture_state(webview, &display_account, &display_events);
    });

    let policy_account = account_id.to_string();
    let policy_events = events.clone();
    webview.connect_decide_policy(move |webview, decision, decision_type| {
        if decision_type != PolicyDecisionType::NavigationAction {
            return false;
        }
        let Some(navigation) = decision.downcast_ref::<NavigationPolicyDecision>() else {
            return false;
        };
        let Some(action) = navigation.navigation_action() else {
            return false;
        };
        let Some(request) = action.request() else {
            return false;
        };
        let Some(uri) = request.uri().map(|uri| uri.to_string()) else {
            return false;
        };
        if links::is_internal_url(&uri) {
            debug!(%uri, "allowed an internal WhatsApp navigation");
            decision.use_();
            return true;
        }
        match links::classify(&uri) {
            LinkAction::Allow => {
                decision.use_();
                true
            }
            LinkAction::External(url) => {
                decision.ignore();
                let _ = policy_events.send(GtkEvent::OpenExternal { url });
                true
            }
            LinkAction::WhatsApp(url) => {
                decision.ignore();
                let _ = policy_events.send(GtkEvent::WhatsAppUrl {
                    account_id: policy_account.clone(),
                    url: url.clone(),
                });
                webview.load_uri(&url);
                true
            }
        }
    });

    let create_account = account_id.to_string();
    let create_events = events.clone();
    webview.connect_create(move |webview, action| {
        let request = action.request()?;
        let uri = request.uri().map(|uri| uri.to_string())?;
        if links::is_internal_url(&uri) {
            debug!(%uri, "ignored an internal WhatsApp popup");
            return None;
        }
        match links::classify(&uri) {
            LinkAction::Allow => {
                webview.load_uri(&uri);
            }
            LinkAction::External(url) => {
                let _ = create_events.send(GtkEvent::OpenExternal { url });
            }
            LinkAction::WhatsApp(url) => {
                let _ = create_events.send(GtkEvent::WhatsAppUrl {
                    account_id: create_account.clone(),
                    url: url.clone(),
                });
                webview.load_uri(&url);
            }
        }
        None
    });

    let permission_window = parent_window;
    webview.connect_permission_request(move |webview, request| {
        let origin = webview.uri().map(|uri| uri.to_string());
        let kind = if request
            .dynamic_cast_ref::<UserMediaPermissionRequest>()
            .is_some()
        {
            PermissionKind::UserMedia
        } else if request
            .dynamic_cast_ref::<NotificationPermissionRequest>()
            .is_some()
        {
            PermissionKind::Notifications
        } else {
            PermissionKind::Other
        };
        let host = origin.as_deref().and_then(origin_host);

        match permission_action(origin.as_deref(), kind, notifications_enabled) {
            PermissionAction::Allow => {
                request.allow();
                true
            }
            PermissionAction::Deny => {
                debug!(
                    origin = host.as_deref().unwrap_or("unknown"),
                    ?kind,
                    "denied a WebKit permission request"
                );
                request.deny();
                true
            }
            PermissionAction::Prompt => {
                let Some(media) = request.dynamic_cast_ref::<UserMediaPermissionRequest>() else {
                    request.deny();
                    return true;
                };
                prompt_user_media_permission(
                    &permission_window,
                    request,
                    media,
                    host.as_deref().unwrap_or("WhatsApp Web"),
                );
                true
            }
        }
    });

    let crash_account = account_id.to_string();
    let crash_events = events;
    webview.connect_web_process_terminated(move |_webview, reason| {
        // Destroying the view (normal close and app quit) also terminates the
        // web process. Only crashes and memory-limit kills need recovery.
        if reason == WebProcessTerminationReason::TerminatedByApi {
            return;
        }
        let _ = crash_events.send(GtkEvent::Crashed {
            account_id: crash_account.clone(),
        });
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionKind {
    UserMedia,
    Notifications,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionAction {
    Allow,
    Deny,
    Prompt,
}

/// Pure permission policy so the security rules can be tested without GTK.
/// Only WhatsApp pages may request anything, and user media always needs an
/// explicit native confirmation.
fn permission_action(
    origin: Option<&str>,
    kind: PermissionKind,
    notifications_enabled: bool,
) -> PermissionAction {
    if !is_whatsapp_origin(origin) {
        return PermissionAction::Deny;
    }
    match kind {
        PermissionKind::UserMedia => PermissionAction::Prompt,
        PermissionKind::Notifications if notifications_enabled => PermissionAction::Allow,
        PermissionKind::Notifications | PermissionKind::Other => PermissionAction::Deny,
    }
}

fn is_whatsapp_origin(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return false;
    };
    let Ok(url) = url::Url::parse(origin.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    links::is_whatsapp_host(host) || links::is_internal_url(origin)
}

fn origin_host(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri.trim()).ok()?;
    url.host_str().map(|host| host.to_string())
}

/// Ask the user before granting camera or microphone access to WhatsApp. The
/// request stays pending until the user answers; denying it is the default.
fn prompt_user_media_permission(
    parent: &gtk::Window,
    request: &PermissionRequest,
    media: &UserMediaPermissionRequest,
    origin: &str,
) {
    let (camera, microphone) = (media.is_for_video_device(), media.is_for_audio_device());
    let device = match (camera, microphone) {
        (true, true) => "camera and microphone",
        (true, false) => "camera",
        (false, true) => "microphone",
        (false, false) => "media devices",
    };

    let dialog = gtk::MessageDialog::builder()
        .message_type(gtk::MessageType::Question)
        .buttons(gtk::ButtonsType::None)
        .text(format!("Allow WhatsApp to use your {device}?"))
        .secondary_text(format!(
            "The request comes from {origin}. Deny it if you did not start a call or record a voice note."
        ))
        .build();
    dialog.set_title("OxideZap permission request");
    dialog.set_transient_for(Some(parent));
    dialog.set_modal(true);
    dialog.add_button("Deny", gtk::ResponseType::No);
    dialog.add_button("Allow", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::No);

    let pending = request.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Yes {
            info!("user allowed a WhatsApp media permission request");
            pending.allow();
        } else {
            info!("user denied a WhatsApp media permission request");
            pending.deny();
        }
        dialog.close();
    });
    dialog.show_all();
}

fn connect_downloads(
    context: &webkit2gtk::WebContext,
    account_id: &str,
    events: Sender<GtkEvent>,
    layout: Rc<RefCell<DownloadLayout>>,
    downloads: Rc<RefCell<HashMap<String, webkit2gtk::Download>>>,
    cancelled_downloads: Rc<RefCell<HashSet<String>>>,
    active_downloads: Rc<Cell<u32>>,
) {
    let download_account = account_id.to_string();
    context.connect_download_started(move |_context, download| {
        let account_id = download_account.clone();
        let events = events.clone();
        let layout = layout.clone();
        let downloads = downloads.clone();
        let cancelled_downloads = cancelled_downloads.clone();
        active_downloads.set(active_downloads.get().saturating_add(1));
        let released = Rc::new(Cell::new(false));
        // One id identifies the download across every event. The GTK object is
        // retained as soon as it is announced, so a `download_uri` retry whose
        // return value is dropped cannot cancel itself.
        let id = Uuid::new_v4().to_string();
        downloads.borrow_mut().insert(id.clone(), download.clone());
        let reserved_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let last_progress = Rc::new(RefCell::new(Instant::now()));

        let progress_id = id.clone();
        let progress_events = events.clone();
        download.connect_received_data(move |download, received| {
            {
                let mut last = last_progress.borrow_mut();
                if last.elapsed() < DOWNLOAD_PROGRESS_INTERVAL {
                    return;
                }
                *last = Instant::now();
            }
            let total = download
                .response()
                .map(|response| response.content_length())
                .filter(|length| *length > 0);
            let _ = progress_events.send(GtkEvent::DownloadProgress {
                id: progress_id.clone(),
                received,
                total,
            });
        });

        let finished_id = id.clone();
        let finished_events = events.clone();
        let finished_downloads = downloads.clone();
        let finished_cancelled = cancelled_downloads.clone();
        let finished_active = active_downloads.clone();
        let finished_released = released.clone();
        download.connect_finished(move |download| {
            if !finished_released.replace(true) {
                finished_active.set(finished_active.get().saturating_sub(1));
            }
            let received = download.received_data_length();
            let total = download
                .response()
                .map(|response| response.content_length())
                .filter(|length| *length > 0);
            let _ = finished_events.send(GtkEvent::DownloadProgress {
                id: finished_id.clone(),
                received,
                total,
            });
            finished_downloads.borrow_mut().remove(&finished_id);
            finished_cancelled.borrow_mut().remove(&finished_id);
            let _ = finished_events.send(GtkEvent::DownloadFinished {
                id: finished_id.clone(),
                success: true,
                cancelled: false,
            });
        });

        let failed_id = id.clone();
        let failed_events = events.clone();
        let failed_downloads = downloads.clone();
        let failed_cancelled = cancelled_downloads.clone();
        let failed_path = reserved_path.clone();
        let failed_active = active_downloads.clone();
        let failed_released = released.clone();
        download.connect_failed(move |_download, error| {
            if !failed_released.replace(true) {
                failed_active.set(failed_active.get().saturating_sub(1));
            }
            let cancelled = failed_cancelled.borrow_mut().remove(&failed_id);
            failed_downloads.borrow_mut().remove(&failed_id);
            if let Some(path) = failed_path.borrow_mut().take() {
                discard_empty_reservation(&path);
            }
            if !cancelled {
                warn!(%error, "download failed");
            }
            let _ = failed_events.send(GtkEvent::DownloadFinished {
                id: failed_id.clone(),
                success: false,
                cancelled,
            });
        });

        let decide_id = id.clone();
        let decide_events = events.clone();
        let decide_downloads = downloads.clone();
        let decide_layout = layout.clone();
        download.connect_decide_destination(move |download, suggested| {
            let path =
                match decide_layout
                    .borrow()
                    .reserve(&account_id, suggested, SystemTime::now())
                {
                    Ok(path) => path,
                    Err(error) => {
                        warn!(%error, %suggested, "could not reserve a download destination");
                        decide_downloads.borrow_mut().remove(&decide_id);
                        let _ = decide_events.send(GtkEvent::DownloadRejected {
                            account_id: account_id.clone(),
                            filename: sanitize_filename(suggested),
                            reason: error.to_string(),
                        });
                        return false;
                    }
                };
            *reserved_path.borrow_mut() = Some(path.clone());
            let filename = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| sanitize_filename(suggested));
            let source_url = download
                .request()
                .and_then(|request| request.uri())
                .map(|uri| uri.to_string());
            let total_bytes = download
                .response()
                .map(|response| response.content_length())
                .filter(|length| *length > 0);

            // The destination already exists as our empty reservation, so
            // WebKit has to be allowed to truncate it instead of refusing it.
            download.set_allow_overwrite(true);
            download.set_destination(path.to_string_lossy().as_ref());
            let _ = decide_events.send(GtkEvent::DownloadStarted {
                id: decide_id.clone(),
                account_id: account_id.clone(),
                filename,
                path,
                source_url,
                total_bytes,
            });
            true
        });
    });
}

/// Positions every visible overlay over its cached GPUI parent window.
///
/// The parent XID is remembered on the entry when the overlay is created, so
/// this 100 ms pass only reads the geometry of a known window. The account tag
/// is consulted once as a fallback when the cached XID became invalid, which
/// covers a window that was recreated without a new `Create` command.
fn sync_overlays(state: &mut GtkState) {
    prune_media_transfers(state, Instant::now());
    let x11 = &state.x11;
    for (account_id, entry) in state.entries.iter_mut() {
        if !entry.visible {
            entry.hide();
            continue;
        }
        if entry.composition_xid.is_some() {
            if !entry.window.is_visible() {
                entry.window.show_all();
            }
            continue;
        }
        let Some(parent) = resolve_parent_xid(x11, account_id, entry) else {
            entry.hide();
            continue;
        };
        let Ok(geometry) = x11.geometry(parent) else {
            entry.hide();
            continue;
        };
        let Ok((root_x, root_y)) = x11.root_position(parent) else {
            entry.hide();
            continue;
        };
        let Some(target) = overlay_target(&geometry, root_x, root_y, entry.scale) else {
            entry.hide();
            continue;
        };

        if entry.transient_for != Some(parent) {
            if let Err(error) = x11.set_transient(entry.xid, parent) {
                warn!(%error, "could not make the WebKit window transient");
            }
            entry.transient_for = Some(parent);
            entry.last_geometry = None;
        }

        if entry.last_geometry != Some(target) {
            if let Err(error) = x11.configure(entry.xid, target.0, target.1, target.2, target.3) {
                debug!(%error, "could not position the WebKit overlay");
            }
            entry.last_geometry = Some(target);
        }

        if !entry.window.is_visible() {
            entry.window.show_all();
            let _ = x11.raise(entry.xid);
        }
    }
}

/// Returns the cached GPUI parent XID when the window is still viewable. A
/// missing or invalid XID falls back to the account tag, which also refreshes
/// the cache explicitly for the next synchronization tick.
fn resolve_parent_xid(x11: &X11, account_id: &str, entry: &mut WebEntry) -> Option<u32> {
    if let Some(parent) = entry.parent_xid {
        match x11.is_viewable(parent) {
            Ok(true) => return Some(parent),
            Ok(false) => return None,
            Err(error) => {
                debug!(%error, "cached GPUI parent XID is stale; falling back to the account tag");
                entry.forget_parent_xid();
            }
        }
    }
    let parent = x11.find_account_window(account_id).ok().flatten()?;
    entry.set_parent_xid(parent);
    if !x11.is_viewable(parent).unwrap_or(false) {
        return None;
    }
    Some(parent)
}

/// Computes the overlay rectangle for a parent window, or `None` when the
/// window is too small to host the web content.
fn overlay_target(
    geometry: &Geometry,
    root_x: i32,
    root_y: i32,
    scale: f32,
) -> Option<(i32, i32, u32, u32)> {
    let titlebar = (TITLEBAR_HEIGHT * scale).round().max(1.0) as u32;
    let margin = (OVERLAY_MARGIN * scale).round().max(1.0) as u32;
    let width = u32::from(geometry.width).saturating_sub(margin * 2);
    let height = u32::from(geometry.height).saturating_sub(titlebar + margin);
    if width < 20 || height < 20 {
        return None;
    }
    Some((
        root_x + margin as i32,
        root_y + titlebar as i32,
        width,
        height,
    ))
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    if provider
        .load_from_data(
            b"window { background-color: #0b141a; }\
              #oxidezap-drop-hint { background-color: rgba(11, 20, 26, 0.88);\
              border: 2px dashed #25d366; border-radius: 14px; padding: 18px; }",
        )
        .is_err()
    {
        return;
    }
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

const PASTE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const PASTE_CHUNK_BYTES: usize = 3 * 1024 * 1024;

/// One validated local file ready for the attachment pipeline. The dialog and
/// notifications use `name`; the full path never reaches a log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareFileInfo {
    pub path: PathBuf,
    pub name: String,
    pub mime: String,
    pub image: bool,
    pub video: bool,
    pub bytes: u64,
}

/// Validates local files before they are attached. Only regular, non-empty
/// files under the paste limit are accepted; the batch shares the same 64 MB
/// budget as clipboard pastes so the page never receives an unbounded payload.
pub fn validate_share_paths(paths: &[PathBuf]) -> Result<Vec<ShareFileInfo>, String> {
    if paths.is_empty() {
        return Err("No files to attach.".to_string());
    }
    let mut total = 0u64;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("file")
            .to_string();
        let metadata = std::fs::metadata(path).map_err(|_| format!("Could not read {name}."))?;
        if !metadata.is_file() {
            return Err(format!("{name} is not a regular file."));
        }
        total = total.saturating_add(metadata.len());
        if metadata.len() > PASTE_MAX_BYTES || total > PASTE_MAX_BYTES {
            return Err(format!(
                "{name} exceeds the 64 MB attachment limit for files."
            ));
        }
        if metadata.len() == 0 {
            return Err(format!("{name} is empty."));
        }
        let mime = mime_for_path(path);
        files.push(ShareFileInfo {
            path: path.clone(),
            name,
            image: mime.starts_with("image/"),
            video: mime.starts_with("video/"),
            mime,
            bytes: metadata.len(),
        });
    }
    Ok(files)
}

/// Extracts local file paths from an XDND/`text/uri-list` payload. Comments,
/// blank lines, non-file schemes, and malformed URIs are ignored.
pub fn parse_file_uri_list(list: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in list.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(url) = url::Url::parse(line) else {
            continue;
        };
        if url.scheme() != "file" {
            continue;
        }
        if let Ok(path) = url.to_file_path() {
            paths.push(path);
        }
    }
    paths
}

/// Builds the WebKit overlay and the hidden "drop to attach" hint.
fn build_drop_overlay(webview: &webkit2gtk::WebView) -> (gtk::Overlay, gtk::Box) {
    let overlay = gtk::Overlay::new();
    overlay.add(webview);

    let hint = gtk::Box::new(gtk::Orientation::Vertical, 6);
    hint.set_widget_name("oxidezap-drop-hint");
    hint.set_halign(gtk::Align::Center);
    hint.set_valign(gtk::Align::Center);
    hint.set_size_request(360, 120);
    let title = gtk::Label::new(None);
    title.set_markup("<span size=\"x-large\" weight=\"bold\">Drop to attach</span>");
    let subtitle = gtk::Label::new(Some(
        "Images, videos, PDFs, and files up to 64 MB stay in the preview until you send.",
    ));
    subtitle.set_line_wrap(true);
    subtitle.set_justify(gtk::Justification::Center);
    hint.add(&title);
    hint.add(&subtitle);
    hint.show_all();
    hint.set_no_show_all(true);
    hint.hide();
    overlay.add_overlay(&hint);
    overlay.set_overlay_pass_through(&hint, true);
    (overlay, hint)
}

/// Registers the WebKit view as a local-file drop target. Only `text/uri-list`
/// entries with a `file://` URI are considered; remote URLs are ignored.
fn setup_drop_target(
    webview: &webkit2gtk::WebView,
    account_id: &str,
    paste: Rc<RefCell<PasteQueue>>,
    drop_hint: gtk::Box,
    events: Sender<GtkEvent>,
) {
    let targets = [gtk::TargetEntry::new(
        "text/uri-list",
        gtk::TargetFlags::OTHER_APP,
        0,
    )];
    webview.drag_dest_set(gtk::DestDefaults::ALL, &targets, gtk::gdk::DragAction::COPY);
    webview.drag_dest_set_track_motion(true);

    let motion_hint = drop_hint.clone();
    webview.connect_drag_motion(move |_webview, context, _x, _y, time| {
        context.drag_status(gtk::gdk::DragAction::COPY, time);
        motion_hint.show();
        true
    });
    let leave_hint = drop_hint.clone();
    webview.connect_drag_leave(move |_webview, _context, _time| {
        leave_hint.hide();
    });

    let drop_account = account_id.to_string();
    let drop_hint = drop_hint;
    let drop_paste = paste;
    let drop_events = events;
    webview.connect_drag_drop(move |webview, context, _x, _y, time| {
        let target = gtk::gdk::Atom::intern("text/uri-list");
        if context.list_targets().contains(&target) {
            webview.drag_get_data(context, &target, time);
            true
        } else {
            false
        }
    });
    webview.connect_drag_data_received(move |_webview, context, _x, _y, data, _info, time| {
        context.drag_finish(true, false, time);
        drop_hint.hide();
        let text = {
            let bytes = data.data();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let paths = parse_file_uri_list(&text);
        queue_share_paths(&drop_account, &paths, &drop_paste, &drop_events, true);
    });
}

fn queue_share_paths(
    account_id: &str,
    paths: &[PathBuf],
    paste: &Rc<RefCell<PasteQueue>>,
    events: &Sender<GtkEvent>,
    view_ready: bool,
) {
    if !view_ready {
        let _ = events.send(GtkEvent::ShareRejected {
            account_id: account_id.to_string(),
            message: "The account window is not ready. Try again in a moment.".to_string(),
        });
        return;
    }
    let files = match validate_share_paths(paths) {
        Ok(files) => files,
        Err(message) => {
            let _ = events.send(GtkEvent::ShareRejected {
                account_id: account_id.to_string(),
                message,
            });
            return;
        }
    };
    let image = files.iter().all(|file| file.image);
    let count = files.len();
    let files = files
        .into_iter()
        .map(|file| PasteFile {
            name: file.name,
            mime: file.mime,
            source: PasteSource::File(file.path),
        })
        .collect();
    paste.borrow().submit(account_id, files, image);
    let _ = events.send(GtkEvent::ShareAttached {
        account_id: account_id.to_string(),
        count,
    });
}

#[derive(Debug)]
enum PasteSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug)]
struct PasteFile {
    name: String,
    mime: String,
    source: PasteSource,
}

#[derive(Debug)]
struct PasteJob {
    account_id: String,
    image: bool,
    files: Vec<PasteFile>,
}

type PastedFile = (String, String, Vec<String>);
type PasteReady = (String, Vec<PastedFile>);
type ScriptDone = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

#[derive(Debug, PartialEq, Eq)]
enum PasteOutcome {
    Ready(Vec<PastedFile>),
    Empty,
    TooLarge { image: bool },
}

#[derive(Debug)]
struct PasteResult {
    account_id: String,
    outcome: PasteOutcome,
}

#[derive(Debug, Default)]
struct PasteBudget {
    used: u64,
}

impl PasteBudget {
    fn add(&mut self, bytes: u64) -> bool {
        self.used = self.used.saturating_add(bytes);
        self.used <= PASTE_MAX_BYTES
    }
}

/// Reads and encodes pasted files on one worker thread. A single worker keeps
/// jobs ordered, and the GTK side only runs the injection script queue.
struct PasteQueue {
    jobs: Sender<PasteJob>,
    results: Receiver<PasteResult>,
    active: bool,
    stopped: bool,
}

impl PasteQueue {
    fn start() -> Self {
        let (jobs, job_rx) = unbounded::<PasteJob>();
        let (result_tx, results) = unbounded::<PasteResult>();
        let thread = std::thread::Builder::new()
            .name("oxidezap-paste".to_string())
            .spawn(move || {
                for job in job_rx {
                    if result_tx.send(process_paste_job(job)).is_err() {
                        break;
                    }
                }
            });
        if let Err(error) = thread {
            warn!(%error, "could not start the paste worker");
        }
        Self {
            jobs,
            results,
            active: false,
            stopped: false,
        }
    }

    fn submit(&self, account_id: &str, files: Vec<PasteFile>, image: bool) {
        let job = PasteJob {
            account_id: account_id.to_string(),
            image,
            files,
        };
        if let Err(error) = self.jobs.send(job) {
            warn!(%error, "could not queue an attachment");
        }
    }

    fn next_ready(&mut self) -> Option<PasteReady> {
        loop {
            if self.stopped {
                return None;
            }
            let result = match self.results.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.stopped = true;
                    warn!("the paste worker stopped");
                    return None;
                }
            };
            match result.outcome {
                PasteOutcome::Ready(files) => return Some((result.account_id, files)),
                PasteOutcome::Empty => {
                    debug!("clipboard paste produced no readable files");
                }
                PasteOutcome::TooLarge { image } => {
                    report_paste_too_large(image);
                }
            }
        }
    }
}

fn report_paste_too_large(image: bool) {
    if image {
        crate::notify::notify(
            "Image too large",
            "Use the attach button for images over 64 MB.",
            None,
        );
    } else {
        crate::notify::notify(
            "File too large",
            "Use the attach button for files over 64 MB.",
            None,
        );
    }
}

fn process_paste_job(job: PasteJob) -> PasteResult {
    let mut budget = PasteBudget::default();
    let mut files = Vec::new();
    for file in job.files {
        match file.source {
            PasteSource::File(path) => {
                let Ok(metadata) = std::fs::metadata(&path) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                if !budget.add(metadata.len()) {
                    return PasteResult {
                        account_id: job.account_id,
                        outcome: PasteOutcome::TooLarge { image: job.image },
                    };
                }
                match read_file_chunks(&path) {
                    Ok(chunks) if !chunks.is_empty() => {
                        files.push((file.name, file.mime, chunks));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(%error, "could not read a pasted file");
                    }
                }
            }
            PasteSource::Bytes(bytes) => {
                if !budget.add(bytes.len() as u64) {
                    return PasteResult {
                        account_id: job.account_id,
                        outcome: PasteOutcome::TooLarge { image: job.image },
                    };
                }
                files.push((file.name, file.mime, base64_chunks(&bytes)));
            }
        }
    }
    let outcome = if files.is_empty() {
        PasteOutcome::Empty
    } else {
        PasteOutcome::Ready(files)
    };
    PasteResult {
        account_id: job.account_id,
        outcome,
    }
}

fn read_file_chunks(path: &Path) -> std::io::Result<Vec<String>> {
    let mut handle = std::fs::File::open(path)?;
    let mut chunks = Vec::new();
    let mut buffer = vec![0u8; PASTE_CHUNK_BYTES];
    loop {
        match handle.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => chunks.push(glib::base64_encode(&buffer[..read]).to_string()),
            Err(error) => return Err(error),
        }
    }
    Ok(chunks)
}

const PASTE_HELPER: &str = r#"
(function () {
  if (window.__oxidezapPaste) return;

  function decode(b64) {
    if (typeof Uint8Array.fromBase64 === "function") {
      return Uint8Array.fromBase64(b64);
    }
    var bin = atob(b64), n = bin.length, out = new Uint8Array(n);
    for (var i = 0; i < n; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function inputFor(kind) {
    var inputs = document.querySelectorAll('input[type="file"]');
    if (kind === "media") {
      // Only the photos/videos input advertises video. Image-only inputs
      // (notably the sticker input) would turn a pasted screenshot into a
      // sticker, so they are never selected here.
      for (var m = 0; m < inputs.length; m++) {
        var media = inputs[m];
        if (media.isConnected === false) continue;
        if (/video/i.test((media.accept || "").trim())) return media;
      }
      return null;
    }
    for (var i = 0; i < inputs.length; i++) {
      var input = inputs[i];
      if (input.isConnected === false) continue;
      var accept = (input.accept || "").trim();
      if (accept === "" || accept === "*" || accept === "*/*") return input;
      if (!/image/i.test(accept) && !/video/i.test(accept)) return input;
    }
    return null;
  }

  function clickAttach() {
    var selectors = [
      '[data-icon="plus"]',
      '[data-icon="attach-menu-plus"]',
      '[data-icon="clip"]',
      '[data-testid="clip"]',
      '[data-testid*="attach"]',
      'button[title="Attach"]',
      'button[title="Adjuntar"]',
      '[aria-label="Attach"]',
      '[aria-label="Adjuntar"]',
      '[title="Attach"]',
      '[title="Adjuntar"]',
      '[aria-label*="Attach"]',
      '[aria-label*="Adjuntar"]',
      '[title*="Attach"]',
      '[title*="Adjuntar"]'
    ];
    for (var i = 0; i < selectors.length; i++) {
      var button = document.querySelector(selectors[i]);
      if (!button) continue;
      (button.closest('button,[role="button"],div[role="button"]') || button).click();
      return true;
    }
    return false;
  }

  function clickItem(kind) {
    var selectors = kind === "media"
      ? ['[data-testid="attach-image"]', '[data-icon="media-multiple"]', '[aria-label*="Photos"]', '[aria-label*="hoto"]', '[aria-label*="Fotos"]', '[aria-label*="Imagen"]', '[title*="Photos"]', '[title*="Fotos"]']
      : ['[data-testid="attach-document"]', '[data-icon="document"]', '[aria-label*="Document"]', '[aria-label*="ocument"]', '[aria-label*="Documento"]', '[title*="Document"]', '[title*="Documento"]'];
    for (var i = 0; i < selectors.length; i++) {
      var item = document.querySelector(selectors[i]);
      if (item) {
        (item.closest('li,button,[role="button"],div[role="button"]') || item).click();
        return true;
      }
    }
    return false;
  }

  function inject(input, files) {
    try {
      var dt = new DataTransfer();
      for (var i = 0; i < files.length; i++) dt.items.add(files[i]);
      input.value = "";
      input.files = dt.files;
      input.dispatchEvent(new Event("change", { bubbles: true }));
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return true;
    } catch (error) {
      console.warn("OxideZap paste: " + error);
      return false;
    }
  }

  function attach(files) {
    if (!files.length) return;
    var media = files.every(function (file) {
      return /^image\//.test(file.type || "") ||
        file.type === "video/mp4" || file.type === "video/3gpp" || file.type === "video/quicktime";
    });
    var kind = media ? "media" : "doc";
    var existing = inputFor(kind);
    if (existing) {
      inject(existing, files);
      return;
    }

    if (!clickAttach()) {
      var fallback = inputFor("doc");
      if (fallback) {
        inject(fallback, files);
        return;
      }
    }
    var tries = 0;
    function poll() {
      var input = inputFor(kind);
      if (input) {
        inject(input, files);
        return;
      }
      if (tries++ === 2) clickItem(kind);
      if (tries < 40) {
        setTimeout(poll, 75);
        return;
      }
      // A photo/video input is not always mounted; the document input accepts
      // every file and is a safe fallback so the paste is never lost.
      var fallback = inputFor("doc");
      if (fallback) {
        inject(fallback, files);
      } else {
        console.warn("OxideZap paste: attach input not found");
      }
    }
    setTimeout(poll, 75);
  }

  window.__oxidezapPaste = {
    files: [],
    begin: function (metadata) {
      this.files = metadata.map(function (item) {
        return { name: item.name, type: item.type, parts: [] };
      });
    },
    chunk: function (index, b64) {
      if (this.files[index]) this.files[index].parts.push(decode(b64));
    },
    commit: function () {
      var files = this.files.map(function (file) {
        return new File(file.parts, file.name, { type: file.type || "application/octet-stream" });
      });
      this.files = [];
      attach(files);
    }
  };
})();
"#;

fn try_paste_clipboard(
    account_id: &str,
    event: &gtk::gdk::EventKey,
    paste: &Rc<RefCell<PasteQueue>>,
) -> bool {
    let is_paste = event.state().contains(gtk::gdk::ModifierType::CONTROL_MASK)
        && (event.keyval() == gtk::gdk::keys::constants::v
            || event.keyval() == gtk::gdk::keys::constants::V);
    if !is_paste {
        return false;
    }

    let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
    let uris = clipboard.wait_for_uris();
    let mut files = Vec::new();
    for uri in uris {
        let Ok(url) = url::Url::parse(uri.as_str()) else {
            continue;
        };
        if url.scheme() != "file" {
            continue;
        }
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = mime_for_path(&path);
        files.push(PasteFile {
            name,
            mime,
            source: PasteSource::File(path),
        });
    }
    if !files.is_empty() {
        paste.borrow().submit(account_id, files, false);
        return true;
    }

    // Screenshots and images copied by image editors live in the clipboard as
    // image data instead of a file URI. The pixbuf must be read and encoded on
    // the GTK thread; the base64 conversion runs in the paste worker.
    let Some(pixbuf) = clipboard.wait_for_image() else {
        return false;
    };
    let bytes = match pixbuf.save_to_bufferv("png", &[]) {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!(%error, "could not encode the clipboard image");
            return false;
        }
    };
    if bytes.is_empty() {
        return false;
    }
    if bytes.len() as u64 > PASTE_MAX_BYTES {
        report_paste_too_large(true);
        return true;
    }
    paste.borrow().submit(
        account_id,
        vec![PasteFile {
            name: "screenshot.png".to_string(),
            mime: "image/png".to_string(),
            source: PasteSource::Bytes(bytes),
        }],
        true,
    );
    true
}

fn dispatch_paste(state: &Rc<RefCell<GtkState>>, paste: &Rc<RefCell<PasteQueue>>) {
    let Ok(state) = state.try_borrow() else {
        return;
    };
    let Ok(mut queue) = paste.try_borrow_mut() else {
        return;
    };
    if queue.active {
        return;
    }
    let Some((account_id, files)) = queue.next_ready() else {
        return;
    };
    let Some(webview) = state
        .entries
        .get(&account_id)
        .map(|entry| entry.webview.clone())
    else {
        return;
    };
    queue.active = true;
    drop(queue);
    drop(state);

    let done_queue = paste.clone();
    queue_paste_files(webview, files, move || {
        if let Ok(mut queue) = done_queue.try_borrow_mut() {
            queue.active = false;
        }
    });
}

fn base64_chunks(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(PASTE_CHUNK_BYTES)
        .map(|chunk| glib::base64_encode(chunk).to_string())
        .collect()
}

fn paste_begin_script(files: &[PastedFile]) -> String {
    let metadata: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, mime, _)| serde_json::json!({ "name": name, "type": mime }))
        .collect();
    format!(
        "{PASTE_HELPER}\n__oxidezapPaste.begin({});",
        serde_json::to_string(&metadata).unwrap_or_else(|_| "[]".to_string())
    )
}

fn queue_paste_files(
    webview: webkit2gtk::WebView,
    files: Vec<PastedFile>,
    on_done: impl FnOnce() + 'static,
) {
    let mut queue = VecDeque::new();
    queue.push_back(paste_begin_script(&files));
    for (index, (_, _, chunks)) in files.iter().enumerate() {
        for chunk in chunks {
            queue.push_back(format!("__oxidezapPaste.chunk({index}, \"{chunk}\");"));
        }
    }
    queue.push_back("__oxidezapPaste.commit();".to_string());

    let done: ScriptDone = Rc::new(RefCell::new(Some(Box::new(on_done))));
    run_script_queue(webview, Rc::new(RefCell::new(queue)), done);
}

fn run_script_queue(
    webview: webkit2gtk::WebView,
    queue: Rc<RefCell<VecDeque<String>>>,
    done: ScriptDone,
) {
    let Some(script) = queue.borrow_mut().pop_front() else {
        finish_script_queue(&done);
        return;
    };
    let next_webview = webview.clone();
    let next_done = done.clone();
    webview.evaluate_javascript(
        &script,
        None,
        None,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            if let Err(error) = result {
                warn!(%error, "could not inject a pasted file");
                finish_script_queue(&next_done);
                return;
            }
            run_script_queue(next_webview, queue, next_done);
        },
    );
}

fn finish_script_queue(done: &ScriptDone) {
    if let Some(callback) = done.borrow_mut().take() {
        callback();
    }
}

fn mime_for_path(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "3gp" => "video/3gpp",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_stylesheet_only_collapses_animation_and_transitions() {
        assert!(REDUCED_MOTION_STYLESHEET.contains("animation-duration: 0.001ms"));
        assert!(REDUCED_MOTION_STYLESHEET.contains("animation-delay: 0ms"));
        assert!(REDUCED_MOTION_STYLESHEET.contains("transition-duration: 0.001ms"));
        assert!(REDUCED_MOTION_STYLESHEET.contains("transition-delay: 0ms"));
        assert!(REDUCED_MOTION_STYLESHEET.contains("animation-iteration-count: 1"));
        assert!(
            !REDUCED_MOTION_STYLESHEET.contains("display"),
            "the stylesheet must not hide content or media"
        );
        assert!(
            !REDUCED_MOTION_STYLESHEET.contains("visibility"),
            "the stylesheet must not hide content or media"
        );
    }

    #[test]
    fn connection_script_bakes_the_poll_and_has_no_placeholder() {
        let script = CONNECTION_SCRIPT.replace(
            CONNECTION_POLL_PLACEHOLDER,
            &Duration::from_secs(5).as_millis().to_string(),
        );
        assert!(script.contains("defaultPollMs = 5000"));
        assert!(!script.contains(CONNECTION_POLL_PLACEHOLDER));
        assert_eq!(
            connection_poll_script(Duration::from_secs(2)),
            "window.__oxidezapConnectionPoll = 2000;"
        );
    }

    #[test]
    fn parses_browser_connection_reports() {
        assert_eq!(
            parse_connection_report("online=1&login=0"),
            Some(ConnectionReport {
                state: ConnectionState::Online,
                login_page: false,
            })
        );
        assert_eq!(
            parse_connection_report("online=0&login=1"),
            Some(ConnectionReport {
                state: ConnectionState::Offline,
                login_page: true,
            })
        );
        assert_eq!(
            parse_connection_report("login=1&online=1"),
            Some(ConnectionReport {
                state: ConnectionState::Online,
                login_page: true,
            })
        );
        assert_eq!(parse_connection_report("online=1"), None);
        assert_eq!(parse_connection_report("login=0"), None);
        assert_eq!(parse_connection_report("online=2&login=0"), None);
        assert_eq!(parse_connection_report(""), None);
    }

    #[test]
    fn sanitizes_download_names() {
        assert_eq!(sanitize_filename("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename(""), "download");
    }

    #[test]
    fn base64_chunks_round_trip() {
        let bytes = vec![0x5a; PASTE_CHUNK_BYTES + 5];
        let chunks = base64_chunks(&bytes);
        assert_eq!(chunks.len(), 2);
        let decoded: Vec<u8> = chunks
            .iter()
            .flat_map(|chunk| glib::base64_decode(chunk))
            .collect();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn paste_budget_rejects_bytes_past_the_limit() {
        let mut budget = PasteBudget::default();
        assert!(budget.add(PASTE_MAX_BYTES - 1));
        assert!(budget.add(1));
        assert!(!budget.add(1));
    }

    #[test]
    fn share_validation_accepts_regular_media_and_documents() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("photo.png");
        let video = dir.path().join("clip.mp4");
        let pdf = dir.path().join("report.pdf");
        let generic = dir.path().join("notes");
        std::fs::write(&image, b"png").unwrap();
        std::fs::write(&video, b"mp4").unwrap();
        std::fs::write(&pdf, b"pdf").unwrap();
        std::fs::write(&generic, b"text").unwrap();

        let files = validate_share_paths(&[image, video, pdf, generic]).unwrap();

        assert_eq!(files.len(), 4);
        assert_eq!(files[0].mime, "image/png");
        assert!(files[0].image);
        assert!(!files[0].video);
        assert_eq!(files[1].mime, "video/mp4");
        assert!(files[1].video);
        assert_eq!(files[2].mime, "application/pdf");
        assert!(!files[2].image);
        assert!(!files[2].video);
        assert_eq!(files[3].mime, "application/octet-stream");
        assert_eq!(files[3].bytes, 4);
    }

    #[test]
    fn share_validation_rejects_directories_missing_files_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let huge = dir.path().join("huge.bin");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(PASTE_MAX_BYTES + 1).unwrap();
        drop(file);

        assert!(validate_share_paths(&[]).is_err());
        assert!(validate_share_paths(&[dir.path().to_path_buf()]).is_err());
        assert!(validate_share_paths(&[dir.path().join("missing.png")]).is_err());
        let error = validate_share_paths(&[huge]).unwrap_err();
        assert!(error.contains("64 MB"), "{error}");
        assert!(!error.contains('/'), "errors must not leak paths: {error}");
    }

    #[test]
    fn share_validation_applies_the_paste_budget_to_the_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        let half = PASTE_MAX_BYTES / 2;
        std::fs::File::create(&first)
            .unwrap()
            .set_len(half + 1)
            .unwrap();
        std::fs::File::create(&second)
            .unwrap()
            .set_len(half + 1)
            .unwrap();

        assert!(validate_share_paths(&[first, second]).is_err());
    }

    #[test]
    fn file_uri_lists_accept_only_local_files() {
        let paths = parse_file_uri_list(
            "# comment\r\nfile:///home/user/photo%20one.png\r\n\
             https://example.com/remote.png\r\nfile:///home/user/report.pdf\r\n\
             not a uri\r\n",
        );

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/user/photo one.png"),
                PathBuf::from("/home/user/report.pdf"),
            ]
        );
    }

    #[test]
    fn paste_metadata_is_json_escaped_before_it_reaches_javascript() {
        let files = vec![(
            "bad\"name\\\n.png".to_string(),
            "image/png".to_string(),
            Vec::<String>::new(),
        )];

        let script = paste_begin_script(&files);

        assert!(script.contains("bad\\\"name"), "{script}");
        assert!(!script.contains("\"bad\"name"), "{script}");
    }

    #[test]
    fn paste_job_streams_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.png");
        let second_path = dir.path().join("second.pdf");
        let first_bytes = vec![0xAB; PASTE_CHUNK_BYTES + 7];
        std::fs::write(&first_path, &first_bytes).unwrap();
        std::fs::write(&second_path, b"document").unwrap();

        let result = process_paste_job(PasteJob {
            account_id: "account".to_string(),
            image: false,
            files: vec![
                PasteFile {
                    name: "first.png".to_string(),
                    mime: "image/png".to_string(),
                    source: PasteSource::File(first_path),
                },
                PasteFile {
                    name: "second.pdf".to_string(),
                    mime: "application/pdf".to_string(),
                    source: PasteSource::File(second_path),
                },
            ],
        });

        assert_eq!(result.account_id, "account");
        let PasteOutcome::Ready(files) = result.outcome else {
            panic!("expected pasted files");
        };
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "first.png");
        assert_eq!(files[0].1, "image/png");
        assert_eq!(files[0].2.len(), 2);
        let decoded: Vec<u8> = files[0]
            .2
            .iter()
            .flat_map(|chunk| glib::base64_decode(chunk))
            .collect();
        assert_eq!(decoded, first_bytes);
        assert_eq!(files[1].0, "second.pdf");
        assert_eq!(files[1].1, "application/pdf");
        assert_eq!(files[1].2.len(), 1);
    }

    #[test]
    fn paste_job_encodes_clipboard_image_bytes() {
        let bytes: Vec<u8> = (0..PASTE_CHUNK_BYTES + 3)
            .map(|index| index as u8)
            .collect();
        let result = process_paste_job(PasteJob {
            account_id: "account".to_string(),
            image: true,
            files: vec![PasteFile {
                name: "screenshot.png".to_string(),
                mime: "image/png".to_string(),
                source: PasteSource::Bytes(bytes.clone()),
            }],
        });

        let PasteOutcome::Ready(files) = result.outcome else {
            panic!("expected pasted image bytes");
        };
        assert_eq!(files[0].0, "screenshot.png");
        let decoded: Vec<u8> = files[0]
            .2
            .iter()
            .flat_map(|chunk| glib::base64_decode(chunk))
            .collect();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn paste_job_rejects_oversized_files_without_reading_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(PASTE_MAX_BYTES + 1).unwrap();
        drop(file);

        let result = process_paste_job(PasteJob {
            account_id: "account".to_string(),
            image: false,
            files: vec![PasteFile {
                name: "huge.bin".to_string(),
                mime: "application/octet-stream".to_string(),
                source: PasteSource::File(path),
            }],
        });

        assert_eq!(result.outcome, PasteOutcome::TooLarge { image: false });
    }

    #[test]
    fn overlay_target_follows_the_parent_geometry() {
        let geometry = Geometry {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };

        assert_eq!(
            overlay_target(&geometry, 50, 60, 1.0),
            Some((54, 102, 792, 554))
        );
    }

    #[test]
    fn overlay_target_scales_the_titlebar_and_margin() {
        let geometry = Geometry {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };

        assert_eq!(
            overlay_target(&geometry, 50, 60, 2.0),
            Some((58, 144, 784, 508))
        );
    }

    #[test]
    fn overlay_target_rejects_windows_too_small_for_the_content() {
        let geometry = Geometry {
            x: 0,
            y: 0,
            width: 20,
            height: 600,
        };

        assert_eq!(overlay_target(&geometry, 0, 0, 1.0), None);
    }

    #[test]
    fn any_active_or_muted_capture_counts_as_busy() {
        let idle = MediaCaptureState::None;
        let active = MediaCaptureState::Active;
        let muted = MediaCaptureState::Muted;

        assert!(!capture_state_is_active((idle, idle, idle)));
        assert!(capture_state_is_active((active, idle, idle)));
        assert!(capture_state_is_active((idle, muted, idle)));
        assert!(capture_state_is_active((idle, idle, active)));
        assert!(capture_state_is_active((muted, muted, muted)));
    }

    #[test]
    fn a_download_or_capture_refuses_suspension() {
        assert_eq!(suspend_refusal(0, false), None);
        assert_eq!(
            suspend_refusal(1, false),
            Some(SuspendRefusal::DownloadActive)
        );
        assert_eq!(
            suspend_refusal(0, true),
            Some(SuspendRefusal::CaptureActive)
        );
        assert_eq!(
            suspend_refusal(2, true),
            Some(SuspendRefusal::DownloadActive)
        );
    }

    #[test]
    fn only_whatsapp_origins_may_request_permissions() {
        assert_eq!(
            permission_action(
                Some("https://web.whatsapp.com/"),
                PermissionKind::UserMedia,
                true
            ),
            PermissionAction::Prompt
        );
        assert_eq!(
            permission_action(
                Some("https://web.whatsapp.com/"),
                PermissionKind::Notifications,
                true
            ),
            PermissionAction::Allow
        );
        assert_eq!(
            permission_action(
                Some("https://web.whatsapp.com/"),
                PermissionKind::Notifications,
                false
            ),
            PermissionAction::Deny
        );
        assert_eq!(
            permission_action(
                Some("https://example.com/"),
                PermissionKind::UserMedia,
                true
            ),
            PermissionAction::Deny
        );
        assert_eq!(
            permission_action(
                Some("https://web.whatsapp.com.evil.test/"),
                PermissionKind::Notifications,
                true
            ),
            PermissionAction::Deny
        );
        assert_eq!(
            permission_action(None, PermissionKind::UserMedia, true),
            PermissionAction::Deny
        );
    }

    #[test]
    fn unrelated_permissions_are_denied_even_on_whatsapp() {
        assert_eq!(
            permission_action(
                Some("https://web.whatsapp.com/"),
                PermissionKind::Other,
                true
            ),
            PermissionAction::Deny
        );
        assert_eq!(
            permission_action(
                Some("https://flows.whatsapp.net/flows/cache_management/"),
                PermissionKind::Other,
                true
            ),
            PermissionAction::Deny
        );
    }

    fn normal_image_request() -> MediaRequest {
        MediaRequest {
            action: MediaAction::Save,
            kind: MediaKind::Image,
            url: "blob:https://web.whatsapp.com/8f9a3c1e-4d2b".to_string(),
            filename: "whatsapp-image.jpg".to_string(),
            total_bytes: 2048,
            view_once: false,
        }
    }

    #[test]
    fn media_urls_are_limited_to_whatsapp_origins() {
        for url in [
            "blob:https://web.whatsapp.com/8f9a3c1e-4d2b",
            "blob:https://mmg.whatsapp.net/8f9a3c1e-4d2b",
            "https://web.whatsapp.com/media/photo.jpg",
            "https://mmg.whatsapp.net/v/t62/file.enc",
            "data:image/png;base64,iVBORw0KGgo=",
            "data:video/mp4;base64,AAAA",
            "data:audio/ogg;base64,AAAA",
            "data:application/pdf;base64,AAAA",
        ] {
            assert_eq!(
                allowed_media_url(url),
                Ok(()),
                "expected {url} to be allowed"
            );
        }

        for url in [
            "https://example.com/photo.jpg",
            "blob:https://example.com/8f9a3c1e-4d2b",
            "blob:https://web.whatsapp.com.evil.test/8f9a3c1e-4d2b",
            "https://whatsapp.com.evil.test/photo.jpg",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html;base64,AAAA",
            "data:image/svg+xml;base64,AAAA",
            "",
            "not a url",
        ] {
            assert!(
                allowed_media_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn normal_media_requests_are_accepted() {
        assert_eq!(validate_media_request(&normal_image_request()), Ok(()));

        let mut video = normal_image_request();
        video.kind = MediaKind::Video;
        video.action = MediaAction::Save;
        assert_eq!(validate_media_request(&video), Ok(()));

        let mut audio = normal_image_request();
        audio.kind = MediaKind::Audio;
        audio.action = MediaAction::Open;
        assert_eq!(validate_media_request(&audio), Ok(()));

        let mut document = normal_image_request();
        document.kind = MediaKind::Document;
        document.action = MediaAction::Open;
        assert_eq!(validate_media_request(&document), Ok(()));
    }

    #[test]
    fn view_once_media_is_always_rejected() {
        let mut request = normal_image_request();
        request.view_once = true;
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::ViewOnce)
        );

        request.action = MediaAction::Open;
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::ViewOnce)
        );
    }

    #[test]
    fn copy_is_only_offered_for_images() {
        let mut request = normal_image_request();
        request.action = MediaAction::Copy;
        assert_eq!(validate_media_request(&request), Ok(()));

        request.kind = MediaKind::Video;
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::UnsupportedAction)
        );
    }

    #[test]
    fn media_requests_reject_oversized_empty_and_foreign_payloads() {
        let mut request = normal_image_request();
        request.total_bytes = MEDIA_MAX_BYTES + 1;
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::TooLarge)
        );

        request.total_bytes = 0;
        assert_eq!(validate_media_request(&request), Err(MediaRejection::Empty));

        request.total_bytes = 2048;
        request.url = "https://example.com/photo.jpg".to_string();
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::UntrustedOrigin)
        );

        request.url = "file:///etc/passwd".to_string();
        assert_eq!(
            validate_media_request(&request),
            Err(MediaRejection::UnsupportedScheme)
        );
    }

    #[test]
    fn media_message_parser_reads_the_transfer_protocol() {
        let begin = r#"{"type":"begin","id":"t1","action":"save","kind":"image","url":"blob:https://web.whatsapp.com/x","filename":"photo.jpg","size":12,"viewOnce":false}"#;
        let Some(MediaMessage::Begin { id, request }) = parse_media_message(begin) else {
            panic!("expected a begin message");
        };
        assert_eq!(id, "t1");
        assert_eq!(request.action, MediaAction::Save);
        assert_eq!(request.kind, MediaKind::Image);
        assert_eq!(request.total_bytes, 12);

        let chunk = r#"{"type":"chunk","id":"t1","data":"AAAA"}"#;
        assert!(matches!(
            parse_media_message(chunk),
            Some(MediaMessage::Chunk { id, .. }) if id == "t1"
        ));

        let end = r#"{"type":"end","id":"t1"}"#;
        assert!(matches!(
            parse_media_message(end),
            Some(MediaMessage::End { id }) if id == "t1"
        ));

        let failed = r#"{"type":"error","reason":"expired"}"#;
        assert!(matches!(
            parse_media_message(failed),
            Some(MediaMessage::Failed {
                reason: MediaFailureReason::Expired,
                ..
            })
        ));
    }

    #[test]
    fn media_message_parser_ignores_unknown_and_malformed_messages() {
        for message in [
            "not json",
            r#"{"type":"pause"}"#,
            r#"{"type":"begin","id":"t1","action":"delete","kind":"image","url":"blob:https://web.whatsapp.com/x","size":1}"#,
            r#"{"type":"begin","id":"t1","action":"save"}"#,
            r#"{"type":"chunk","id":"t1"}"#,
            r#"{"type":"error","reason":"something-new"}"#,
        ] {
            assert!(parse_media_message(message).is_none(), "{message}");
        }
    }

    #[test]
    fn media_script_honors_the_settings_toggle() {
        assert!(media_script(true).contains("var DEFAULT_ENABLED = true;"));
        assert!(media_script(false).contains("var DEFAULT_ENABLED = false;"));
        assert!(!media_script(true).contains(MEDIA_ENABLED_PLACEHOLDER));
    }

    #[test]
    fn media_chunks_are_bounded_and_reject_invalid_base64() {
        assert_eq!(decode_media_chunk("AAAA", 0, 12), Ok(vec![0, 0, 0]));
        assert_eq!(
            decode_media_chunk("AAAA", 10, 12),
            Err(MediaRejection::TooLarge)
        );
        assert_eq!(
            decode_media_chunk("!!!!", 0, 12),
            Err(MediaRejection::Malformed)
        );
        assert_eq!(
            decode_media_chunk("AAA", 0, 12),
            Err(MediaRejection::Malformed)
        );
    }
}
