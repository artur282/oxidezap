use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossbeam_channel::{bounded, unbounded, Sender};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

pub const SETTINGS_VERSION: u32 = 1;
/// Version of the portable `SettingsExport` file format. It is independent
/// from `SETTINGS_VERSION`, so the on-disk settings schema can keep evolving
/// without silently reinterpreted exports.
pub const SETTINGS_EXPORT_VERSION: u32 = 1;
/// Upper bound for an imported settings file. A real export is a few KiB;
/// anything beyond this is rejected before it is parsed.
pub const MAX_IMPORT_BYTES: u64 = 1024 * 1024;
pub const MIN_ZOOM: f64 = 0.8;
pub const MAX_ZOOM: f64 = 1.5;
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Shift+W";
/// How long a settings change waits before it is written, so that bursts of
/// unread/title events coalesce into a single save.
pub const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "System",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SavedBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl SavedBounds {
    pub fn is_valid(&self) -> bool {
        self.width >= 480.0 && self.height >= 360.0 && self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub bounds: Option<SavedBounds>,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default = "notification_sound_default")]
    pub notification_sound: bool,
}

fn notification_sound_default() -> bool {
    true
}

impl Account {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            unread: 0,
            bounds: None,
            maximized: false,
            muted: false,
            notification_sound: true,
        }
    }

    pub fn with_id(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            unread: 0,
            bounds: None,
            maximized: false,
            muted: false,
            notification_sound: true,
        }
    }

    pub fn initial(&self) -> String {
        self.name
            .chars()
            .find(|ch| !ch.is_whitespace())
            .map(|ch| ch.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LockSettings {
    pub enabled: bool,
    pub hash: Option<String>,
    pub lock_on_launch: bool,
    pub lock_on_hide: bool,
    pub idle_minutes: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub accounts: Vec<Account>,
    pub active_account: Option<String>,
    pub theme: ThemeMode,
    pub zoom: f64,
    pub notifications: bool,
    pub do_not_disturb: bool,
    pub close_to_tray: bool,
    pub start_at_login: bool,
    pub automatic_update_checks: bool,
    pub skipped_update_version: Option<String>,
    pub remember_bounds: bool,
    pub shortcut: String,
    pub lock: LockSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            accounts: Vec::new(),
            active_account: None,
            theme: ThemeMode::System,
            zoom: 1.0,
            notifications: true,
            do_not_disturb: false,
            close_to_tray: true,
            start_at_login: false,
            automatic_update_checks: true,
            skipped_update_version: None,
            remember_bounds: true,
            shortcut: DEFAULT_SHORTCUT.to_string(),
            lock: LockSettings::default(),
        }
    }
}

impl Settings {
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(settings) => settings,
            Err(error) => {
                if path.exists() {
                    let backup = path.with_extension("json.broken");
                    match fs::read(path) {
                        Ok(data) => {
                            if let Err(backup_error) =
                                crate::paths::write_private_file(&backup, &data)
                            {
                                warn!(
                                    %backup_error,
                                    "could not write the settings backup"
                                );
                            }
                        }
                        Err(backup_error) => {
                            warn!(%backup_error, "could not read the settings file for backup");
                        }
                    }
                    warn!(
                        path = %path.display(),
                        backup = %backup.display(),
                        %error,
                        "could not read settings; starting from defaults"
                    );
                }
                let mut settings = Self::default();
                settings.ensure_first_account();
                settings
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        let mut settings: Self = serde_json::from_slice(&data)
            .with_context(|| format!("could not parse {}", path.display()))?;
        settings.normalize();
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::paths::create_private_dir(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let data = serde_json::to_vec_pretty(self).context("could not serialize settings")?;
        let tmp = path.with_extension("json.tmp");
        crate::paths::write_private_file(&tmp, &data)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
        crate::paths::restrict_file(path)
            .with_context(|| format!("could not restrict {}", path.display()))?;
        info!(path = %path.display(), "settings saved");
        Ok(())
    }

    pub fn normalize(&mut self) {
        self.version = SETTINGS_VERSION;
        self.zoom = clamp_zoom(self.zoom);
        if self.shortcut.trim().is_empty() {
            self.shortcut = DEFAULT_SHORTCUT.to_string();
        }
        self.lock.idle_minutes = self.lock.idle_minutes.min(240);
        if self.lock.hash.is_none() {
            self.lock.enabled = false;
        }
        self.ensure_first_account();
        if self
            .active_account
            .as_ref()
            .is_none_or(|id| self.account(id).is_none())
        {
            self.active_account = self.accounts.first().map(|account| account.id.clone());
        }
    }

    pub fn ensure_first_account(&mut self) -> bool {
        if self.accounts.is_empty() {
            let account = Account::new("Personal");
            self.active_account = Some(account.id.clone());
            self.accounts.push(account);
            return true;
        }
        false
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|account| account.id == id)
    }

