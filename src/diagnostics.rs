//! Privacy-scrubbed diagnostics for bug reports plus a bounded in-memory log
//! of recent warnings and errors.
//!
//! The report is assembled from structured values that OxideZap controls. It
//! never reads WhatsApp conversations, cookies, account profiles, proxy
//! credentials, or the app-lock hash, and every path is shortened to `~` when
//! it lives under the current home directory.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::paths::Paths;
use crate::settings::{Settings, format_timestamp};

/// Maximum number of warnings and errors kept for the report. Older entries
/// are discarded first so a long session cannot grow memory without bound.
pub const MAX_ERROR_ENTRIES: usize = 50;
/// Maximum length of one stored error message, in characters.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 400;
/// Maximum length of a system value such as a desktop name. Values come from
/// environment variables and files, so they are flattened and bounded before
/// the report includes them.
const MAX_FIELD_CHARS: usize = 160;
/// Assignment prefixes removed from free-form error messages as a second line
/// of defense. The curated messages never contain credentials.
const SENSITIVE_ASSIGNMENT_KEYS: [&str; 7] = [
    "password=",
    "passwd=",
    "token=",
    "secret=",
    "cookie=",
    "authorization=",
    "hash=",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn label(self) -> &'static str {
        match self {
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub level: DiagnosticLevel,
    pub message: String,
}

/// Small bounded queue of warning and error messages for the current session.
/// Entries are scrubbed when they are recorded, so a caller cannot leak an
/// account id or a credential by accident.
#[derive(Clone, Debug, Default)]
pub struct ErrorLog {
    entries: VecDeque<DiagnosticEntry>,
}

impl ErrorLog {
    pub fn record(&mut self, level: DiagnosticLevel, message: impl AsRef<str>) {
        let message = scrub_message(message.as_ref(), current_home().as_deref());
        if message.is_empty() {
            return;
        }
        while self.entries.len() >= MAX_ERROR_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(DiagnosticEntry { level, message });
    }

    pub fn to_vec(&self) -> Vec<DiagnosticEntry> {
        self.entries.iter().cloned().collect()
    }
}

/// Counts of runtime state that the report summarizes. Account ids and names
/// are deliberately absent: the report only keeps aggregate numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeSummary {
    pub locked: bool,
    pub loaded_views: usize,
    pub suspended_views: usize,
    pub resuming_views: usize,
    pub connected: usize,
    pub offline: usize,
    pub reconnecting: usize,
    pub connecting: usize,
    pub crash_tracking: usize,
    pub crash_failed: usize,
    pub crash_attempts: u32,
    pub active_downloads: usize,
}

