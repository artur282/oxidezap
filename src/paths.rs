use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};
use tracing::warn;

pub const APP_ID: &str = "oxidezap";

/// Mode for directories owned by OxideZap. The application never stores
/// anything in a directory that must be readable by other system users.
#[cfg(unix)]
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode for files owned by OxideZap, notably the settings file and its
/// backups, which contain the app-lock password hash.
#[cfg(unix)]
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Create `path` and any missing parents, then restrict the leaf directory to
/// `0700` on Unix. Missing parents outside the OxideZap tree are created with
/// the process umask and are never modified.
pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    restrict_directory(path)
}

/// Restrict an existing file to `0600` on Unix. Missing paths and symlinks are
/// ignored; a symlink is never followed.
pub fn restrict_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        set_private_permissions(path, PrivateTarget::File)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Restrict an existing directory to `0700` on Unix. Missing paths and
/// symlinks are ignored; a symlink is never followed.
pub fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        set_private_permissions(path, PrivateTarget::Directory)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Validates a user-supplied absolute directory setting. Relative paths and
/// paths that exist but are not directories are rejected instead of being
/// silently reinterpreted against the current working directory.
pub fn valid_directory_setting(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return None;
    }
    if path.exists() && !path.is_dir() {
        return None;
    }
    Some(path)
}

