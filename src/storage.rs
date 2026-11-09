//! Per-account storage inspection and cleanup.
//!
//! Account data lives under `Paths::accounts_dir`, one directory per account,
//! with WebKit's base data directory in `data/` and its base cache directory in
//! `cache/` (see `webview::create_entry`). This module owns the read-only size
//! accounting and the destructive filesystem operations for those directories.
//!
//! # Coordination with WebKit
//!
//! WebKitGTK 2.0.2 (the binding used by OxideZap) does not expose
//! `webkit_website_data_manager_clear`, so a coordinated in-place clear is not
//! available. The GTK thread therefore removes the account's `WebKitWebView`
//! first, waits briefly for the web process to release its profile, and only
//! then runs the filesystem operation on a worker thread. The application
//! recreates the view afterwards. This is why the operations here must never be
//! called while the matching webview is still open.
//!
//! # Limits
//!
//! Scans are bounded by entry count and directory depth and run on a worker
//! thread. They skip symlinks and report `complete == false` when a limit or an
//! unreadable entry is reached. The reported size is therefore a lower bound,
//! not an exact disk-usage measurement.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Maximum number of directory entries inspected for one account directory.
pub const MAX_SCAN_ENTRIES: usize = 200_000;
/// Maximum directory depth inspected for one account directory.
pub const MAX_SCAN_DEPTH: usize = 32;

/// Bounds for one directory scan. A scan that reaches either bound is marked
/// incomplete instead of walking an arbitrarily large tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_SCAN_ENTRIES,
            max_depth: MAX_SCAN_DEPTH,
        }
    }
}

/// Result of one bounded directory walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectorySize {
    pub bytes: u64,
    pub entries: usize,
    /// `false` when the walk stopped early because of a limit or an entry that
    /// could not be inspected.
    pub complete: bool,
}

/// Size of one account's WebKit profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountStorageInfo {
    pub account_id: String,
    pub data: DirectorySize,
    pub cache: DirectorySize,
}

impl AccountStorageInfo {
    pub fn total_bytes(&self) -> u64 {
        self.data.bytes.saturating_add(self.cache.bytes)
    }

    pub fn complete(&self) -> bool {
        self.data.complete && self.cache.complete
    }
}

/// A point-in-time view of every configured account and the free space of the
/// filesystem that holds the account directories.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageSnapshot {
    pub accounts: Vec<AccountStorageInfo>,
    pub total_bytes: u64,
    pub free_bytes: Option<u64>,
}

impl StorageSnapshot {
    pub fn account(&self, account_id: &str) -> Option<&AccountStorageInfo> {
        self.accounts
            .iter()
            .find(|info| info.account_id == account_id)
    }
}

/// Actions offered by Settings > Storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageOperation {
    /// Delete `cache/` and recreate it. The WebKit data directory, and therefore
    /// the WhatsApp session, is untouched.
    ClearCache,
    /// Delete `data/` and recreate it. The account stays configured but has to
    /// scan the QR code again.
    ClearSession,
    /// Delete the complete account directory and forget the account.
    RemoveData,
}

impl StorageOperation {
    pub fn label(self) -> &'static str {
        match self {
            StorageOperation::ClearCache => "clear the cache",
            StorageOperation::ClearSession => "sign out",
            StorageOperation::RemoveData => "delete the account data",
        }
    }

    /// How much confirmation the UI must request before running the operation.
    pub fn confirmation_level(self) -> ConfirmationLevel {
        match self {
            StorageOperation::ClearCache => ConfirmationLevel::None,
            StorageOperation::ClearSession => ConfirmationLevel::Simple,
            StorageOperation::RemoveData => ConfirmationLevel::TypedName,
        }
    }
}

/// Confirmation strength for a storage operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmationLevel {
    /// Run immediately.
    None,
    /// Ask a yes/no question that explains the consequence.
    Simple,
    /// Require the user to type the account name.
    TypedName,
}

/// Confirmation level for an operation, taking into account whether the
/// account window is currently open. Clearing the cache of a closed account is
/// safe and immediate; clearing the cache of an open account closes and
/// reloads its webview, so it asks first. Signing out and deleting data always
/// ask.
pub fn required_confirmation(operation: StorageOperation, account_open: bool) -> ConfirmationLevel {
    match operation {
        StorageOperation::ClearCache if account_open => ConfirmationLevel::Simple,
        operation => operation.confirmation_level(),
    }
}

/// Result of a successful cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageOutcome {
    pub freed_bytes: u64,
}

