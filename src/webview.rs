use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use gtk::prelude::*;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webkit2gtk::{
    DownloadExt, NavigationPolicyDecision, NavigationPolicyDecisionExt, NetworkError,
    NetworkProxyMode, NetworkProxySettings, NotificationExt, NotificationPermissionRequest,
    PermissionRequest, PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType, PolicyError,
    SettingsExt, URIRequestExt, URIResponseExt, UserContentInjectedFrames, UserContentManager,
    UserContentManagerExt, UserMediaPermissionRequest, UserMediaPermissionRequestExt, UserScript,
    UserScriptInjectionTime, WebContextExt, WebProcessTerminationReason, WebViewExt,
    WebViewExtManual, WebsiteDataManagerExt,
};

use crate::app::downloads::{
    DOWNLOAD_PROGRESS_INTERVAL, DownloadLayout, discard_empty_reservation, sanitize_filename,
};
use crate::links::{self, LinkAction};
use crate::paths::Paths;
use crate::settings::storage::{self, StorageOperation};
use crate::settings::{ProxyMode, ProxySettings, WritingSettings};
use crate::unread::parse_unread;
use crate::x11::{Geometry, X11};

/// The height of the native GPUI titlebar, in logical pixels. The WebKit
/// overlay is positioned below it using the window scale factor.
pub const TITLEBAR_HEIGHT: f32 = 42.0;
/// Exposed GPUI pixels around the WebKit overlay so the window edge-resize
/// handles remain reachable instead of being covered by the GTK window.
pub const OVERLAY_MARGIN: f32 = 4.0;
pub const WHATSAPP_URL: &str = "https://web.whatsapp.com/";

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
}

#[derive(Clone, Debug)]
pub enum GtkCommand {
    Create {
        account_id: String,
        /// GPUI window the overlay is anchored to. `None` means discovery
        /// failed and the account tag will be used once as a fallback.
        parent_xid: Option<u32>,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        url: String,
        html: Option<String>,
        show: bool,
        scale: f32,
        zoom: f64,
        writing: WritingSettings,
        proxy: ProxySettings,
    },
    Show {
        account_id: String,
    },
    Hide {
        account_id: String,
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
                    sync_overlays(&mut state);
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
}

struct WebEntry {
    window: gtk::Window,
    webview: webkit2gtk::WebView,
    xid: u32,
    /// Cached GPUI window XID. Synchronization never scans the X11 tree while
    /// this XID is valid.
    parent_xid: Option<u32>,
    visible: bool,
    scale: f32,
    zoom: f64,
    transient_for: Option<u32>,
    last_geometry: Option<(i32, i32, u32, u32)>,
    /// Number of downloads that have started but not finished. Reloading the
    /// page while a download is streaming would cancel it, so the command
    /// handler refuses to do that.
    downloads: Rc<Cell<u32>>,
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
}

fn handle_command(command: GtkCommand, state: &mut GtkState) {
    match command {
        GtkCommand::Create {
            account_id,
            parent_xid,
            data_dir,
            cache_dir,
            url,
            html,
            show,
            scale,
            zoom,
            writing,
            proxy,
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
                state.events.clone(),
                state.download_layout.clone(),
                state.downloads.clone(),
                state.cancelled_downloads.clone(),
                state.notifications_enabled,
                state.paste.clone(),
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
        GtkCommand::SetDownloadLayout(layout) => {
            *state.download_layout.borrow_mut() = layout;
        }
        GtkCommand::CancelDownload { id } => {
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
        GtkCommand::Quit => {
            for (_, entry) in state.entries.drain() {
                entry.window.close();
            }
            gtk::main_quit();
        }
    }
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
    events: Sender<GtkEvent>,
    download_layout: Rc<RefCell<DownloadLayout>>,
    download_registry: Rc<RefCell<HashMap<String, webkit2gtk::Download>>>,
    cancelled_downloads: Rc<RefCell<HashSet<String>>>,
    notifications_enabled: bool,
    paste: Rc<RefCell<PasteQueue>>,
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
    window.set_type_hint(gtk::gdk::WindowTypeHint::Utility);
    window.set_default_size(800, 600);

    let manager = webkit2gtk::WebsiteDataManager::builder()
        .base_data_directory(data_dir.to_string_lossy().as_ref())
        .base_cache_directory(cache_dir.to_string_lossy().as_ref())
        .build();
    apply_proxy(&manager, proxy);
    let context = webkit2gtk::WebContext::with_website_data_manager(&manager);
    apply_spell_checking(&context, writing);
    let user_content = UserContentManager::new();
    connect_connection_script(&user_content, account_id, events.clone());
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
    );
    let downloads = Rc::new(Cell::new(0));
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
    webview.connect_key_press_event(move |_webview, event| {
        if try_paste_clipboard(&paste_account, event, &paste) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    window.add(&webview);
    window.realize();

    let gdk_window = window
        .window()
        .context("the GTK window has no GDK window after realize")?;
    let x11_window = gdk_window
        .downcast::<gdkx11::X11Window>()
        .map_err(|_| anyhow!("the GTK window is not an X11 window"))?;
    let xid = x11_window.xid() as u32;

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
        visible: show,
        scale,
        zoom,
        transient_for: None,
        last_geometry: None,
        downloads,
    })
}

const CONNECTION_MESSAGE_HANDLER: &str = "oxidezapConnection";

/// Reports the browser's online state and whether the QR login screen is
/// showing. User scripts are injected by WebKit itself, so the page CSP does
/// not block them. The script deliberately reads only `navigator.onLine` and
/// a short list of known QR containers; it does not inspect chat content.
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

  window.addEventListener("online", report);
  window.addEventListener("offline", report);
  setInterval(report, 2000);
  report();
})();
"#;

fn connect_connection_script(
    manager: &UserContentManager,
    account_id: &str,
    events: Sender<GtkEvent>,
) {
    if !manager.register_script_message_handler(CONNECTION_MESSAGE_HANDLER) {
        warn!(account_id = %account_id, "could not register the connection message handler");
        return;
    }
    manager.add_script(&UserScript::new(
        CONNECTION_SCRIPT,
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
) {
    let load_account = account_id.to_string();
    let load_events = events.clone();
    webview.connect_load_changed(move |_webview, event| {
        let state = match event {
            webkit2gtk::LoadEvent::Started => LoadState::Started,
            webkit2gtk::LoadEvent::Redirected => LoadState::Redirected,
            webkit2gtk::LoadEvent::Committed => LoadState::Committed,
            webkit2gtk::LoadEvent::Finished => LoadState::Finished,
            _ => return,
        };
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
    let x11 = &state.x11;
    for (account_id, entry) in state.entries.iter_mut() {
        if !entry.visible {
            entry.hide();
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
        .load_from_data(b"window { background-color: #0b141a; }")
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
            warn!(%error, "could not queue a clipboard paste");
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
                        warn!(%error, path = %path.display(), "could not read a pasted file");
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

fn queue_paste_files(
    webview: webkit2gtk::WebView,
    files: Vec<PastedFile>,
    on_done: impl FnOnce() + 'static,
) {
    let metadata: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, mime, _)| serde_json::json!({ "name": name, "type": mime }))
        .collect();
    let mut queue = VecDeque::new();
    queue.push_back(format!(
        "{PASTE_HELPER}\n__oxidezapPaste.begin({});",
        serde_json::to_string(&metadata).unwrap_or_else(|_| "[]".to_string())
    ));
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
}
