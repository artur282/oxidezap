use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use gtk::prelude::*;
use tracing::{debug, info, warn};
use webkit2gtk::{
    DownloadExt, NavigationPolicyDecision, NavigationPolicyDecisionExt, NotificationExt,
    NotificationPermissionRequest, PermissionRequest, PermissionRequestExt, PolicyDecisionExt,
    PolicyDecisionType, SettingsExt, URIRequestExt, UserMediaPermissionRequest,
    UserMediaPermissionRequestExt, WebContextExt, WebViewExt,
};

use crate::links::{self, LinkAction};
use crate::unread::parse_unread;
use crate::x11::X11;

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
        account_id: String,
        filename: String,
    },
    DownloadFinished {
        account_id: String,
        filename: String,
        path: PathBuf,
        success: bool,
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
}

#[derive(Clone, Debug)]
pub enum GtkCommand {
    Create {
        account_id: String,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        url: String,
        html: Option<String>,
        show: bool,
        scale: f32,
        zoom: f64,
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
                downloads_dir,
                notifications_enabled,
                entries: HashMap::new(),
                x11,
                paste: paste.clone(),
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
    downloads_dir: PathBuf,
    notifications_enabled: bool,
    entries: HashMap<String, WebEntry>,
    x11: X11,
    paste: Rc<RefCell<PasteQueue>>,
}

struct WebEntry {
    window: gtk::Window,
    webview: webkit2gtk::WebView,
    xid: u32,
    visible: bool,
    scale: f32,
    zoom: f64,
    transient_for: Option<u32>,
    last_geometry: Option<(i32, i32, u32, u32)>,
}

impl WebEntry {
    fn hide(&mut self) {
        if self.window.is_visible() {
            self.window.hide();
        }
        self.last_geometry = None;
    }
}

fn handle_command(command: GtkCommand, state: &mut GtkState) {
    match command {
        GtkCommand::Create {
            account_id,
            data_dir,
            cache_dir,
            url,
            html,
            show,
            scale,
            zoom,
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
                state.events.clone(),
                state.downloads_dir.clone(),
                state.notifications_enabled,
                state.paste.clone(),
            ) {
                Ok(entry) => {
                    let xid = entry.xid;
                    state.entries.insert(account_id.clone(), entry);
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
                entry.webview.reload();
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
    events: Sender<GtkEvent>,
    downloads_dir: PathBuf,
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
    let context = webkit2gtk::WebContext::with_website_data_manager(&manager);
    let webview = webkit2gtk::WebView::with_context(&context);
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
    connect_downloads(&context, account_id, events, downloads_dir);
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
        visible: show,
        scale,
        zoom,
        transient_for: None,
        last_geometry: None,
    })
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
    webview.connect_web_process_terminated(move |_webview, _reason| {
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
    downloads_dir: PathBuf,
) {
    let download_account = account_id.to_string();
    context.connect_download_started(move |_context, download| {
        let account_id = download_account.clone();
        let events = events.clone();
        let downloads_dir = downloads_dir.clone();
        download.connect_decide_destination(move |download, suggested| {
            let filename = sanitize_filename(suggested);
            let path = unique_download_path(&downloads_dir, &filename);
            download.set_destination(path.to_string_lossy().as_ref());
            let _ = events.send(GtkEvent::DownloadStarted {
                account_id: account_id.clone(),
                filename: filename.clone(),
            });

            let finished_path = path.clone();
            let finished_filename = filename.clone();
            let finished_account = account_id.clone();
            let finished_events = events.clone();
            download.connect_finished(move |_download| {
                let _ = finished_events.send(GtkEvent::DownloadFinished {
                    account_id: finished_account.clone(),
                    filename: finished_filename.clone(),
                    path: finished_path.clone(),
                    success: true,
                });
            });

            let failed_path = path.clone();
            let failed_filename = filename.clone();
            let failed_account = account_id.clone();
            let failed_events = events.clone();
            download.connect_failed(move |_download, error| {
                warn!(%error, "download failed");
                let _ = failed_events.send(GtkEvent::DownloadFinished {
                    account_id: failed_account.clone(),
                    filename: failed_filename.clone(),
                    path: failed_path.clone(),
                    success: false,
                });
            });
            true
        });
    });
}

fn sync_overlays(state: &mut GtkState) {
    let x11 = &state.x11;
    for (account_id, entry) in state.entries.iter_mut() {
        if !entry.visible {
            entry.hide();
            continue;
        }
        let Ok(Some(parent)) = x11.find_account_window(account_id) else {
            entry.hide();
            continue;
        };
        if !x11.is_viewable(parent).unwrap_or(false) {
            entry.hide();
            continue;
        }
        let Ok(geometry) = x11.geometry(parent) else {
            entry.hide();
            continue;
        };
        let Ok((root_x, root_y)) = x11.root_position(parent) else {
            entry.hide();
            continue;
        };
        let titlebar = (TITLEBAR_HEIGHT * entry.scale).round().max(1.0) as u32;
        let margin = (OVERLAY_MARGIN * entry.scale).round().max(1.0) as u32;
        let width = u32::from(geometry.width).saturating_sub(margin * 2);
        let height = u32::from(geometry.height).saturating_sub(titlebar + margin);
        if width < 20 || height < 20 {
            entry.hide();
            continue;
        }

        if entry.transient_for != Some(parent) {
            if let Err(error) = x11.set_transient(entry.xid, parent) {
                warn!(%error, "could not make the WebKit window transient");
            }
            entry.transient_for = Some(parent);
            entry.last_geometry = None;
        }

        let target = (
            root_x + margin as i32,
            root_y + titlebar as i32,
            width,
            height,
        );
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

fn sanitize_filename(filename: &str) -> String {
    let filename = filename
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            _ => ch,
        })
        .collect::<String>();
    let filename = filename.trim();
    if filename.is_empty() {
        "download".to_string()
    } else {
        filename.to_string()
    }
}

fn unique_download_path(dir: &Path, filename: &str) -> PathBuf {
    let path = dir.join(filename);
    if !path.exists() {
        return path;
    }
    let stem = Path::new(filename)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string());
    let extension = Path::new(filename)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    for index in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path
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
    fn download_paths_do_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_download_path(dir.path(), "file.txt");
        assert_eq!(first, dir.path().join("file.txt"));
        std::fs::write(&first, b"x").unwrap();
        let second = unique_download_path(dir.path(), "file.txt");
        assert_eq!(second, dir.path().join("file (1).txt"));
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
}