/// Bounds the number of entries and the depth of a directory walk. Symlinks are
/// never followed: `DirEntry::file_type` does not resolve them, so a linked
/// directory or file is skipped and never contributes to the total.
pub fn directory_size_with_limits(root: &Path, limits: ScanLimits) -> DirectorySize {
    let mut size = DirectorySize {
        complete: limits.max_entries > 0,
        ..DirectorySize::default()
    };
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    'walk: while let Some((directory, depth)) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                size.complete = false;
                continue;
            }
        };
        for entry in entries {
            if size.entries >= limits.max_entries {
                size.complete = false;
                break 'walk;
            }
            let Ok(entry) = entry else {
                size.complete = false;
                continue;
            };
            size.entries += 1;
            let Ok(file_type) = entry.file_type() else {
                size.complete = false;
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth + 1 > limits.max_depth {
                    size.complete = false;
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if file_type.is_file() {
                match entry.metadata() {
                    Ok(metadata) => size.bytes = size.bytes.saturating_add(metadata.len()),
                    Err(_) => size.complete = false,
                }
            }
        }
    }

    size
}

pub fn directory_size(root: &Path) -> DirectorySize {
    directory_size_with_limits(root, ScanLimits::default())
}

/// Scans one account with the default limits. Read-only and safe to call for an
/// account id that has no directory yet.
pub fn account_storage_info(paths: &Paths, account_id: &str) -> AccountStorageInfo {
    if !is_safe_account_id(account_id) {
        let incomplete = DirectorySize {
            complete: false,
            ..DirectorySize::default()
        };
        return AccountStorageInfo {
            account_id: account_id.to_string(),
            data: incomplete,
            cache: incomplete,
        };
    }
    let account_dir = paths.account_dir(account_id);
    AccountStorageInfo {
        account_id: account_id.to_string(),
        data: scan_directory(&account_dir.join("data")),
        cache: scan_directory(&account_dir.join("cache")),
    }
}

/// Scans every configured account. The caller runs this on a worker thread so
/// the GPUI frame loop keeps painting while the filesystem is walked.
pub fn scan_storage(paths: &Paths, account_ids: &[String]) -> StorageSnapshot {
    let accounts: Vec<AccountStorageInfo> = account_ids
        .iter()
        .map(|account_id| account_storage_info(paths, account_id))
        .collect();
    let total_bytes = accounts
        .iter()
        .fold(0u64, |total, info| total.saturating_add(info.total_bytes()));
    StorageSnapshot {
        accounts,
        total_bytes,
        free_bytes: available_space(&paths.data_dir),
    }
}

/// Free bytes available to the current user on the filesystem that contains
/// `path`. Returns `None` when the platform or filesystem cannot report it.
#[cfg(unix)]
pub fn available_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid, NUL-terminated C string and `stat` points to
    // writable memory for exactly one `statvfs`.
    let result = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: `statvfs` returned success, so it initialized the whole struct.
    let stat = unsafe { stat.assume_init() };
    let bytes = u128::from(stat.f_bavail).saturating_mul(u128::from(stat.f_frsize));
    Some(u64::try_from(bytes).unwrap_or(u64::MAX))
}

#[cfg(not(unix))]
pub fn available_space(_path: &Path) -> Option<u64> {
    None
}

/// Runs a storage operation. The caller must have closed the matching webview
/// first (see the module documentation).
pub fn perform(
    paths: &Paths,
    account_id: &str,
    operation: StorageOperation,
) -> Result<StorageOutcome> {
    let freed_bytes = match operation {
        StorageOperation::ClearCache => clear_cache(paths, account_id)?,
        StorageOperation::ClearSession => clear_session(paths, account_id)?,
        StorageOperation::RemoveData => remove_data(paths, account_id)?,
    };
    Ok(StorageOutcome { freed_bytes })
}

/// Deletes the account cache and recreates an empty private directory. The
/// session in `data/` is preserved, so the user stays signed in.
pub fn clear_cache(paths: &Paths, account_id: &str) -> Result<u64> {
    let account_dir = checked_account_dir(paths, account_id)?;
    clear_owned_directory(&account_dir, &paths.account_cache_dir(account_id))
}

/// Deletes the WebKit data directory and recreates an empty private directory.
/// The configured account and its cache are preserved; WhatsApp Web asks for
/// the QR code again.
pub fn clear_session(paths: &Paths, account_id: &str) -> Result<u64> {
    let account_dir = checked_account_dir(paths, account_id)?;
    clear_owned_directory(&account_dir, &paths.account_data_dir(account_id))
}

