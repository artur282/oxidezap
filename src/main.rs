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
mod updates;
mod webview;
mod x11;

use std::fs;
use std::io::Write;
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
use crate::single_instance::{
    ChatTarget, DndMode, ForwardError, InstanceListener, InstanceMessage, InstanceReply,
    InstanceRequest,
};
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Show,
    Toggle,
    Lock,
    Quit,
    Hide,
    Downloads,
    Status,
    ListAccounts,
    Dnd(DndMode),
    OpenUrl(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Cli {
    help: bool,
    version: bool,
    smoke_test: bool,
    autostart: bool,
    action: Option<Action>,
    account: Option<String>,
    chat: Option<String>,
}

impl Cli {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut cli = Self::default();
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "" => {}
                "-h" | "--help" => cli.help = true,
                "-V" | "--version" => cli.version = true,
                "--smoke-test" => cli.smoke_test = true,
                "--autostart" => cli.autostart = true,
                "--show" => cli.set_action(Action::Show)?,
                "--toggle" => cli.set_action(Action::Toggle)?,
                "--lock" => cli.set_action(Action::Lock)?,
                "--quit" => cli.set_action(Action::Quit)?,
                "--hide" => cli.set_action(Action::Hide)?,
                "--downloads" => cli.set_action(Action::Downloads)?,
                "--status" => cli.set_action(Action::Status)?,
                "--list-accounts" => cli.set_action(Action::ListAccounts)?,
                "--account" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.starts_with('-'))
                        .ok_or("--account requires an account id or name")?;
                    cli.set_account(value)?;
                }
                "--chat" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.starts_with('-'))
                        .ok_or("--chat requires a phone number or chat id")?;
                    cli.set_chat(value)?;
                }
                "--dnd" => {
                    let mut mode = DndMode::On;
                    if let Some(value) = args.get(index + 1) {
                        if !value.starts_with('-') && !is_url(value) {
                            mode = parse_dnd(value)?;
                            index += 1;
                        }
                    }
                    cli.set_action(Action::Dnd(mode))?;
                }
                other if other.starts_with("--account=") => {
                    cli.set_account(&other["--account=".len()..])?;
                }
                other if other.starts_with("--chat=") => {
                    cli.set_chat(&other["--chat=".len()..])?;
                }
                other if other.starts_with("--dnd=") => {
                    cli.set_action(Action::Dnd(parse_dnd(&other["--dnd=".len()..])?))?;
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown option: {other}"));
                }
                other if is_url(other) => {
                    if other.chars().any(char::is_control) {
                        return Err("URL arguments must be a single line".to_string());
                    }
                    cli.set_action(Action::OpenUrl(other.to_string()))?;
                }
                other => return Err(format!("unexpected argument: {other}")),
            }
            index += 1;
        }

        if cli.action.is_some() && (cli.account.is_some() || cli.chat.is_some()) {
            return Err("--account and --chat cannot be combined with another action".to_string());
        }
        Ok(cli)
    }

    fn set_action(&mut self, action: Action) -> Result<(), String> {
        if self.action.is_some() {
            return Err("only one action can be requested at a time".to_string());
        }
        self.action = Some(action);
        Ok(())
    }

    fn set_account(&mut self, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("--account requires an account id or name".to_string());
        }
        if value.chars().any(char::is_control) {
            return Err("--account must be a single line".to_string());
        }
        if self.account.is_some() {
            return Err("--account was supplied more than once".to_string());
        }
        self.account = Some(value.to_string());
        Ok(())
    }

    fn set_chat(&mut self, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("--chat requires a phone number or chat id".to_string());
        }
        if self.chat.is_some() {
            return Err("--chat was supplied more than once".to_string());
        }
        self.chat = Some(value.to_string());
        Ok(())
    }

    fn requested_message(&self) -> Result<InstanceMessage, String> {
        if let Some(raw) = &self.chat {
            let chat = ChatTarget::parse(raw)?;
            return Ok(InstanceMessage::OpenChat {
                account: self.account.clone(),
                chat,
            });
        }
        if let Some(account) = &self.account {
            return Ok(InstanceMessage::OpenAccount(account.clone()));
        }
        Ok(match self.action.as_ref() {
            None | Some(Action::Show) => InstanceMessage::Show,
            Some(Action::Toggle) => InstanceMessage::Toggle,
            Some(Action::Lock) => InstanceMessage::Lock,
            Some(Action::Quit) => InstanceMessage::Quit,
            Some(Action::Hide) => InstanceMessage::Hide,
            Some(Action::Downloads) => InstanceMessage::OpenDownloads,
            Some(Action::Status) => InstanceMessage::Status,
            Some(Action::ListAccounts) => InstanceMessage::ListAccounts,
            Some(Action::Dnd(mode)) => InstanceMessage::SetDoNotDisturb(*mode),
            Some(Action::OpenUrl(url)) => InstanceMessage::OpenUrl(url.clone()),
        })
    }
}