/// Everything the report builder needs. It is cloned on the GPUI thread and
/// rendered on a background worker, so report generation never blocks a frame.
#[derive(Clone, Debug)]
pub struct DiagnosticsContext {
    pub app_version: String,
    pub settings: Settings,
    pub paths: Paths,
    pub home_dir: Option<PathBuf>,
    pub runtime: RuntimeSummary,
    pub errors: Vec<DiagnosticEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRow {
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportSection {
    pub title: &'static str,
    pub rows: Vec<ReportRow>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsReport {
    pub generated_at: String,
    pub sections: Vec<ReportSection>,
    pub errors: Vec<DiagnosticEntry>,
}

impl DiagnosticsReport {
    pub fn render_text(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "OxideZap diagnostics");
        let _ = writeln!(text, "Generated: {}", self.generated_at);
        let _ = writeln!(
            text,
            "Privacy: no chats, account names or ids, cookies, proxy credentials, \
             lock hashes, or tokens. Home paths are shortened to ~."
        );
        for section in &self.sections {
            let _ = writeln!(text);
            let _ = writeln!(text, "[{}]", section.title);
            for row in &section.rows {
                let _ = writeln!(text, "{}: {}", row.label, row.value);
            }
        }
        let _ = writeln!(text);
        let _ = writeln!(text, "[Recent errors]");
        if self.errors.is_empty() {
            let _ = writeln!(text, "None recorded in this session.");
        } else {
            for entry in &self.errors {
                let _ = writeln!(text, "{}: {}", entry.level.label(), entry.message);
            }
        }
        text
    }
}

/// Builds the report from an owned snapshot. This function performs only local
/// reads (`/etc/os-release`, `/proc`, environment variables, and GTK/WebKit
/// version constants) and is safe to run on a background worker.
pub fn collect(context: &DiagnosticsContext) -> DiagnosticsReport {
    let home = context.home_dir.as_deref();
    let account_count = context.settings.accounts.len();
    let accounts = if context.settings.active_account.is_some() {
        format!("{account_count} configured, active account selected")
    } else {
        format!("{account_count} configured, no active account")
    };

    let mut sections = vec![
        ReportSection {
            title: "Application",
            rows: vec![
                row("Version", context.app_version.clone()),
                row("Accounts", accounts),
            ],
        },
        ReportSection {
            title: "System",
            rows: vec![
                row("Distribution", os_name()),
                row("Kernel", kernel_version()),
                row("Architecture", std::env::consts::ARCH.to_string()),
                row("CPU", cpu_summary()),
                row("Memory", memory_summary()),
                row("GTK", gtk_version()),
                row("WebKitGTK", webkit_version()),
            ],
        },
        ReportSection {
            title: "Session",
            rows: vec![
                row("Type", session_type()),
                row("Desktop", desktop_name()),
                row("DISPLAY", env_flag("DISPLAY")),
                row("WAYLAND_DISPLAY", env_flag("WAYLAND_DISPLAY")),
                row("GDK_BACKEND", gdk_backend()),
                row("WebKit environment", webkit_env_names()),
            ],
        },
        ReportSection {
            title: "Settings",
            rows: settings_rows(context, home),
        },
        ReportSection {
            title: "Runtime",
            rows: runtime_rows(&context.runtime),
        },
        ReportSection {
            title: "Paths",
            rows: vec![
                row(
                    "Config directory",
                    redact_home_path(&context.paths.config_dir, home),
                ),
                row(
                    "Data directory",
                    redact_home_path(&context.paths.data_dir, home),
                ),
                row(
                    "Cache directory",
                    redact_home_path(&context.paths.cache_dir, home),
                ),
                row(
                    "Downloads directory",
                    redact_home_path(&context.paths.downloads_dir, home),
                ),
                row(
                    "Settings file",
                    redact_home_path(&context.paths.settings_file, home),
                ),
                row(
                    "Log file",
                    redact_home_path(&context.paths.log_file(), home),
                ),
            ],
        },
    ];

    if let Some(home) = home {
        for section in &mut sections {
            for row in &mut section.rows {
                row.value = redact_home_text(&row.value, Some(home));
            }
        }
    }

    let start = context.errors.len().saturating_sub(MAX_ERROR_ENTRIES);
    let errors = context.errors[start..]
        .iter()
        .filter_map(|entry| {
            let message = scrub_message(&entry.message, home);
            (!message.is_empty()).then_some(DiagnosticEntry {
                level: entry.level,
                message,
            })
        })
        .collect();

    DiagnosticsReport {
        generated_at: format_timestamp(SystemTime::now()),
        sections,
        errors,
    }
}

/// Filename offered by the file chooser, for example
/// `oxidezap-diagnostics-2026-11-07T12-34-56Z.txt`.
pub fn report_file_name(now: SystemTime) -> String {
    format!(
        "oxidezap-diagnostics-{}.txt",
        format_timestamp(now).replace(':', "-")
    )
}

/// Adds the `.txt` extension when the user typed a name without one.
pub fn report_path(path: PathBuf) -> PathBuf {
    let is_text = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"));
    if is_text {
        path
    } else {
        path.with_extension("txt")
    }
}

/// Shortens a path under the user's home directory to `~`.
pub fn redact_home_path(path: &Path, home: Option<&Path>) -> String {
    let display = path.display().to_string();
    let Some(home) = home else {
        return display;
    };
    let home = home.display().to_string();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return display;
    }
    if display.trim_end_matches('/') == home {
        return "~".to_string();
    }
    match display.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => display,
    }
}

/// Replaces the home directory string wherever it appears in free-form text.
fn redact_home_text(value: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return value.to_string();
    };
    let home = home.display().to_string();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return value.to_string();
    }
    value.replace(home, "~")
}

/// The current home directory, used for `~` redaction and message scrubbing.
pub fn current_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && !path.as_os_str().is_empty())
}

fn row(label: &'static str, value: impl Into<String>) -> ReportRow {
    ReportRow {
        label,
        value: single_line(&value.into()),
    }
}

