use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Sender};
use gtk::prelude::*;
use tracing::{debug, info, warn};
use webkit2gtk::{
    DownloadExt, NavigationPolicyDecision, NavigationPolicyDecisionExt, NotificationExt,
    PermissionRequestExt, PolicyDecisionExt, PolicyDecisionType, SettingsExt, URIRequestExt,
    WebContextExt, WebViewExt,
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

pub fn spawn(events: Sender<GtkEvent>, downloads_dir: PathBuf) -> Result<GtkHandle> {
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
            let state = Rc::new(RefCell::new(GtkState {
                events,
                downloads_dir,
                entries: HashMap::new(),
                x11,
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
    entries: HashMap<String, WebEntry>,
    x11: X11,
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
) -> Result<WebEntry> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("could not create {}", cache_dir.display()))?;

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
    }
    webview.set_zoom_level(zoom);

    connect_webview_signals(&webview, account_id, events.clone());
    connect_downloads(&context, account_id, events, downloads_dir);

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

    webview.connect_permission_request(|_webview, request| {
        request.allow();
        true
    });

    let crash_account = account_id.to_string();
    let crash_events = events;
    webview.connect_web_process_terminated(move |_webview, _reason| {
        let _ = crash_events.send(GtkEvent::Crashed {
            account_id: crash_account.clone(),
        });
    });
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
    fn download_paths_do_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_download_path(dir.path(), "file.txt");
        assert_eq!(first, dir.path().join("file.txt"));
        std::fs::write(&first, b"x").unwrap();
        let second = unique_download_path(dir.path(), "file.txt");
        assert_eq!(second, dir.path().join("file (1).txt"));
    }
}