    pub fn add_account(&mut self, name: impl Into<String>) -> String {
        let account = Account::new(name);
        let id = account.id.clone();
        self.accounts.push(account);
        id
    }

    pub fn remove_account(&mut self, id: &str) -> Option<Account> {
        let index = self.accounts.iter().position(|account| account.id == id)?;
        let removed = self.accounts.remove(index);
        if self.active_account.as_deref() == Some(id) {
            self.active_account = self.accounts.first().map(|account| account.id.clone());
        }
        Some(removed)
    }

    pub fn rename_account(&mut self, id: &str, name: impl Into<String>) -> bool {
        let Some(account) = self.account_mut(id) else {
            return false;
        };
        let name = name.into();
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        account.name = name.to_string();
        true
    }

    pub fn set_unread(&mut self, id: &str, unread: u32) -> bool {
        let Some(account) = self.account_mut(id) else {
            return false;
        };
        if account.unread == unread {
            return false;
        }
        account.unread = unread;
        true
    }

    pub fn total_unread(&self) -> u32 {
        self.accounts.iter().map(|account| account.unread).sum()
    }

    pub fn set_muted(&mut self, id: &str, muted: bool) -> bool {
        let Some(account) = self.account_mut(id) else {
            return false;
        };
        if account.muted == muted {
            return false;
        }
        account.muted = muted;
        true
    }

    pub fn set_notification_sound(&mut self, id: &str, enabled: bool) -> bool {
        let Some(account) = self.account_mut(id) else {
            return false;
        };
        if account.notification_sound == enabled {
            return false;
        }
        account.notification_sound = enabled;
        true
    }

    pub fn set_do_not_disturb(&mut self, enabled: bool) -> bool {
        if self.do_not_disturb == enabled {
            return false;
        }
        self.do_not_disturb = enabled;
        true
    }

    /// A muted account or the global do-not-disturb mode suppresses message
    /// notifications. Download, crash, and error notifications are unaffected.
    pub fn message_notifications_enabled(&self, account_id: &str) -> bool {
        self.notifications
            && !self.do_not_disturb
            && self
                .account(account_id)
                .is_some_and(|account| !account.muted)
    }

    pub fn notification_sound_enabled(&self, account_id: &str) -> bool {
        self.message_notifications_enabled(account_id)
            && self
                .account(account_id)
                .is_some_and(|account| account.notification_sound)
    }

    pub fn set_zoom(&mut self, zoom: f64) -> bool {
        let zoom = clamp_zoom(zoom);
        if (self.zoom - zoom).abs() < f64::EPSILON {
            return false;
        }
        self.zoom = zoom;
        true
    }

    pub fn set_theme(&mut self, theme: ThemeMode) -> bool {
        if self.theme == theme {
            return false;
        }
        self.theme = theme;
        true
    }

    pub fn first_account_id(&self) -> Option<String> {
        self.accounts.first().map(|account| account.id.clone())
    }

    pub fn active_account_id(&self) -> Option<String> {
        self.active_account
            .clone()
            .or_else(|| self.first_account_id())
    }
}

pub fn clamp_zoom(zoom: f64) -> f64 {
    if !zoom.is_finite() {
        return 1.0;
    }
    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Portable settings document written by "Export settings" and read by
/// "Import settings". It contains only preferences: account sessions, cookies,
/// chat data, runtime state, and the app-lock password hash are never written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsExport {
    pub version: u32,
    pub exported_at: String,
    pub settings: Settings,
}

