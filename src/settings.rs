use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

pub const SETTINGS_VERSION: u32 = 1;
pub const MIN_ZOOM: f64 = 0.8;
pub const MAX_ZOOM: f64 = 1.5;
pub const DEFAULT_SHORTCUT: &str = "Ctrl+Shift+W";

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
}

impl Account {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            unread: 0,
            bounds: None,
            maximized: false,
        }
    }

    pub fn with_id(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            unread: 0,
            bounds: None,
            maximized: false,
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
                    let _ = fs::copy(path, &backup);
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
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let data = serde_json::to_vec_pretty(self).context("could not serialize settings")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &data).with_context(|| format!("could not write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let settings = Settings::default();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(settings.zoom, 1.0);
        assert!(settings.notifications);
        assert!(settings.close_to_tray);
        assert!(!settings.lock.enabled);
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
}