fn settings_rows(context: &DiagnosticsContext, home: Option<&Path>) -> Vec<ReportRow> {
    let settings = &context.settings;
    let proxy = settings.network.proxy.redacted_summary();
    let downloads = match settings.downloads_directory.as_deref() {
        Some(directory) => redact_home_path(Path::new(directory), home),
        None => format!(
            "{} (default)",
            redact_home_path(&context.paths.downloads_dir, home)
        ),
    };
    let writing = if settings.writing.spell_checking_enabled() {
        format!("spell check on ({})", settings.writing.language.label())
    } else {
        "spell check off".to_string()
    };
    let memory = match settings.memory.suspend_after() {
        Some(delay) => format!("release hidden accounts after {} min", delay.as_secs() / 60),
        None => "off".to_string(),
    };
    let last_cleanup = settings
        .last_storage_cleanup
        .clone()
        .unwrap_or_else(|| "never".to_string());

    vec![
        row("Theme", settings.theme.label().to_string()),
        row("Zoom", format!("{:.0}%", settings.zoom * 100.0)),
        row("Desktop notifications", on_off(settings.notifications)),
        row(
            "Connection notifications",
            on_off(settings.connection_notifications),
        ),
        row("Do not disturb", settings.do_not_disturb.summary()),
        row("Close to tray", on_off(settings.close_to_tray)),
        row("Launch at login", on_off(settings.start_at_login)),
        row(
            "Automatic update checks",
            on_off(settings.automatic_update_checks),
        ),
        row("Remember window bounds", on_off(settings.remember_bounds)),
        row("Media action menu", on_off(settings.media_actions)),
        row(
            "Experimental CSS",
            on_off(settings.appearance.experimental_css),
        ),
        row("Writing", writing),
        row(
            "App lock",
            if settings.lock.enabled && settings.lock.hash.is_some() {
                "enabled"
            } else {
                "disabled"
            }
            .to_string(),
        ),
        row("Proxy", proxy),
        row("Low-memory mode", memory),
        row("Downloads folder", downloads),
        row(
            "Download layout",
            format!(
                "per-account folders: {}, date folders: {}",
                on_off(settings.downloads_account_folders),
                on_off(settings.downloads_date_folders)
            ),
        ),
        row("Last storage cleanup", last_cleanup),
    ]
}

fn runtime_rows(runtime: &RuntimeSummary) -> Vec<ReportRow> {
    let crash = if runtime.crash_tracking == 0 {
        "idle".to_string()
    } else {
        format!(
            "{} in recovery, {} failed, {} attempts used",
            runtime.crash_tracking, runtime.crash_failed, runtime.crash_attempts
        )
    };
    vec![
        row(
            "WebKit views",
            format!(
                "{} loaded, {} suspended, {} resuming",
                runtime.loaded_views, runtime.suspended_views, runtime.resuming_views
            ),
        ),
        row(
            "Connections",
            format!(
                "{} connected, {} offline, {} reconnecting, {} connecting",
                runtime.connected, runtime.offline, runtime.reconnecting, runtime.connecting
            ),
        ),
        row("Crash recovery", crash),
        row(
            "Downloads in progress",
            runtime.active_downloads.to_string(),
        ),
        row(
            "Application lock",
            if runtime.locked { "locked" } else { "unlocked" },
        ),
    ]
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn env_flag(name: &str) -> &'static str {
    if std::env::var_os(name).is_some() {
        "set"
    } else {
        "not set"
    }
}

fn session_type() -> String {
    match std::env::var("XDG_SESSION_TYPE") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "wayland" => "Wayland".to_string(),
            "x11" => "X11".to_string(),
            "tty" => "TTY".to_string(),
            "" => "unknown".to_string(),
            other => format!("other ({other})"),
        },
        Err(_) => {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                "Wayland".to_string()
            } else if std::env::var_os("DISPLAY").is_some() {
                "X11".to_string()
            } else {
                "unknown".to_string()
            }
        }
    }
}

fn desktop_name() -> String {
    for name in [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ] {
        if let Ok(value) = std::env::var(name) {
            let value = single_line(value.trim());
            if !value.is_empty() {
                return value;
            }
        }
    }
    "unknown".to_string()
}

fn gdk_backend() -> String {
    match std::env::var("GDK_BACKEND") {
        Ok(value) => {
            let value = single_line(value.trim());
            if value.is_empty() {
                "automatic".to_string()
            } else {
                value
            }
        }
        Err(_) => "automatic".to_string(),
    }
}