impl SettingsExport {
    /// Builds an export from live settings. The lock password hash is removed,
    /// the app lock is disabled, and per-account runtime state (unread counts
    /// and window bounds) is reset so the result is portable and free of
    /// secrets.
    pub fn new(settings: &Settings, now: SystemTime) -> Self {
        Self {
            version: SETTINGS_EXPORT_VERSION,
            exported_at: format_timestamp(now),
            settings: exportable_settings(settings),
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("could not serialize the settings export")
    }

    /// Parses and validates an export. Unknown fields are tolerated, the
    /// version must match exactly, oversized input is rejected before parsing,
    /// and any lock password hash in the file is discarded.
    pub fn from_bytes(bytes: &[u8]) -> Result<Settings> {
        if bytes.len() as u64 > MAX_IMPORT_BYTES {
            bail!(
                "settings export is too large ({} bytes; the limit is {} bytes)",
                bytes.len(),
                MAX_IMPORT_BYTES
            );
        }
        let export: Self = serde_json::from_slice(bytes)
            .context("the file is not a valid OxideZap settings export")?;
        export.into_settings()
    }

    fn into_settings(self) -> Result<Settings> {
        if self.version != SETTINGS_EXPORT_VERSION {
            bail!(
                "unsupported settings export version {}; this build supports version {}",
                self.version,
                SETTINGS_EXPORT_VERSION
            );
        }
        let mut settings = self.settings;
        scrub_lock_password(&mut settings);
        settings.normalize();
        Ok(settings)
    }
}

/// Removes every trace of the app-lock password. Exports never carry it and
/// imports never accept one from a file.
fn scrub_lock_password(settings: &mut Settings) {
    settings.lock.hash = None;
    settings.lock.enabled = false;
    settings.lock.lock_on_launch = false;
    settings.lock.lock_on_hide = false;
    settings.lock.idle_minutes = 0;
}

fn exportable_settings(settings: &Settings) -> Settings {
    let mut settings = settings.clone();
    scrub_lock_password(&mut settings);
    for account in &mut settings.accounts {
        account.unread = 0;
        account.bounds = None;
        account.maximized = false;
    }
    settings.normalize();
    settings
}

/// A validated, normalized settings file ready to be applied, together with a
/// human-readable list of the preferences that would change.
#[derive(Clone, Debug)]
pub struct ImportPreview {
    pub settings: Settings,
    pub changes: Vec<String>,
}

impl ImportPreview {
    pub fn from_bytes(bytes: &[u8], current: &Settings) -> Result<Self> {
        let settings = SettingsExport::from_bytes(bytes)?;
        let changes = describe_changes(current, &settings);
        Ok(Self { settings, changes })
    }

    /// Reads an export from disk, rejecting files larger than
    /// `MAX_IMPORT_BYTES` before they are allocated or parsed.
    pub fn from_path(path: &Path, current: &Settings) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("could not inspect {}", path.display()))?;
        if metadata.len() > MAX_IMPORT_BYTES {
            bail!(
                "settings export is too large ({} bytes; the limit is {} bytes)",
                metadata.len(),
                MAX_IMPORT_BYTES
            );
        }
        let data = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        Self::from_bytes(&data, current)
    }
}

fn describe_changes(current: &Settings, imported: &Settings) -> Vec<String> {
    let mut changes = Vec::new();
    if current.theme != imported.theme {
        changes.push(format!(
            "Theme: {} -> {}",
            current.theme.label(),
            imported.theme.label()
        ));
    }
    if (current.zoom - imported.zoom).abs() >= 0.001 {
        changes.push(format!(
            "Zoom: {}% -> {}%",
            percent(current.zoom),
            percent(imported.zoom)
        ));
    }
    if current.notifications != imported.notifications {
        changes.push(format!(
            "Desktop notifications: {}",
            on_off(imported.notifications)
        ));
    }
    if current.do_not_disturb != imported.do_not_disturb {
        changes.push(format!(
            "Do not disturb: {}",
            on_off(imported.do_not_disturb)
        ));
    }
    if current.close_to_tray != imported.close_to_tray {
        changes.push(format!("Close to tray: {}", on_off(imported.close_to_tray)));
    }
    if current.start_at_login != imported.start_at_login {
        changes.push(format!(
            "Launch at login: {}",
            on_off(imported.start_at_login)
        ));
    }
    if current.remember_bounds != imported.remember_bounds {
        changes.push(format!(
            "Remember window bounds: {}",
            on_off(imported.remember_bounds)
        ));
    }
    if current.shortcut != imported.shortcut {
        changes.push(format!(
            "Global shortcut: {} -> {}",
            current.shortcut, imported.shortcut
        ));
    }
    let current_accounts = account_names(current);
    let imported_accounts = account_names(imported);
    if current_accounts != imported_accounts {
        changes.push(format!(
            "Accounts: {} -> {}",
            account_list(&current_accounts),
            account_list(&imported_accounts)
        ));
    }
    if changes.is_empty() {
        changes.push("No preference changes.".to_string());
    }
    changes
}

