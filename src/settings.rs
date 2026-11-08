use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

pub const SETTINGS_VERSION: u32 = 1;
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
            assert!(
                !fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
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
}
