mod app;
mod assets;
mod autostart;
mod badge;
mod hotkey;
mod links;
mod lock;
mod notify;
mod paths;
mod settings;
mod shortcut;
mod single_instance;
mod theme;
mod tray;
mod ui;
mod unread;
mod webview;
mod x11;

use std::fs;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel::unbounded;
use gpui::{AppContext, Application};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::app::{AppModel, SmokeState};
use crate::assets::Assets;
use crate::badge::Badge;
use crate::hotkey::HotkeyHandle;
use crate::paths::Paths;
use crate::settings::{Account, Settings};
use crate::shortcut::Shortcut;
use crate::single_instance::{InstanceListener, InstanceMessage};
use crate::tray::TrayHandle;
use crate::webview::GtkHandle;
use crate::x11::X11;

const SMOKE_HTML: &str = r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>OxideZap smoke test</title></head>
<body style="margin:0;background:#25d366;color:#0b141a;font:600 42px system-ui;display:flex;align-items:center;justify-content:center;height:100vh">
  OxideZap WebKit overlay
</body>
</html>"#;

fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("oxidezap {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let smoke = args.iter().any(|arg| arg == "--smoke-test");
    let autostart = args.iter().any(|arg| arg == "--autostart");
    let quit_requested = args.iter().any(|arg| arg == "--quit");

    let paths = if smoke {
        smoke_paths()?
    } else {
        Paths::discover()?
    };
    paths.ensure()?;
    paths.write_runtime_assets()?;

    if !smoke {
        let forwarded: Vec<String> = args
            .iter()
            .filter(|arg| arg.as_str() != "--autostart")
            .cloned()
            .collect();
        if single_instance::send_to_running(&paths.socket_file, &forwarded) {
            return Ok(());
        }
        if quit_requested {
            return Ok(());
        }
    }

    let settings = if smoke {
        smoke_settings()
    } else {
        Settings::load_or_default(&paths.settings_file)
    };
    let start_hidden = autostart && settings.start_at_login;

    let (gtk_events_tx, gtk_events_rx) = unbounded();
    let gtk: GtkHandle = webview::spawn(gtk_events_tx, paths.downloads_dir.clone())
        .context("WebKit could not start")?;

    let (instance_tx, instance_rx) = unbounded::<InstanceMessage>();
    let instance_listener: Option<InstanceListener> = if smoke {
        None
    } else {
        match single_instance::listen(&paths.socket_file, instance_tx) {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!(%error, "single-instance socket unavailable");
                None
            }
        }
    };

    let (tray_tx, tray_rx) = unbounded();
    let tray: Option<TrayHandle> = if smoke {
        None
    } else {
        TrayHandle::start(tray_tx)
    };

    let (hotkey_tx, hotkey_rx) = unbounded();
    let shortcut = Shortcut::parse(&settings.shortcut).unwrap_or_default();
    let hotkey: HotkeyHandle = hotkey::start(shortcut, hotkey_tx);
    let badge = Badge::start();
    let x11 = X11::connect().context("OxideZap requires an X11 or XWayland session")?;

    let startup_url = args
        .iter()
        .find(|arg| {
            !arg.starts_with("--")
                && (arg.starts_with("whatsapp://")
                    || arg.starts_with("http://")
                    || arg.starts_with("https://"))
        })
        .cloned();
    let startup_html = if smoke {
        Some(SMOKE_HTML.to_string())
    } else {
        None
    };
    let smoke_state = if smoke {
        Some(SmokeState {
            account_id: "smoke".to_string(),
            started: Instant::now(),
            gtk_xid: None,
            loaded_at: None,
            verified: false,
        })
    } else {
        None
    };

    let settings_for_model = settings.clone();
    Application::new().with_assets(Assets).run(move |cx| {
        let model = cx.new(|cx| {
            AppModel::new(
                cx,
                paths.clone(),
                settings_for_model.clone(),
                gtk.clone(),
                gtk_events_rx,
                x11,
                tray,
                tray_rx,
                hotkey,
                hotkey_rx,
                instance_listener,
                instance_rx,
                badge,
                smoke_state,
                startup_html,
                startup_url,
                start_hidden,
            )
        });
        model.update(cx, |model, cx| {
            model.start_pump(cx);
            model.start(cx);
        });
        info!("OxideZap started");
    });

    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oxidezap=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn smoke_paths() -> Result<Paths> {
    let root = std::env::temp_dir().join(format!("oxidezap-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    Ok(Paths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        downloads_dir: root.join("downloads"),
        settings_file: root.join("config/settings.json"),
        accounts_dir: root.join("data/accounts"),
        socket_file: root.join("instance.sock"),
    })
}

fn smoke_settings() -> Settings {
    Settings {
        accounts: vec![Account::with_id("smoke", "Smoke test")],
        active_account: Some("smoke".to_string()),
        close_to_tray: false,
        remember_bounds: false,
        ..Settings::default()
    }
}

fn print_help() {
    println!(
        "OxideZap {}\n\
         A native WhatsApp Web desktop client for Linux, built with Rust and GPUI.\n\
         \n\
         Usage: oxidezap [OPTIONS] [URL]\n\
         \n\
         Options:\n\
           --toggle        Show or hide the running instance\n\
           --lock          Lock the running instance\n\
           --quit          Quit the running instance\n\
           --autostart     Start hidden when launched at login\n\
           --smoke-test    Run the X11/WebKit overlay smoke test and exit\n\
           -h, --help      Print this help\n\
           -V, --version   Print the version\n\
         \n\
         URLs: whatsapp://send?phone=..., https://wa.me/..., https://chat.whatsapp.com/...\n\
         The application requires WebKitGTK 4.1 and an X11 or XWayland session.",
        env!("CARGO_PKG_VERSION")
    );
}