/// Deletes the complete account directory, including `data/` and `cache/`.
/// The caller must also remove the account from the settings.
pub fn remove_data(paths: &Paths, account_id: &str) -> Result<u64> {
    let account_dir = checked_account_dir(paths, account_id)?;
    ensure_safe_directory(&account_dir)?;
    let freed = directory_size(&account_dir).bytes;
    remove_owned_directory(&account_dir)?;
    Ok(freed)
}

/// Whether `account_id` is a single, ordinary path component. Settings files
/// can be imported from elsewhere, so an account id is untrusted input and must
/// never be able to escape `accounts/`.
pub fn is_safe_account_id(account_id: &str) -> bool {
    !account_id.is_empty()
        && account_id != "."
        && account_id != ".."
        && !account_id.contains('/')
        && !account_id.contains('\\')
        && !account_id.contains('\0')
}

/// Resolves an account directory, rejecting ids that could escape the accounts
/// root.
pub fn checked_account_dir(paths: &Paths, account_id: &str) -> Result<PathBuf> {
    if !is_safe_account_id(account_id) {
        bail!("refusing to use unsafe account id {account_id:?}");
    }
    let directory = paths.account_dir(account_id);
    if directory.parent() != Some(paths.accounts_dir.as_path()) {
        bail!("refusing to touch a directory outside the accounts root");
    }
    Ok(directory)
}

/// Rejects destructive operations that would leave the application without an
/// account. `account_exists` comes from the live settings, not from the
/// directory listing.
pub fn validate_operation(
    operation: StorageOperation,
    account_count: usize,
    account_exists: bool,
) -> Result<()> {
    if !account_exists {
        bail!("the account no longer exists");
    }
    if operation == StorageOperation::RemoveData && account_count <= 1 {
        bail!("the last account cannot be deleted");
    }
    Ok(())
}

/// Matches the typed confirmation for a destructive action against the account
/// name. The comparison ignores surrounding whitespace but is otherwise exact.
pub fn typed_confirmation_matches(typed: &str, account_name: &str) -> bool {
    let account_name = account_name.trim();
    !account_name.is_empty() && typed.trim() == account_name
}

/// Formats a byte count with binary units. Values below 1 KiB stay in bytes.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn clear_owned_directory(account_dir: &Path, target: &Path) -> Result<u64> {
    ensure_safe_directory(account_dir)?;
    if !account_dir.is_dir() {
        // The account was removed between the operation being requested and
        // this worker running. Nothing to clear and nothing to recreate.
        return Ok(0);
    }
    ensure_safe_directory(target)?;
    let freed = scan_directory(target).bytes;
    remove_owned_directory(target)?;
    // A concurrent account removal must not have its directory recreated by a
    // late cache/session clear.
    if account_dir.is_dir() {
        crate::paths::create_private_dir(target)
            .with_context(|| format!("could not create {}", target.display()))?;
    }
    Ok(freed)
}

/// Reads a profile directory without following a symlink at its root. A linked
/// profile is never scanned: it belongs to somebody else from our point of
/// view.
fn scan_directory(path: &Path) -> DirectorySize {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => DirectorySize {
            complete: false,
            ..DirectorySize::default()
        },
        Ok(metadata) if !metadata.is_dir() => DirectorySize {
            complete: false,
            ..DirectorySize::default()
        },
        _ => directory_size(path),
    }
}

/// A missing directory is fine (nothing to clear yet). A symlink or a regular
/// file is refused: deleting through it could touch a foreign tree.
fn ensure_safe_directory(directory: &Path) -> Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to modify the symlinked directory {}",
                directory.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("{} is not a directory", directory.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("could not inspect {}", directory.display()))
        }
    }
}