fn percent(zoom: f64) -> u32 {
    (zoom * 100.0).round() as u32
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn account_names(settings: &Settings) -> Vec<String> {
    settings
        .accounts
        .iter()
        .map(|account| account.name.clone())
        .collect()
}

fn account_list(names: &[String]) -> String {
    if names.is_empty() {
        return "(none)".to_string();
    }
    names.join(", ")
}

/// Writes the full current settings (including the lock password hash) to a
/// dated, private backup file. Backups exist so a destructive import or reset
/// can be undone by restoring the file. The parent directory is created as
/// `0700` and the file is written as `0600` on Unix.
pub fn write_settings_backup(dir: &Path, settings: &Settings, now: SystemTime) -> Result<PathBuf> {
    crate::paths::create_private_dir(dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let stamp = format_timestamp(now).replace(':', "-");
    let data =
        serde_json::to_vec_pretty(settings).context("could not serialize the settings backup")?;
    for attempt in 0..100 {
        let name = if attempt == 0 {
            format!("settings-{stamp}.json")
        } else {
            format!("settings-{stamp}-{attempt}.json")
        };
        let path = dir.join(name);
        if path.exists() {
            continue;
        }
        crate::paths::write_private_file(&path, &data)
            .with_context(|| format!("could not write {}", path.display()))?;
        return Ok(path);
    }
    bail!("could not choose a unique name for the settings backup")
}

/// Filename offered by the export file chooser, for example
/// `oxidezap-settings-2026-11-07T12-34-56Z.json`.
pub fn export_file_name(now: SystemTime) -> String {
    format!(
        "oxidezap-settings-{}.json",
        format_timestamp(now).replace(':', "-")
    )
}

/// Adds the `.json` extension when the user typed a name without one.
pub fn json_path(path: PathBuf) -> PathBuf {
    let is_json = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    if is_json {
        path
    } else {
        path.with_extension("json")
    }
}

/// Formats a `SystemTime` as an RFC 3339 UTC timestamp with second precision.
/// The conversion is pure so it can be tested without a clock.
pub fn format_timestamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts days since the Unix epoch to a proleptic Gregorian date.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

/// Coalesces many settings changes into one debounced write.
///
/// The caller marks the state dirty, starts one timer when `mark_dirty`
/// returns a generation, and only writes when that generation is reported by
/// `timer_fired`. Later changes reuse the running timer instead of scheduling
/// another one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveDebounce {
    dirty: bool,
    scheduled: bool,
    generation: u64,
}

impl SaveDebounce {
    /// Marks the settings dirty. Returns the generation to pass to
    /// `timer_fired` when a new timer must be started, or `None` when a timer
    /// is already pending.
    pub fn mark_dirty(&mut self) -> Option<u64> {
        self.dirty = true;
        if self.scheduled {
            return None;
        }
        self.scheduled = true;
        self.generation = self.generation.wrapping_add(1);
        Some(self.generation)
    }

    /// Reports whether the timer with this generation should write. A stale
    /// timer never consumes a newer dirty state.
    pub fn timer_fired(&mut self, generation: u64) -> bool {
        if !self.scheduled || generation != self.generation {
            return false;
        }
        self.scheduled = false;
        std::mem::take(&mut self.dirty)
    }

    /// Cancels a pending timer, for example when the process is quitting and
    /// the settings are written synchronously.
    pub fn clear(&mut self) {
        self.dirty = false;
        self.scheduled = false;
    }
}

/// Serializes settings writes on a worker thread so JSON serialization and
/// disk I/O never run on the UI thread. A single worker also keeps writes
/// ordered: the last snapshot received is the last one written.
pub struct SettingsWriter {
    tx: Sender<WriteRequest>,
}

struct WriteRequest {
    settings: Settings,
    path: PathBuf,
    done: Option<Sender<()>>,
}

impl SettingsWriter {
    pub fn start() -> Self {
        let (tx, rx) = unbounded::<WriteRequest>();
        let thread = std::thread::Builder::new()
            .name("oxidezap-settings".to_string())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    if let Err(error) = request.settings.save(&request.path) {
                        warn!(%error, "could not save settings");
                    }
                    if let Some(done) = request.done {
                        let _ = done.send(());
                    }
                }
            });
        if let Err(error) = thread {
            warn!(%error, "could not start the settings writer thread");
        }
        Self { tx }
    }

    /// Queues a save without blocking. The single worker writes snapshots in
    /// the order they were queued, so the newest state always wins.
    pub fn save(&self, settings: Settings, path: PathBuf) {
        let request = WriteRequest {
            settings,
            path,
            done: None,
        };
        if let Err(error) = self.tx.send(request) {
            let request = error.0;
            let _ = request.settings.save(&request.path);
        }
    }

    /// Queues the final save and waits for the worker to finish it. Used when
    /// quitting so the process does not exit before the last state is stored.
    pub fn save_and_wait(&self, settings: Settings, path: PathBuf) {
        let (done_tx, done_rx) = bounded(1);
        let request = WriteRequest {
            settings,
            path,
            done: Some(done_tx),
        };
        if let Err(error) = self.tx.send(request) {
            let request = error.0;
            let _ = request.settings.save(&request.path);
            return;
        }
        if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            warn!("settings writer did not confirm the final save");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let settings = Settings::default();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(settings.zoom, 1.0);
        assert!(settings.notifications);
        assert!(!settings.do_not_disturb);
        assert!(settings.close_to_tray);
        assert!(!settings.lock.enabled);
    }

    #[test]
    fn notification_policy_respects_mute_sound_and_do_not_disturb() {
        let mut settings = Settings::default();
        settings.ensure_first_account();
        let id = settings.accounts[0].id.clone();

        assert!(settings.message_notifications_enabled(&id));
        assert!(settings.notification_sound_enabled(&id));

        assert!(settings.set_muted(&id, true));
        assert!(!settings.message_notifications_enabled(&id));
        assert!(!settings.notification_sound_enabled(&id));
        assert!(!settings.set_muted(&id, true));

        assert!(settings.set_muted(&id, false));
        assert!(settings.set_notification_sound(&id, false));
        assert!(settings.message_notifications_enabled(&id));
        assert!(!settings.notification_sound_enabled(&id));

        assert!(settings.set_notification_sound(&id, true));
        assert!(settings.set_do_not_disturb(true));
        assert!(!settings.message_notifications_enabled(&id));
        assert!(!settings.notification_sound_enabled(&id));
        assert!(!settings.set_do_not_disturb(true));
        assert!(settings.set_do_not_disturb(false));

        settings.notifications = false;
        assert!(!settings.message_notifications_enabled(&id));
        assert!(!settings.notification_sound_enabled(&id));

        assert!(!settings.message_notifications_enabled("missing"));
        assert!(!settings.notification_sound_enabled("missing"));
    }

    #[test]
    fn muting_one_account_keeps_the_others_active() {
        let mut settings = Settings {
            accounts: vec![Account::with_id("a", "A"), Account::with_id("b", "B")],
            ..Settings::default()
        };

        settings.set_muted("a", true);

        assert!(!settings.message_notifications_enabled("a"));
        assert!(settings.message_notifications_enabled("b"));
        assert!(settings.notification_sound_enabled("b"));
    }

    #[test]
    fn legacy_settings_default_to_audible_unmuted_accounts() {
        let json = br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a"}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert!(!settings.do_not_disturb);
        assert!(!settings.accounts[0].muted);
        assert!(settings.accounts[0].notification_sound);
    }

    #[test]
    fn update_checks_are_enabled_by_default() {
        let settings = Settings::default();
        assert!(settings.automatic_update_checks);
        assert!(settings.skipped_update_version.is_none());
    }

    #[test]
    fn legacy_settings_enable_update_checks() {
        let json = br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a"}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert!(settings.automatic_update_checks);
        assert!(settings.skipped_update_version.is_none());
    }

    #[test]
    fn update_preferences_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.ensure_first_account();
        settings.automatic_update_checks = false;
        settings.skipped_update_version = Some("0.2.0".to_string());

        settings.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();

        assert_eq!(loaded, settings);
        assert!(!loaded.automatic_update_checks);
        assert_eq!(loaded.skipped_update_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn first_account_is_created_and_active() {
        let mut settings = Settings::default();
        assert!(settings.ensure_first_account());
        assert_eq!(settings.accounts.len(), 1);
        assert_eq!(
            settings.active_account.as_deref(),
            Some(settings.accounts[0].id.as_str())
        );
        assert!(!settings.ensure_first_account());
    }

    #[test]
    fn zoom_is_clamped() {
        let mut settings = Settings::default();
        assert!(settings.set_zoom(0.1));
        assert_eq!(settings.zoom, MIN_ZOOM);
        assert!(settings.set_zoom(9.0));
        assert_eq!(settings.zoom, MAX_ZOOM);
        assert!(!settings.set_zoom(MAX_ZOOM));
    }

    #[test]
    fn unread_counts_are_tracked() {
        let mut settings = Settings::default();
        settings.ensure_first_account();
        let id = settings.accounts[0].id.clone();
        assert!(settings.set_unread(&id, 3));
        assert_eq!(settings.total_unread(), 3);
        assert!(!settings.set_unread(&id, 3));
        assert!(settings.set_unread(&id, 0));
        assert_eq!(settings.total_unread(), 0);
    }

    #[test]
    fn removing_the_active_account_selects_the_next_one() {
        let mut settings = Settings {
            accounts: vec![Account::with_id("a", "A"), Account::with_id("b", "B")],
            active_account: Some("a".to_string()),
            ..Settings::default()
        };
        let removed = settings.remove_account("a").expect("removed");
        assert_eq!(removed.id, "a");
        assert_eq!(settings.active_account.as_deref(), Some("b"));
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = Settings {
            accounts: vec![Account::with_id("acc-1", "Work")],
            active_account: Some("acc-1".to_string()),
            theme: ThemeMode::Dark,
            zoom: 1.25,
            notifications: false,
            lock: LockSettings {
                enabled: true,
                hash: Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def".to_string()),
                ..LockSettings::default()
            },
            ..Settings::default()
        };

        settings.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded, settings);
    }

    #[test]
    fn broken_settings_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{not json").unwrap();
        let settings = Settings::load_or_default(&path);
        assert_eq!(settings.accounts.len(), 1);
        assert!(path.with_extension("json.broken").exists());
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn saved_settings_are_private() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("oxidezap/settings.json");
            Settings::default().save(&path).unwrap();

            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert!(!path.with_extension("json.tmp").exists());
        }

        #[test]
        fn saving_corrects_existing_permissions() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            fs::write(&path, b"{}").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

            Settings::default().save(&path).unwrap();

            assert_eq!(mode(&path), 0o600);
        }

        #[test]
        fn broken_settings_backup_is_private() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            fs::write(&path, b"{not json").unwrap();
            Settings::load_or_default(&path);

            let backup = path.with_extension("json.broken");
            assert_eq!(mode(&backup), 0o600);
        }

        #[test]
        fn saving_replaces_a_symlinked_settings_file_without_touching_the_target() {
            let dir = tempfile::tempdir().unwrap();
            let victim = dir.path().join("victim.json");
            fs::write(&victim, b"do not touch").unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
            let path = dir.path().join("settings.json");
            std::os::unix::fs::symlink(&victim, &path).unwrap();

            Settings::default().save(&path).unwrap();

            assert_eq!(mode(&path), 0o600);
            assert!(!fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(mode(&victim), 0o644);
            assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        }
    }

    #[test]
    fn lock_is_disabled_without_a_hash() {
        let mut settings = Settings::default();
        settings.lock.enabled = true;
        settings.normalize();
        assert!(!settings.lock.enabled);
    }

    #[test]
    fn account_initials_are_derived_from_the_name() {
        assert_eq!(Account::with_id("a", "  work").initial(), "W");
        assert_eq!(Account::with_id("a", "").initial(), "?");
    }

    #[test]
    fn debounce_coalesces_rapid_changes() {
        let mut debounce = SaveDebounce::default();
        let generation = debounce.mark_dirty().expect("a timer is scheduled");
        assert_eq!(debounce.mark_dirty(), None);
        assert_eq!(debounce.mark_dirty(), None);
        assert!(debounce.timer_fired(generation));
        assert!(!debounce.timer_fired(generation));
    }

    #[test]
    fn debounce_schedules_again_after_a_write() {
        let mut debounce = SaveDebounce::default();
        let first = debounce.mark_dirty().expect("a timer is scheduled");
        assert!(debounce.timer_fired(first));
        let second = debounce.mark_dirty().expect("a new timer is scheduled");
        assert_ne!(first, second);
        assert!(debounce.timer_fired(second));
    }

    #[test]
    fn a_stale_timer_does_not_consume_a_newer_change() {
        let mut debounce = SaveDebounce::default();
        let first = debounce.mark_dirty().expect("a timer is scheduled");
        assert!(debounce.timer_fired(first));
        let second = debounce.mark_dirty().expect("a new timer is scheduled");
        assert!(!debounce.timer_fired(first));
        assert!(debounce.timer_fired(second));
    }

    #[test]
    fn clearing_the_debounce_discards_the_pending_write() {
        let mut debounce = SaveDebounce::default();
        let generation = debounce.mark_dirty().expect("a timer is scheduled");
        debounce.clear();
        assert!(!debounce.timer_fired(generation));
    }

    #[test]
    fn settings_writer_persists_the_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let writer = SettingsWriter::start();

        for zoom in [0.8, 0.9, 1.1, 1.2, 1.3, 1.4, 1.5] {
            let mut settings = Settings {
                accounts: vec![Account::with_id("acc-1", "Work")],
                active_account: Some("acc-1".to_string()),
                ..Settings::default()
            };
            settings.set_zoom(zoom);
            writer.save(settings, path.clone());
        }

        let mut expected = Settings {
            accounts: vec![Account::with_id("acc-1", "Work")],
            active_account: Some("acc-1".to_string()),
            theme: ThemeMode::Dark,
            ..Settings::default()
        };
        expected.set_zoom(1.25);
        writer.save_and_wait(expected.clone(), path.clone());

        assert_eq!(Settings::load(&path).unwrap(), expected);
    }

    fn exported_fixture() -> Settings {
        let mut account = Account::with_id("acc-1", "Work");
        account.unread = 7;
        account.bounds = Some(SavedBounds {
            x: 10.0,
            y: 20.0,
            width: 1280.0,
            height: 800.0,
        });
        account.maximized = true;
        Settings {
            accounts: vec![account],
            active_account: Some("acc-1".to_string()),
            theme: ThemeMode::Dark,
            zoom: 1.25,
            notifications: false,
            do_not_disturb: true,
            close_to_tray: false,
            start_at_login: true,
            remember_bounds: false,
            shortcut: "Ctrl+Alt+O".to_string(),
            lock: LockSettings {
                enabled: true,
                hash: Some("$argon2id$v=19$m=19456,t=2,p=1$secret$hash".to_string()),
                lock_on_launch: true,
                lock_on_hide: true,
                idle_minutes: 15,
            },
            ..Settings::default()
        }
    }

    #[test]
    fn timestamps_are_formatted_as_utc_rfc3339() {
        assert_eq!(
            format_timestamp(UNIX_EPOCH),
            "1970-01-01T00:00:00Z".to_string()
        );
        assert_eq!(
            format_timestamp(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z".to_string()
        );
    }

    #[test]
    fn export_round_trips_preferences() {
        let settings = exported_fixture();
        let export =
            SettingsExport::new(&settings, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let json = export.to_json().unwrap();

        assert_eq!(export.exported_at, "2023-11-14T22:13:20Z");
        assert_eq!(export.version, SETTINGS_EXPORT_VERSION);
        assert!(json.contains("\"version\": 1"));
        assert!(json.contains("\"exported_at\": \"2023-11-14T22:13:20Z\""));
        assert!(json.contains("\"settings\""));

        let restored = SettingsExport::from_bytes(json.as_bytes()).unwrap();
        assert_eq!(restored.theme, ThemeMode::Dark);
        assert_eq!(restored.zoom, 1.25);
        assert!(!restored.notifications);
        assert!(restored.do_not_disturb);
        assert!(!restored.close_to_tray);
        assert!(restored.start_at_login);
        assert!(!restored.remember_bounds);
        assert_eq!(restored.shortcut, "Ctrl+Alt+O");
        assert_eq!(restored.accounts.len(), 1);
        assert_eq!(restored.accounts[0].name, "Work");
    }

    #[test]
    fn export_excludes_the_lock_password_and_runtime_state() {
        let settings = exported_fixture();
        let hash = settings.lock.hash.clone().unwrap();
        let export = SettingsExport::new(&settings, UNIX_EPOCH);
        let json = export.to_json().unwrap();

        assert!(
            !json.contains(&hash),
            "the export must not contain the hash"
        );
        assert!(
            !json.contains("argon2"),
            "the export must not contain hash data"
        );
        assert!(
            !json.contains("unread\": 7"),
            "unread counts are runtime state"
        );
        assert!(!json.contains("1280.0"), "window bounds are runtime state");

        let restored = SettingsExport::from_bytes(json.as_bytes()).unwrap();
        assert!(restored.lock.hash.is_none());
        assert!(!restored.lock.enabled);
        assert_eq!(restored.accounts[0].unread, 0);
        assert!(restored.accounts[0].bounds.is_none());
        assert!(!restored.accounts[0].maximized);
    }

    #[test]
    fn import_discards_a_lock_password_found_in_a_file() {
        let mut settings = exported_fixture();
        settings.lock.hash = None;
        let mut export = SettingsExport::new(&settings, UNIX_EPOCH);
        export.settings.lock.hash = Some("$argon2id$smuggled".to_string());
        export.settings.lock.enabled = true;
        export.settings.lock.lock_on_launch = true;
        let json = serde_json::to_vec(&export).unwrap();

        let restored = SettingsExport::from_bytes(&json).unwrap();
        assert!(restored.lock.hash.is_none());
        assert!(!restored.lock.enabled);
        assert!(!restored.lock.lock_on_launch);
        assert_eq!(restored.lock.idle_minutes, 0);
    }

    #[test]
    fn import_tolerates_unknown_fields() {
        let json = br#"{
            "version": 1,
            "exported_at": "2026-01-01T00:00:00Z",
            "future_top_level": {"anything": true},
            "settings": {
                "version": 1,
                "theme": "light",
                "languages": ["en"],
                "accounts": [{"id": "a", "name": "A", "unknown_account": 3}]
            }
        }"#;

        let settings = SettingsExport::from_bytes(json).unwrap();
        assert_eq!(settings.theme, ThemeMode::Light);
        assert_eq!(settings.accounts[0].name, "A");
        assert_eq!(settings.accounts[0].id, "a");
    }

    #[test]
    fn import_rejects_invalid_json() {
        let error = SettingsExport::from_bytes(b"{not json").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not a valid OxideZap settings export"),
            "{error}"
        );
    }

    #[test]
    fn import_rejects_an_unknown_version() {
        let json = br#"{"version": 2, "exported_at": "2026-01-01T00:00:00Z", "settings": {}}"#;
        let error = SettingsExport::from_bytes(json).unwrap_err();
        assert!(error.to_string().contains("version 2"), "{error}");
        assert!(error.to_string().contains("supports version 1"), "{error}");
    }

    #[test]
    fn import_rejects_a_file_that_is_too_large() {
        let bytes = vec![b' '; MAX_IMPORT_BYTES as usize + 1];
        let error = SettingsExport::from_bytes(&bytes).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.json");
        fs::write(&path, &bytes).unwrap();
        let error = ImportPreview::from_path(&path, &Settings::default()).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn import_normalizes_out_of_range_values() {
        let json = br#"{
            "version": 1,
            "exported_at": "2026-01-01T00:00:00Z",
            "settings": {
                "version": 1,
                "zoom": 9.0,
                "shortcut": "   ",
                "lock": {"enabled": true, "hash": "$argon2id$smuggled"},
                "accounts": [{"id": "a", "name": "A"}],
                "active_account": "missing"
            }
        }"#;

        let settings = SettingsExport::from_bytes(json).unwrap();
        assert_eq!(settings.zoom, MAX_ZOOM);
        assert_eq!(settings.shortcut, DEFAULT_SHORTCUT);
        assert!(settings.lock.hash.is_none());
        assert_eq!(settings.active_account.as_deref(), Some("a"));
    }

    #[test]
    fn import_preview_summarizes_the_changes() {
        let current = Settings::default();
        let import = exported_fixture();
        let json = SettingsExport::new(&import, UNIX_EPOCH).to_json().unwrap();

        let preview = ImportPreview::from_bytes(json.as_bytes(), &current).unwrap();
        assert!(preview
            .changes
            .contains(&"Theme: System -> Dark".to_string()));
        assert!(preview.changes.contains(&"Zoom: 100% -> 125%".to_string()));
        assert!(preview
            .changes
            .iter()
            .any(|change| change.starts_with("Accounts:")));
    }

    #[test]
    fn import_preview_reports_an_identical_file() {
        let current = Settings::default();
        let json = SettingsExport::new(&current, UNIX_EPOCH).to_json().unwrap();

        let preview = ImportPreview::from_bytes(json.as_bytes(), &current).unwrap();
        assert_eq!(preview.changes, vec!["No preference changes."]);
    }

    #[test]
    fn export_file_names_are_dated_and_safe() {
        let name = export_file_name(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(name, "oxidezap-settings-2023-11-14T22-13-20Z.json");
    }

    #[test]
    fn json_extension_is_added_only_when_missing() {
        assert_eq!(
            json_path(PathBuf::from("/tmp/settings")),
            PathBuf::from("/tmp/settings.json")
        );
        assert_eq!(
            json_path(PathBuf::from("/tmp/settings.JSON")),
            PathBuf::from("/tmp/settings.JSON")
        );
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_dated_private_and_never_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("backups");
        let settings = exported_fixture();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let first = write_settings_backup(&dir, &settings, now).unwrap();
        let second = write_settings_backup(&dir, &settings, now).unwrap();

        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("settings-2023-11-14T22-13-20Z"));
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let restored = Settings::load(&first).unwrap();
        assert_eq!(restored, settings);
    }
}