fn is_url(value: &str) -> bool {
    value.starts_with("whatsapp://")
        || value.starts_with("http://")
        || value.starts_with("https://")
}

fn parse_dnd(value: &str) -> Result<DndMode, String> {
    DndMode::parse(value)
        .ok_or_else(|| format!("invalid --dnd value '{value}'; expected on, off, 1h, or tomorrow"))
}

fn main() -> Result<()> {
    restrict_process_umask();
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match Cli::parse(&args) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("oxidezap: {error}");
            eprintln!("Try 'oxidezap --help' for more information.");
            std::process::exit(2);
        }
    };
    if cli.help {
        print_help();
        return Ok(());
    }
    if cli.version {
        println!("oxidezap {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let smoke = cli.smoke_test;
    let autostart = cli.autostart;

    let paths = if smoke {
        smoke_paths()?
    } else {
        Paths::discover()?
    };

    let message = match cli.requested_message() {
        Ok(message) => message,
        Err(error) => fail(error),
    };

    if !smoke && message.is_query() {
        return print_query(&paths, &message);
    }

    paths.ensure()?;
    paths.write_runtime_assets()?;

    let _instance_lock = if !smoke {
        match single_instance::acquire_lock(&paths.lock_file)? {
            Some(lock) => {
                // An older build may still own the socket without taking the
                // lock file; forward to it before becoming the primary instance.
                match single_instance::send_request(&paths.socket_file, &message) {
                    Ok(reply) => return apply_reply(reply),
                    Err(ForwardError::NotRunning) => {}
                    Err(ForwardError::Rejected(error)) => fail(error),
                    Err(ForwardError::Io(error)) => {
                        tracing::warn!(%error, "discarding a stale single-instance socket");
                    }
                }
                validate_initial_action(&message, &paths);
                Some(lock)
            }
            None => {
                // Another process is starting or running. Wait briefly for its
                // socket instead of starting a second application window.
                return match single_instance::send_request_with_retry(
                    &paths.socket_file,
                    &message,
                    50,
                ) {
                    Ok(reply) => apply_reply(reply),
                    Err(ForwardError::NotRunning) => fail(
                        "another OxideZap instance is running but is not answering; \
                         remove the stale socket or wait for it to start",
                    ),
                    Err(ForwardError::Rejected(error)) => fail(error),
                    Err(ForwardError::Io(error)) => fail(error),
                };
            }
        }
    } else {
        None
    };

    if message == InstanceMessage::Quit {
        return Ok(());
    }

    let settings_existed = paths.settings_file.exists();
    let settings = if smoke {
        smoke_settings()
    } else {
        Settings::load_or_default(&paths.settings_file)
    };
    let start_hidden = autostart && settings.start_at_login;
    let first_run = !smoke && !settings_existed;

    let (gtk_events_tx, gtk_events_rx) = unbounded();
    let gtk: GtkHandle = webview::spawn(
        gtk_events_tx,
        paths.downloads_dir.clone(),
        settings.notifications,
    )
    .context("WebKit could not start")?;

    let (instance_tx, instance_rx) = unbounded::<InstanceRequest>();
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

    let startup_url = args.iter().find(|arg| is_url(arg.as_str())).cloned();
    let startup_html = if smoke {
        Some(SMOKE_HTML.to_string())
    } else {
        None
    };
    let initial_action = if smoke { None } else { Some(message) };
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
                initial_action,
                start_hidden,
                first_run,
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

fn print_query(paths: &Paths, message: &InstanceMessage) -> Result<()> {
    match single_instance::send_request_with_retry(&paths.socket_file, message, 20) {
        Ok(reply) => apply_reply(reply),
        Err(ForwardError::NotRunning) => {
            let settings = local_settings(paths);
            let text = match message {
                InstanceMessage::Status => app::status_text(&settings, false, 0),
                InstanceMessage::ListAccounts => app::account_list_text(&settings),
                _ => unreachable!("print_query is only used for query messages"),
            };
            print!("{text}");
            std::io::stdout().flush()?;
            Ok(())
        }
        Err(ForwardError::Rejected(error)) => fail(error),
        Err(ForwardError::Io(error)) => fail(error),
    }
}

fn apply_reply(reply: InstanceReply) -> Result<()> {
    match reply {
        InstanceReply::Ok => Ok(()),
        InstanceReply::Output(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            std::io::stdout().flush()?;
            Ok(())
        }
        InstanceReply::Error(error) => fail(error),
    }
}

fn validate_initial_action(message: &InstanceMessage, paths: &Paths) {
    let settings = local_settings(paths);
    let result = match message {
        InstanceMessage::OpenAccount(query) => app::resolve_account(&settings, query).map(|_| ()),
        InstanceMessage::OpenChat {
            account: Some(query),
            ..
        } => app::resolve_account(&settings, query).map(|_| ()),
        InstanceMessage::OpenChat { account: None, .. } => {
            if settings.active_account_id().is_some() {
                Ok(())
            } else {
                Err("no active account; pass --account".to_string())
            }
        }
        InstanceMessage::Lock => {
            if settings.lock.enabled && settings.lock.hash.is_some() {
                Ok(())
            } else {
                Err("the application lock is not configured".to_string())
            }
        }
        _ => Ok(()),
    };
    if let Err(error) = result {
        fail(error);
    }
}

fn local_settings(paths: &Paths) -> Settings {
    Settings::load(&paths.settings_file).unwrap_or_default()
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("oxidezap: {message}");
    std::process::exit(1);
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oxidezap=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Apply a restrictive file creation mask before any thread or file is
/// created, so WebKitGTK's cookies, localStorage, IndexedDB and cache files
/// inherit private permissions instead of the desktop default `0644`/`0755`.
#[cfg(unix)]
fn restrict_process_umask() {
    // SAFETY: `umask` only modifies the process-wide mode creation mask. It is
    // called at the start of `main`, before any worker thread exists.
    unsafe {
        libc::umask(0o077);
    }
}

#[cfg(not(unix))]
fn restrict_process_umask() {}

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
        lock_file: root.join("instance.lock"),
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
         Actions:\n\
           (no option)             Show the running instance, or start it\n\
           --toggle                Show or hide the running instance\n\
           --hide                  Hide all account windows without closing them\n\
           --lock                  Lock the running instance\n\
           --quit                  Quit the running instance\n\
           --account <id|name>     Open or focus an account\n\
           --chat <phone|id>       Open a chat in the active account (never sends)\n\
           --dnd [MODE]            Set Do Not Disturb: on, off, 1h, or tomorrow\n\
           --downloads             Open the downloads folder\n\
           --status                Print version, accounts, DND, and window count\n\
           --list-accounts         List accounts as id, name, and active state\n\
         \n\
         Other options:\n\
           --autostart             Start hidden when launched at login\n\
           --smoke-test            Run the X11/WebKit overlay smoke test and exit\n\
           -h, --help              Print this help\n\
           -V, --version           Print the version\n\
         \n\
         --dnd 1h enables Do Not Disturb for one hour and --dnd tomorrow until\n\
         the next local midnight. Use Settings or the tray for recurring quiet hours.\n\
         --chat accepts a phone number or <number>@c.us and never sends a message.\n\
         URLs: whatsapp://send?phone=..., https://wa.me/..., https://chat.whatsapp.com/...\n\
         The application requires WebKitGTK 4.1 and an X11 or XWayland session.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_documented_actions() {
        assert_eq!(
            Cli::parse(&args(&["--toggle"])).unwrap().action,
            Some(Action::Toggle)
        );
        assert_eq!(
            Cli::parse(&args(&["--lock"])).unwrap().action,
            Some(Action::Lock)
        );
        assert_eq!(
            Cli::parse(&args(&["--quit"])).unwrap().action,
            Some(Action::Quit)
        );
        assert_eq!(
            Cli::parse(&args(&["--hide"])).unwrap().action,
            Some(Action::Hide)
        );
        assert_eq!(
            Cli::parse(&args(&["--downloads"])).unwrap().action,
            Some(Action::Downloads)
        );
        assert_eq!(
            Cli::parse(&args(&["--status"])).unwrap().action,
            Some(Action::Status)
        );
        assert_eq!(
            Cli::parse(&args(&["--list-accounts"])).unwrap().action,
            Some(Action::ListAccounts)
        );
        assert!(Cli::parse(&args(&["--autostart"])).unwrap().autostart);
        assert!(Cli::parse(&args(&["--smoke-test"])).unwrap().smoke_test);
    }

    #[test]
    fn parses_account_and_chat_values() {
        let cli = Cli::parse(&args(&["--account", "Work"])).unwrap();
        assert_eq!(cli.account.as_deref(), Some("Work"));
        assert_eq!(cli.action, None);

        let cli = Cli::parse(&args(&["--chat", "+54 9 11 1234-5678"])).unwrap();
        assert_eq!(cli.chat.as_deref(), Some("+54 9 11 1234-5678"));

        let cli = Cli::parse(&args(&["--account=acc-1", "--chat=5491112345678@c.us"])).unwrap();
        assert_eq!(cli.account.as_deref(), Some("acc-1"));
        assert_eq!(cli.chat.as_deref(), Some("5491112345678@c.us"));
    }

    #[test]
    fn combines_account_and_chat_into_one_message() {
        let cli = Cli::parse(&args(&["--account", "Work", "--chat", "5491112345678"])).unwrap();
        let message = cli.requested_message().unwrap();
        assert_eq!(
            message,
            InstanceMessage::OpenChat {
                account: Some("Work".to_string()),
                chat: ChatTarget::parse("5491112345678").unwrap(),
            }
        );
    }

    #[test]
    fn dnd_accepts_an_optional_value() {
        assert_eq!(
            Cli::parse(&args(&["--dnd"])).unwrap().action,
            Some(Action::Dnd(DndMode::On))
        );
        assert_eq!(
            Cli::parse(&args(&["--dnd", "off"])).unwrap().action,
            Some(Action::Dnd(DndMode::Off))
        );
        assert_eq!(
            Cli::parse(&args(&["--dnd=1h"])).unwrap().action,
            Some(Action::Dnd(DndMode::OneHour))
        );
        assert_eq!(
            Cli::parse(&args(&["--dnd", "tomorrow"])).unwrap().action,
            Some(Action::Dnd(DndMode::Tomorrow))
        );
        assert_eq!(
            Cli::parse(&args(&["--dnd", "--hide"])).unwrap_err(),
            "only one action can be requested at a time"
        );
        assert!(Cli::parse(&args(&["--dnd", "later"])).is_err());
    }

    #[test]
    fn rejects_conflicting_actions_and_bad_values() {
        assert!(Cli::parse(&args(&["--toggle", "--hide"])).is_err());
        assert!(Cli::parse(&args(&["--status", "--account", "Work"])).is_err());
        assert!(Cli::parse(&args(&["--chat", "--hide"])).is_err());
        assert!(Cli::parse(&args(&["--account"])).is_err());
        assert!(Cli::parse(&args(&["--chat"])).is_err());
        assert!(Cli::parse(&args(&["--account="])).is_err());
        assert!(Cli::parse(&args(&["--unknown"])).is_err());
        assert!(Cli::parse(&args(&["not-a-url"])).is_err());
    }

    #[test]
    fn urls_become_open_url_actions() {
        assert_eq!(
            Cli::parse(&args(&["whatsapp://send?phone=123456"]))
                .unwrap()
                .action,
            Some(Action::OpenUrl("whatsapp://send?phone=123456".to_string()))
        );
        assert_eq!(
            Cli::parse(&args(&["https://wa.me/123456"])).unwrap().action,
            Some(Action::OpenUrl("https://wa.me/123456".to_string()))
        );
    }

    #[test]
    fn a_bare_invocation_requests_show() {
        assert_eq!(
            Cli::parse(&args(&[])).unwrap().requested_message().unwrap(),
            InstanceMessage::Show
        );
        assert_eq!(
            Cli::parse(&args(&["--autostart"]))
                .unwrap()
                .requested_message()
                .unwrap(),
            InstanceMessage::Show
        );
    }
}