fn remove_owned_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing to remove the symlink {}", path.display());
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(path).with_context(|| format!("could not remove {}", path.display()))
        }
        Ok(_) => bail!("{} is not a directory", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("could not inspect {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

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

    fn create_account(
        paths: &Paths,
        id: &str,
        data_files: &[(&str, usize)],
        cache_files: &[(&str, usize)],
    ) {
        for (name, size) in data_files {
            let path = paths.account_data_dir(id).join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![0u8; *size]).unwrap();
        }
        for (name, size) in cache_files {
            let path = paths.account_cache_dir(id).join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![0u8; *size]).unwrap();
        }
    }

    #[test]
    fn directory_size_sums_regular_files() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("nested")).unwrap();
        fs::write(root.path().join("a.bin"), vec![0u8; 10]).unwrap();
        fs::write(root.path().join("nested/b.bin"), vec![0u8; 20]).unwrap();

        let size = directory_size(root.path());

        assert_eq!(size.bytes, 30);
        assert_eq!(size.entries, 3);
        assert!(size.complete);
    }

    #[test]
    fn missing_directories_are_empty_and_complete() {
        let root = tempfile::tempdir().unwrap();
        let size = directory_size(&root.path().join("does-not-exist"));
        assert_eq!(size.bytes, 0);
        assert_eq!(size.entries, 0);
        assert!(size.complete);
    }

    #[test]
    fn size_scans_are_bounded_by_entry_count() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..8 {
            fs::write(root.path().join(format!("{index}.bin")), vec![0u8; 5]).unwrap();
        }

        let size = directory_size_with_limits(
            root.path(),
            ScanLimits {
                max_entries: 3,
                max_depth: 8,
            },
        );

        assert_eq!(size.entries, 3);
        assert!(!size.complete);
        assert!(size.bytes <= 15);
    }

    #[test]
    fn size_scans_are_bounded_by_depth() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("top.bin"), vec![0u8; 7]).unwrap();
        fs::create_dir_all(root.path().join("deep")).unwrap();
        fs::write(root.path().join("deep/bottom.bin"), vec![0u8; 11]).unwrap();

        let size = directory_size_with_limits(
            root.path(),
            ScanLimits {
                max_entries: 100,
                max_depth: 0,
            },
        );

        assert_eq!(size.bytes, 7);
        assert!(!size.complete);
    }

    #[cfg(unix)]
    #[test]
    fn size_scans_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.bin"), vec![0u8; 4096]).unwrap();
        symlink(outside.path(), root.path().join("linked-dir")).unwrap();
        symlink(
            outside.path().join("secret.bin"),
            root.path().join("linked-file"),
        )
        .unwrap();
        fs::write(root.path().join("real.bin"), vec![0u8; 9]).unwrap();

        let size = directory_size(root.path());

        assert_eq!(size.bytes, 9);
        assert!(size.complete);
    }

    #[test]
    fn account_info_splits_data_and_cache() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        create_account(
            &paths,
            "acc",
            &[("session/leveldb.bin", 12)],
            &[("net/cache.bin", 5)],
        );

        let info = account_storage_info(&paths, "acc");

        assert_eq!(info.account_id, "acc");
        assert_eq!(info.data.bytes, 12);
        assert_eq!(info.cache.bytes, 5);
        assert_eq!(info.total_bytes(), 17);
        assert!(info.complete());
    }

    #[test]
    fn snapshots_sum_every_account_and_report_free_space() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(&paths.data_dir).unwrap();
        create_account(&paths, "a", &[("x", 3)], &[("y", 4)]);
        create_account(&paths, "b", &[("x", 5)], &[]);

        let snapshot = scan_storage(&paths, &["a".to_string(), "b".to_string()]);

        assert_eq!(snapshot.accounts.len(), 2);
        assert_eq!(snapshot.total_bytes, 12);
        assert!(snapshot.account("a").is_some());
        assert!(snapshot.account("missing").is_none());
        #[cfg(unix)]
        assert!(snapshot.free_bytes.is_some());
    }

    #[test]
    fn clear_cache_keeps_the_session_data() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        create_account(&paths, "acc", &[("session.bin", 10)], &[("cache.bin", 5)]);

        let freed = clear_cache(&paths, "acc").unwrap();

        assert_eq!(freed, 5);
        assert!(paths.account_data_dir("acc").join("session.bin").exists());
        assert_eq!(
            fs::read_dir(paths.account_cache_dir("acc"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn clear_session_keeps_the_cache_and_recreates_the_data_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        create_account(&paths, "acc", &[("session.bin", 10)], &[("cache.bin", 5)]);

        let freed = clear_session(&paths, "acc").unwrap();

        assert_eq!(freed, 10);
        assert_eq!(
            fs::read_dir(paths.account_data_dir("acc")).unwrap().count(),
            0
        );
        assert!(paths.account_cache_dir("acc").join("cache.bin").exists());
    }

    #[test]
    fn remove_data_deletes_only_the_target_account() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        create_account(&paths, "a", &[("session.bin", 4)], &[("cache.bin", 2)]);
        create_account(&paths, "b", &[("session.bin", 7)], &[]);

        remove_data(&paths, "a").unwrap();

        assert!(!paths.account_dir("a").exists());
        assert!(paths.account_data_dir("b").join("session.bin").exists());
    }

    #[test]
    fn unsafe_account_ids_cannot_escape_the_accounts_root() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(&paths.accounts_dir).unwrap();
        let victim = root.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), b"keep").unwrap();

        for id in ["", ".", "..", "../victim", "a/b", "a\\b", "a\0b"] {
            assert!(!is_safe_account_id(id), "{id:?}");
            assert!(remove_data(&paths, id).is_err(), "{id:?}");
        }

        let info = account_storage_info(&paths, "../victim");
        assert_eq!(info.data.bytes, 0);
        assert_eq!(info.cache.bytes, 0);
        assert!(!info.complete());

        assert!(victim.join("keep.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn destructive_operations_refuse_a_symlinked_account_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(&paths.accounts_dir).unwrap();
        let victim = root.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), b"keep").unwrap();
        symlink(&victim, paths.account_dir("linked")).unwrap();

        assert!(clear_cache(&paths, "linked").is_err());
        assert!(remove_data(&paths, "linked").is_err());

        assert!(victim.join("keep.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn destructive_operations_refuse_symlinked_profile_directories() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.account_dir("acc")).unwrap();
        let victim = root.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), b"keep").unwrap();
        symlink(&victim, paths.account_data_dir("acc")).unwrap();

        assert!(clear_session(&paths, "acc").is_err());
        assert!(victim.join("keep.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scans_and_cache_clears_skip_a_symlinked_cache_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        fs::create_dir_all(paths.account_dir("acc")).unwrap();
        let victim = root.path().join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("keep.txt"), vec![0u8; 32]).unwrap();
        symlink(&victim, paths.account_cache_dir("acc")).unwrap();

        let info = account_storage_info(&paths, "acc");

        assert_eq!(info.cache.bytes, 0);
        assert!(!info.cache.complete);
        assert!(clear_cache(&paths, "acc").is_err());
        assert!(victim.join("keep.txt").exists());
    }

    #[test]
    fn the_last_account_cannot_be_removed() {
        assert!(validate_operation(StorageOperation::RemoveData, 1, true).is_err());
        assert!(validate_operation(StorageOperation::RemoveData, 2, true).is_ok());
        assert!(validate_operation(StorageOperation::ClearSession, 1, true).is_ok());
        assert!(validate_operation(StorageOperation::ClearCache, 1, true).is_ok());
        assert!(validate_operation(StorageOperation::ClearCache, 1, false).is_err());
    }

    #[test]
    fn destructive_operations_require_confirmation() {
        assert_eq!(
            StorageOperation::ClearCache.confirmation_level(),
            ConfirmationLevel::None
        );
        assert_eq!(
            StorageOperation::ClearSession.confirmation_level(),
            ConfirmationLevel::Simple
        );
        assert_eq!(
            StorageOperation::RemoveData.confirmation_level(),
            ConfirmationLevel::TypedName
        );
    }

    #[test]
    fn clearing_the_cache_of_an_open_account_requires_confirmation() {
        assert_eq!(
            required_confirmation(StorageOperation::ClearCache, false),
            ConfirmationLevel::None
        );
        assert_eq!(
            required_confirmation(StorageOperation::ClearCache, true),
            ConfirmationLevel::Simple
        );
        assert_eq!(
            required_confirmation(StorageOperation::ClearSession, false),
            ConfirmationLevel::Simple
        );
        assert_eq!(
            required_confirmation(StorageOperation::RemoveData, false),
            ConfirmationLevel::TypedName
        );
    }

    #[test]
    fn typed_confirmation_requires_the_exact_account_name() {
        assert!(typed_confirmation_matches("Work", "Work"));
        assert!(typed_confirmation_matches("  Work  ", "Work"));
        assert!(!typed_confirmation_matches("", "Work"));
        assert!(!typed_confirmation_matches("work", "Work"));
        assert!(!typed_confirmation_matches("Work", "Work 2"));
        assert!(!typed_confirmation_matches("", ""));
    }

    #[test]
    fn byte_counts_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn settings_timestamp_helper_accepts_epoch() {
        // The cleanup marker reuses the settings timestamp format; this test
        // keeps the dependency visible if that helper ever moves.
        assert_eq!(
            crate::settings::format_timestamp(UNIX_EPOCH + Duration::from_secs(0)),
            "1970-01-01T00:00:00Z"
        );
    }
}
