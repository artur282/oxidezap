use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};

pub const APP_ID: &str = "oxidezap";

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub settings_file: PathBuf,
    pub accounts_dir: PathBuf,
    pub socket_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let project = ProjectDirs::from("", "", APP_ID)
            .context("could not determine the XDG config/data directories")?;
        let user_dirs = UserDirs::new();
        let home = user_dirs
            .as_ref()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .context("could not determine the home directory")?;

        let downloads_dir = user_dirs
            .as_ref()
            .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
            .unwrap_or_else(|| home.join("Downloads"));

        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        let uid = unsafe { libc::getuid() };
        let socket_file = runtime_dir.join(format!("{APP_ID}-{uid}.sock"));

        let config_dir = project.config_dir().to_path_buf();
        let data_dir = project.data_dir().to_path_buf();
        let cache_dir = project.cache_dir().to_path_buf();
        let accounts_dir = data_dir.join("accounts");
        let settings_file = config_dir.join("settings.json");

        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
            downloads_dir,
            settings_file,
            accounts_dir,
            socket_file,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.accounts_dir,
            &self.downloads_dir,
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn account_dir(&self, account_id: &str) -> PathBuf {
        self.accounts_dir.join(account_id)
    }

    pub fn account_data_dir(&self, account_id: &str) -> PathBuf {
        self.account_dir(account_id).join("data")
    }

    pub fn account_cache_dir(&self, account_id: &str) -> PathBuf {
        self.account_dir(account_id).join("cache")
    }

    pub fn runtime_icon(&self) -> PathBuf {
        self.cache_dir.join("oxidezap-128.png")
    }

    pub fn write_runtime_assets(&self) -> Result<()> {
        let icon = self.runtime_icon();
        if !icon.exists() {
            fs::write(&icon, include_bytes!("../assets/oxidezap-128.png"))
                .with_context(|| format!("could not write {}", icon.display()))?;
        }
        Ok(())
    }

    pub fn autostart_file(&self) -> PathBuf {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".config")
            });
        config_home.join("autostart").join("oxidezap.desktop")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_directories_are_isolated() {
        let paths = Paths {
            config_dir: PathBuf::from("/tmp/config"),
            data_dir: PathBuf::from("/tmp/data"),
            cache_dir: PathBuf::from("/tmp/cache"),
            downloads_dir: PathBuf::from("/tmp/downloads"),
            settings_file: PathBuf::from("/tmp/config/settings.json"),
            accounts_dir: PathBuf::from("/tmp/data/accounts"),
            socket_file: PathBuf::from("/tmp/run/oxidezap.sock"),
        };
        assert_eq!(
            paths.account_data_dir("a"),
            PathBuf::from("/tmp/data/accounts/a/data")
        );
        assert_eq!(
            paths.account_cache_dir("b"),
            PathBuf::from("/tmp/data/accounts/b/cache")
        );
        assert_ne!(paths.account_data_dir("a"), paths.account_data_dir("b"));
    }
}