fn webkit_env_names() -> String {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| key.starts_with("WEBKIT_"))
        .collect();
    names.sort();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn gtk_version() -> String {
    format!(
        "{}.{}.{}",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version()
    )
}

fn webkit_version() -> String {
    // SAFETY: the WebKitGTK version getters only return library constants and
    // do not require GTK or WebKit to be initialized on the calling thread.
    let (major, minor, micro) = unsafe {
        (
            webkit2gtk::ffi::webkit_get_major_version(),
            webkit2gtk::ffi::webkit_get_minor_version(),
            webkit2gtk::ffi::webkit_get_micro_version(),
        )
    };
    format!("{major}.{minor}.{micro}")
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| parse_os_release(&contents))
        .map(|name| single_line(&name))
        .unwrap_or_else(|| format!("{} (unknown distribution)", std::env::consts::OS))
}

fn parse_os_release(contents: &str) -> Option<String> {
    let mut name = None;
    let mut version = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "PRETTY_NAME" => return Some(value),
            "NAME" => name = Some(value),
            "VERSION_ID" => version = Some(value),
            _ => {}
        }
    }
    match (name, version) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        (Some(name), None) => Some(name),
        _ => None,
    }
}

fn kernel_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|value| single_line(value.trim()))
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_string())
}

fn cpu_summary() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|contents| cpu_summary_from(&contents))
        .unwrap_or_else(|_| "unavailable".to_string())
}

fn cpu_summary_from(contents: &str) -> String {
    let mut model = None;
    let mut threads = 0;
    for line in contents.lines() {
        if line.starts_with("processor") {
            threads += 1;
        }
        if model.is_none() {
            if let Some((key, value)) = line.split_once(':') {
                if key.trim() == "model name" {
                    model = Some(value.trim().to_string());
                }
            }
        }
    }
    match model {
        Some(model) if threads > 0 => format!("{model} ({threads} threads)"),
        Some(model) => model,
        None => "unavailable".to_string(),
    }
}

fn memory_summary() -> String {
    std::fs::read_to_string("/proc/meminfo")
        .map(|contents| memory_summary_from(&contents))
        .unwrap_or_else(|_| "unavailable".to_string())
}

fn memory_summary_from(contents: &str) -> String {
    let total = contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "MemTotal" {
            return None;
        }
        value.split_whitespace().next()?.parse::<u64>().ok()
    });
    match total {
        Some(kibibytes) => format!("{:.1} GiB total", kibibytes as f64 / 1_048_576.0),
        None => "unavailable".to_string(),
    }
}

/// Flattens whitespace and control characters, then bounds the length so an
/// unusual environment value cannot inject line breaks into the report.
fn single_line(value: &str) -> String {
    let flattened: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let normalized = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, MAX_FIELD_CHARS)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let truncated: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Removes the home path, secret-like assignments, WhatsApp ids, and account
/// UUIDs from a free-form message. The result is a single bounded line.
fn scrub_message(message: &str, home: Option<&Path>) -> String {
    let flattened: String = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let mut scrubbed = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(home) = home {
        scrubbed = redact_home_text(&scrubbed, Some(home));
    }
    scrubbed = scrubbed
        .split(' ')
        .map(redact_url_token)
        .map(|token| redact_sensitive_assignment(&token))
        .collect::<Vec<_>>()
        .join(" ");
    scrubbed = redact_whatsapp_ids(&scrubbed);
    scrubbed = redact_uuids(&scrubbed);
    truncate_chars(scrubbed.trim(), MAX_ERROR_MESSAGE_CHARS)
}

fn redact_url_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let index = lower.find("https://").or_else(|| lower.find("http://"));
    let Some(index) = index else {
        return token.to_string();
    };
    let url = &token[index..];
    match url.find(['?', '#']) {
        Some(cut) => format!("{}{}?<redacted>", &token[..index], &url[..cut]),
        None => token.to_string(),
    }
}

fn redact_sensitive_assignment(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    if SENSITIVE_ASSIGNMENT_KEYS
        .iter()
        .any(|key| lower.contains(key))
    {
        "<redacted>".to_string()
    } else {
        token.to_string()
    }
}