/// Write a file that must remain private. On Unix the file is created with
/// `0600`, an existing file is corrected to `0600`, and `O_NOFOLLOW` prevents
/// writing through a symlink.
pub fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut options = fs::OpenOptions::new();
        options
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options
            .open(path)
            .with_context(|| format!("could not write {}", path.display()))?;
        file.write_all(data)
            .with_context(|| format!("could not write {}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .with_context(|| format!("could not set permissions on {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data).with_context(|| format!("could not write {}", path.display()))
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateTarget {
    File,
    Directory,
}

#[cfg(unix)]
fn set_private_permissions(path: &Path, target: PrivateTarget) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let matches_target = match target {
        PrivateTarget::File => metadata.is_file(),
        PrivateTarget::Directory => metadata.is_dir(),
    };
    if !matches_target {
        return Ok(());
    }

    let mode = match target {
        PrivateTarget::File => PRIVATE_FILE_MODE,
        PrivateTarget::Directory => PRIVATE_DIR_MODE,
    };
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path);
    match opened {
        Ok(file) => file
            .set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("could not set permissions on {}", path.display()))?,
        // The path became a symlink between the check and the open. Leave it
        // alone: changing permissions through it could touch a foreign file.
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(()),
        Err(_) => fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("could not set permissions on {}", path.display()))?,
    }
    Ok(())
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub settings_file: PathBuf,
    pub accounts_dir: PathBuf,
    pub socket_file: PathBuf,
    pub lock_file: PathBuf,
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
        let lock_file = runtime_dir.join(format!("{APP_ID}-{uid}.lock"));

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
            lock_file,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.accounts_dir,
        ] {
            create_private_dir(dir)?;
        }
        // Downloads is a user directory shared with the rest of the desktop;
        // it is created but never re-chmodded by OxideZap.
        fs::create_dir_all(&self.downloads_dir)
            .with_context(|| format!("could not create {}", self.downloads_dir.display()))?;
        self.migrate_private_permissions();
        Ok(())
    }

    /// Harden installations created before issue #28. Only the paths OxideZap
    /// owns are touched, and symlinks are skipped so a foreign target is never
    /// chmodded.
    fn migrate_private_permissions(&self) {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.accounts_dir,
        ] {
            if let Err(error) = restrict_directory(dir) {
                warn!(%error, "could not restrict an application directory");
            }
        }
        if !is_symlink(&self.config_dir) {
            for file in [
                self.settings_file.clone(),
                self.settings_file.with_extension("json.tmp"),
                self.settings_file.with_extension("json.broken"),
            ] {
                if let Err(error) = restrict_file(&file) {
                    warn!(%error, "could not restrict a settings file");
                }
            }
        }
        // A linked accounts directory belongs to somebody else from our point
        // of view; scanning it would chmod a foreign tree.
        if is_symlink(&self.accounts_dir) {
            return;
        }

        let entries = match fs::read_dir(&self.accounts_dir) {
            Ok(entries) => entries,
            Err(error) => {
                warn!(%error, "could not scan the account directories");
                return;
            }
        };
        for entry in entries.flatten() {
            // `file_type` does not follow symlinks: a linked account entry is
            // skipped before its children are considered.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let account_dir = entry.path();
            for dir in [
                account_dir.clone(),
                account_dir.join("data"),
                account_dir.join("cache"),
            ] {
                if let Err(error) = restrict_directory(&dir) {
                    warn!(%error, "could not restrict an account directory");
                }
            }
        }
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

    pub fn settings_backup_dir(&self) -> PathBuf {
        self.config_dir.join("backups")
    }

    pub fn runtime_icon(&self) -> PathBuf {
        self.cache_dir.join("oxidezap-128.png")
    }

    pub fn update_cache_file(&self) -> PathBuf {
        self.cache_dir.join("update-check.json")
    }

    /// Bounded download history, stored next to the settings file so it is
    /// private and easy to back up independently from account data.
    pub fn downloads_index_file(&self) -> PathBuf {
        self.config_dir.join("downloads.json")
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
            lock_file: PathBuf::from("/tmp/run/oxidezap.lock"),
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

    #[test]
    fn update_cache_lives_in_the_cache_directory() {
        let paths = Paths {
            config_dir: PathBuf::from("/tmp/config"),
            data_dir: PathBuf::from("/tmp/data"),
            cache_dir: PathBuf::from("/tmp/cache"),
            downloads_dir: PathBuf::from("/tmp/downloads"),
            settings_file: PathBuf::from("/tmp/config/settings.json"),
            accounts_dir: PathBuf::from("/tmp/data/accounts"),
            socket_file: PathBuf::from("/tmp/run/oxidezap.sock"),
            lock_file: PathBuf::from("/tmp/run/oxidezap.lock"),
        };
        assert_eq!(
            paths.update_cache_file(),
            PathBuf::from("/tmp/cache/update-check.json")
        );
    }

    #[test]
    fn download_history_lives_in_the_config_directory() {
        let paths = Paths {
            config_dir: PathBuf::from("/tmp/config"),
            data_dir: PathBuf::from("/tmp/data"),
            cache_dir: PathBuf::from("/tmp/cache"),
            downloads_dir: PathBuf::from("/tmp/downloads"),
            settings_file: PathBuf::from("/tmp/config/settings.json"),
            accounts_dir: PathBuf::from("/tmp/data/accounts"),
            socket_file: PathBuf::from("/tmp/run/oxidezap.sock"),
            lock_file: PathBuf::from("/tmp/run/oxidezap.lock"),
        };
        assert_eq!(
            paths.downloads_index_file(),
            PathBuf::from("/tmp/config/downloads.json")
        );
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn test_paths(root: &Path) -> Paths {
            Paths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                cache_dir: root.join("cache"),
                downloads_dir: root.join("downloads"),
                settings_file: root.join("config/settings.json"),
                accounts_dir: root.join("data/accounts"),
                socket_file: root.join("run/oxidezap.sock"),
                lock_file: root.join("run/oxidezap.lock"),
            }
        }

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        fn set_mode(path: &Path, mode: u32) {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }

        #[test]
        fn application_directories_are_private() {
            let root = tempfile::tempdir().unwrap();
            let paths = test_paths(root.path());
            paths.ensure().unwrap();

            for dir in [
                &paths.config_dir,
                &paths.data_dir,
                &paths.cache_dir,
                &paths.accounts_dir,
            ] {
                assert_eq!(mode(dir), 0o700, "{}", dir.display());
            }
        }

        #[test]
        fn existing_installations_are_migrated() {
            let root = tempfile::tempdir().unwrap();
            let paths = test_paths(root.path());
            let account_dir = paths.accounts_dir.join("account");
            for dir in [
                &paths.config_dir,
                &paths.data_dir,
                &paths.cache_dir,
                &paths.accounts_dir,
                &account_dir,
                &account_dir.join("data"),
                &account_dir.join("cache"),
            ] {
                fs::create_dir_all(dir).unwrap();
                set_mode(dir, 0o755);
            }
            fs::write(&paths.settings_file, b"{}").unwrap();
            set_mode(&paths.settings_file, 0o644);
            let backup = paths.settings_file.with_extension("json.broken");
            fs::write(&backup, b"{}").unwrap();
            set_mode(&backup, 0o664);

            paths.ensure().unwrap();

            for dir in [
                &paths.config_dir,
                &paths.data_dir,
                &paths.cache_dir,
                &paths.accounts_dir,
                &account_dir,
                &account_dir.join("data"),
                &account_dir.join("cache"),
            ] {
                assert_eq!(mode(dir), 0o700, "{}", dir.display());
            }
            assert_eq!(mode(&paths.settings_file), 0o600);
            assert_eq!(mode(&backup), 0o600);
        }

        #[test]
        fn permission_migration_does_not_follow_symlinks() {
            let root = tempfile::tempdir().unwrap();
            let victim = root.path().join("victim");
            fs::create_dir(&victim).unwrap();
            set_mode(&victim, 0o755);
            let victim_settings = victim.join("settings.json");
            fs::write(&victim_settings, b"{}").unwrap();
            set_mode(&victim_settings, 0o644);

            let paths = test_paths(root.path());
            fs::create_dir_all(&paths.accounts_dir).unwrap();
            let config_link = paths.config_dir.clone();
            std::os::unix::fs::symlink(&victim, &config_link).unwrap();
            let account_link = paths.accounts_dir.join("linked");
            std::os::unix::fs::symlink(&victim, &account_link).unwrap();

            paths.ensure().unwrap();

            assert_eq!(mode(&victim), 0o755);
            assert_eq!(mode(&victim_settings), 0o644);
            assert!(fs::symlink_metadata(&config_link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(fs::symlink_metadata(&account_link)
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }
}