fn redact_whatsapp_ids(value: &str) -> String {
    value
        .split(' ')
        .map(|token| {
            let lower = token.to_ascii_lowercase();
            if lower.contains("@c.us")
                || lower.contains("@s.whatsapp.net")
                || lower.contains("@g.us")
            {
                "<whatsapp-id>".to_string()
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_uuids(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = uuid_end(bytes, index) {
            result.push_str("<account-id>");
            index = end;
        } else {
            let character = value[index..]
                .chars()
                .next()
                .expect("the scan always stops on a character boundary");
            result.push(character);
            index += character.len_utf8();
        }
    }
    result
}

fn uuid_end(bytes: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(36)?;
    if end > bytes.len() {
        return None;
    }
    let mut index = start;
    for (group, length) in [8_usize, 4, 4, 4, 12].into_iter().enumerate() {
        if group > 0 {
            if bytes[index] != b'-' {
                return None;
            }
            index += 1;
        }
        for _ in 0..length {
            if !bytes[index].is_ascii_hexdigit() {
                return None;
            }
            index += 1;
        }
    }
    if start > 0 && bytes[start - 1].is_ascii_hexdigit() {
        return None;
    }
    if end < bytes.len() && bytes[end].is_ascii_hexdigit() {
        return None;
    }
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Account, ManualProxy, ProxyMode, ProxySettings};

    const ACCOUNT_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn home() -> PathBuf {
        PathBuf::from("/home/tester")
    }

    fn fixture_context() -> DiagnosticsContext {
        let mut settings = Settings {
            accounts: vec![Account::with_id(ACCOUNT_ID, "Alice Secret")],
            active_account: Some(ACCOUNT_ID.to_string()),
            ..Settings::default()
        };
        settings.lock.enabled = true;
        settings.lock.hash = Some("$argon2id$v=19$m=19456,t=2,p=1$abc$secret-hash".to_string());
        settings.appearance.experimental_css = true;
        settings.appearance.custom_css = "secret-css-marker {}".to_string();
        settings.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: ManualProxy {
                host: "proxy.example.com".to_string(),
                port: 8080,
                username: "proxy-user".to_string(),
                password: "proxy-secret".to_string(),
                ..ManualProxy::default()
            },
        };
        let fake_home = home();
        let paths = Paths {
            config_dir: fake_home.join(".config/oxidezap"),
            data_dir: fake_home.join(".local/share/oxidezap"),
            cache_dir: fake_home.join(".cache/oxidezap"),
            downloads_dir: fake_home.join("Downloads"),
            settings_file: fake_home.join(".config/oxidezap/settings.json"),
            accounts_dir: fake_home.join(".local/share/oxidezap/accounts"),
            socket_file: PathBuf::from("/run/user/1000/oxidezap-1000.sock"),
            lock_file: PathBuf::from("/run/user/1000/oxidezap-1000.lock"),
        };
        DiagnosticsContext {
            app_version: "9.9.9".to_string(),
            settings,
            paths,
            home_dir: Some(fake_home),
            runtime: RuntimeSummary {
                loaded_views: 1,
                connected: 1,
                active_downloads: 1,
                ..RuntimeSummary::default()
            },
            errors: vec![DiagnosticEntry {
                level: DiagnosticLevel::Error,
                message: format!(
                    "account {ACCOUNT_ID} at {}/.config/oxidezap chat 5491100000000@c.us password=hunter2 token=supersecrettoken",
                    home().display()
                ),
            }],
        }
    }

    fn labels(section: &ReportSection) -> Vec<&'static str> {
        section.rows.iter().map(|row| row.label).collect()
    }

    #[test]
    fn error_log_is_bounded_and_keeps_the_newest_entries() {
        let mut log = ErrorLog::default();
        for index in 0..(MAX_ERROR_ENTRIES + 5) {
            log.record(DiagnosticLevel::Warning, format!("problem {index}"));
        }

        let entries = log.to_vec();
        assert_eq!(entries.len(), MAX_ERROR_ENTRIES);
        assert_eq!(entries.first().unwrap().message, "problem 5");
        assert_eq!(
            entries.last().unwrap().message,
            format!("problem {}", MAX_ERROR_ENTRIES + 4)
        );
    }

    #[test]
    fn error_log_scrubs_whatsapp_ids_and_credentials() {
        let mut log = ErrorLog::default();
        log.record(
            DiagnosticLevel::Error,
            format!(
                "account {ACCOUNT_ID} chat 5491100000000@c.us\npassword=hunter2 token=supersecrettoken"
            ),
        );

        let message = &log.to_vec()[0].message;
        assert!(!message.contains(ACCOUNT_ID), "{message}");
        assert!(!message.contains("5491100000000@c.us"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains("supersecrettoken"), "{message}");
        assert!(!message.contains('\n'), "{message}");
        assert!(message.contains("<account-id>"), "{message}");
        assert!(message.contains("<whatsapp-id>"), "{message}");
        assert!(message.contains("<redacted>"), "{message}");
    }

    #[test]
    fn home_paths_are_shortened_to_a_tilde() {
        let home = home();
        assert_eq!(redact_home_path(&home, Some(&home)), "~");
        assert_eq!(
            redact_home_path(&home.join(".config/oxidezap"), Some(&home)),
            "~/.config/oxidezap"
        );
        assert_eq!(
            redact_home_path(Path::new("/etc/hosts"), Some(&home)),
            "/etc/hosts"
        );
        assert_eq!(
            redact_home_path(Path::new("/home/tester2/file"), Some(&home)),
            "/home/tester2/file"
        );
        assert_eq!(
            redact_home_path(&home.join(".config"), Some(Path::new("/home/tester/"))),
            "~/.config"
        );
    }

    #[test]
    fn report_has_stable_sections_and_rows() {
        let report = collect(&fixture_context());
        let titles: Vec<&'static str> = report
            .sections
            .iter()
            .map(|section| section.title)
            .collect();
        assert_eq!(
            titles,
            vec![
                "Application",
                "System",
                "Session",
                "Settings",
                "Runtime",
                "Paths"
            ]
        );
        assert_eq!(labels(&report.sections[0]), vec!["Version", "Accounts"]);
        assert_eq!(
            labels(&report.sections[1]),
            vec![
                "Distribution",
                "Kernel",
                "Architecture",
                "CPU",
                "Memory",
                "GTK",
                "WebKitGTK"
            ]
        );
        assert!(labels(&report.sections[3]).contains(&"Proxy"));
        assert!(labels(&report.sections[4]).contains(&"Crash recovery"));
        assert!(labels(&report.sections[5]).contains(&"Log file"));

        let text = report.render_text();
        assert!(text.starts_with("OxideZap diagnostics\n"));
        assert!(text.contains("\n[Recent errors]\n"));
        assert!(text.contains("Version: 9.9.9"));
    }

    #[test]
    fn report_never_contains_account_secrets_or_home_paths() {
        let report = collect(&fixture_context()).render_text();

        assert!(!report.contains("Alice Secret"), "{report}");
        assert!(!report.contains(ACCOUNT_ID), "{report}");
        assert!(!report.contains("proxy-user"), "{report}");
        assert!(!report.contains("proxy-secret"), "{report}");
        assert!(!report.contains("secret-hash"), "{report}");
        assert!(!report.contains("secret-css-marker"), "{report}");
        assert!(!report.contains("hunter2"), "{report}");
        assert!(!report.contains("supersecrettoken"), "{report}");
        assert!(!report.contains("5491100000000@c.us"), "{report}");
        assert!(!report.contains("/home/tester"), "{report}");
        assert!(report.contains("~/.config/oxidezap"), "{report}");
        assert!(report.contains("proxy.example.com:8080"), "{report}");
        assert!(report.contains("<account-id>"), "{report}");
    }

    #[test]
    fn system_values_are_parsed_and_flattened() {
        assert_eq!(
            parse_os_release(
                "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n"
            ),
            Some("Ubuntu 24.04 LTS".to_string())
        );
        assert_eq!(
            parse_os_release("NAME=Fedora\nVERSION_ID=42\n"),
            Some("Fedora 42".to_string())
        );
        assert_eq!(
            cpu_summary_from("processor\t: 0\nmodel name\t: Example CPU\nprocessor\t: 1\n"),
            "Example CPU (2 threads)"
        );
        assert_eq!(
            memory_summary_from("MemTotal:       16384000 kB\nMemFree:  100 kB\n"),
            "15.6 GiB total"
        );
        assert_eq!(single_line("line one\nline\ttwo"), "line one line two");
    }

    #[test]
    fn report_file_names_are_dated_and_safe() {
        let name = report_file_name(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        );
        assert_eq!(name, "oxidezap-diagnostics-2023-11-14T22-13-20Z.txt");
        assert_eq!(
            report_path(PathBuf::from("/tmp/report")),
            PathBuf::from("/tmp/report.txt")
        );
        assert_eq!(
            report_path(PathBuf::from("/tmp/report.TXT")),
            PathBuf::from("/tmp/report.TXT")
        );
    }
}
