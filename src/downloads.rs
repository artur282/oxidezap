use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::paths::{create_private_dir, restrict_file, write_private_file};

/// The download history keeps the newest records and drops the oldest finished
/// ones. Active downloads are never evicted by the cap.
pub const DOWNLOAD_HISTORY_LIMIT: usize = 500;
pub const DOWNLOAD_INDEX_VERSION: u32 = 1;
/// Minimum interval between progress events sent from the GTK thread. A large
/// file receives thousands of chunks; the UI only needs a few updates a second.
pub const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// A single path component cannot exceed 255 bytes on common Linux
/// filesystems. Leave room for the collision suffix (` (123)`).
pub const MAX_FILENAME_BYTES: usize = 200;
const MAX_FOLDER_BYTES: usize = 64;
const MAX_DOWNLOAD_PATH_ATTEMPTS: u32 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl DownloadStatus {
    pub fn label(self) -> &'static str {
        match self {
            DownloadStatus::InProgress => "In progress",
            DownloadStatus::Completed => "Completed",
            DownloadStatus::Failed => "Failed",
            DownloadStatus::Cancelled => "Cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: String,
    pub account_id: String,
    #[serde(default)]
    pub account_name: String,
    pub filename: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub received_bytes: u64,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    pub status: DownloadStatus,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
}

impl DownloadRecord {
    pub fn is_active(&self) -> bool {
        self.status == DownloadStatus::InProgress
    }

    pub fn progress(&self) -> Option<f64> {
        let total = self.total_bytes.filter(|total| *total > 0)?;
        Some((self.received_bytes as f64 / total as f64).clamp(0.0, 1.0))
    }

    /// A finished failed or cancelled download can be retried only when WebKit
    /// captured a plain HTTP(S) source. `blob:` and `data:` URLs are scoped to
    /// the page that created them and cannot be replayed later.
    pub fn retry_url(&self) -> Option<&str> {
        if !matches!(
            self.status,
            DownloadStatus::Failed | DownloadStatus::Cancelled
        ) {
            return None;
        }
        let url = self.source_url.as_deref()?;
        (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
    }

    pub fn destination_dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }
}

/// Current Unix time in milliseconds, the unit persisted by the history.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A compact size for the download list. Values are binary units.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < MIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else if bytes_f < GIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else {
        format!("{:.2} GiB", bytes_f / GIB)
    }
}

/// Where the next download is written. It is built on the GPUI thread and sent
/// to the GTK thread, so destination decisions never read shared settings.
#[derive(Clone, Debug)]
pub struct DownloadLayout {
    pub base_dir: PathBuf,
    pub account_subfolders: bool,
    pub date_subfolders: bool,
    account_folders: HashMap<String, String>,
}

impl Default for DownloadLayout {
    fn default() -> Self {
        Self {
            base_dir: PathBuf::new(),
            account_subfolders: false,
            date_subfolders: false,
            account_folders: HashMap::new(),
        }
    }
}

impl DownloadLayout {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            ..Self::default()
        }
    }

    pub fn with_options(mut self, account_subfolders: bool, date_subfolders: bool) -> Self {
        self.account_subfolders = account_subfolders;
        self.date_subfolders = date_subfolders;
        self
    }

    pub fn with_accounts(mut self, accounts: impl IntoIterator<Item = (String, String)>) -> Self {
        self.account_folders = accounts
            .into_iter()
            .map(|(id, name)| (id, sanitize_folder_name(&name)))
            .collect();
        self
    }

    /// The folder that receives a download for `account_id` at `at`. Only
    /// sanitized single components are appended to the configured base.
    pub fn destination_dir(&self, account_id: &str, at: SystemTime) -> PathBuf {
        let mut dir = self.base_dir.clone();
        if self.account_subfolders {
            let account = self
                .account_folders
                .get(account_id)
                .map(String::as_str)
                .unwrap_or("Account");
            dir.push(account);
        }
        if self.date_subfolders {
            dir.push(year_month(at));
        }
        dir
    }

    /// Creates the destination tree and reserves a collision-free path inside
    /// it. Both the base folder and the final folder are canonicalized so a
    /// pre-existing symlink cannot redirect a download outside the base.
    pub fn reserve(&self, account_id: &str, filename: &str, at: SystemTime) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.base_dir)?;
        let dir = self.destination_dir(account_id, at);
        fs::create_dir_all(&dir)?;
        let base = self.base_dir.canonicalize()?;
        let dir = dir.canonicalize()?;
        if !dir.starts_with(&base) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "download folder {} is outside {}",
                    dir.display(),
                    base.display()
                ),
            ));
        }
        let path = reserve_download_path(&dir, &sanitize_filename(filename))?;
        if !path_is_within(&dir, &path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is outside {}", path.display(), dir.display()),
            ));
        }
        Ok(path)
    }
}

fn sanitize_folder_name(name: &str) -> String {
    let folder = sanitize_component(name, "Account");
    truncate_to_bytes(&folder, MAX_FOLDER_BYTES)
}

fn sanitize_component(value: &str, fallback: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | '\0') || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

/// Turns a WebKit-suggested name into a single, safe path component. Path
/// separators and control characters are replaced, `.`/`..` fall back to
/// `download`, and the result is bounded to a portable byte length.
pub fn sanitize_filename(filename: &str) -> String {
    let filename = sanitize_component(filename, "download");
    truncate_filename(&filename)
}

fn truncate_filename(filename: &str) -> String {
    if filename.len() <= MAX_FILENAME_BYTES {
        return filename.to_string();
    }
    let (stem, extension) = split_download_name(filename);
    let budget = MAX_FILENAME_BYTES.saturating_sub(extension.len());
    let mut stem = stem;
    while stem.len() > budget {
        stem.pop();
    }
    let candidate = format!("{stem}{extension}");
    let candidate = truncate_to_bytes(&candidate, MAX_FILENAME_BYTES);
    if candidate.is_empty() {
        "download".to_string()
    } else {
        candidate
    }
}

fn truncate_to_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Claims `dir/filename`, or the first free `name (n).ext` variant, by creating
/// the file with `create_new`. Two downloads racing for the same name can never
/// both win the same path, and an existing file is never overwritten: the
/// returned path is an existing, empty file that belongs to this download.
fn reserve_download_path(dir: &Path, filename: &str) -> io::Result<PathBuf> {
    let (stem, extension) = split_download_name(filename);
    for index in 0..MAX_DOWNLOAD_PATH_ATTEMPTS {
        let candidate = if index == 0 {
            dir.join(filename)
        } else {
            dir.join(format!("{stem} ({index}){extension}"))
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_reservation) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free download name for {filename}"),
    ))
}

fn split_download_name(filename: &str) -> (String, String) {
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string());
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    (stem, extension)
}

/// Drops the empty reservation of a download WebKit could not write to.
/// Partial files stay on disk because they may still be worth recovering.
pub(crate) fn discard_empty_reservation(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() > 0 {
        return;
    }
    if let Err(error) = fs::remove_file(path) {
        warn!(%error, path = %path.display(), "could not remove an empty download reservation");
    }
}

/// Reports whether `path` is lexically inside `base`. Used by tests and as a
/// defensive check around paths persisted by older versions.
pub fn path_is_within(base: &Path, path: &Path) -> bool {
    path.starts_with(base)
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

/// Validates a user-supplied download folder. Relative paths and paths that
/// exist but are not directories are rejected instead of being silently
/// reinterpreted against the current working directory.
pub fn valid_base_dir(value: &str) -> Option<PathBuf> {
    crate::paths::valid_directory_setting(value)
}

/// `YYYY-MM` in UTC. The civil-date conversion is pure, so folder layout tests
/// do not depend on the host clock or timezone.
pub fn year_month(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (year, month, _) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}")
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct DownloadIndex {
    version: u32,
    records: Vec<DownloadRecord>,
}

impl Default for DownloadIndex {
    fn default() -> Self {
        Self {
            version: DOWNLOAD_INDEX_VERSION,
            records: Vec::new(),
        }
    }
}

/// Persistent, bounded download history. Records are newest first.
pub struct DownloadManager {
    file: PathBuf,
    records: Vec<DownloadRecord>,
}

impl DownloadManager {
    pub fn empty(file: PathBuf) -> Self {
        Self {
            file,
            records: Vec::new(),
        }
    }

    pub fn load(file: PathBuf) -> Self {
        let data = match fs::read(&file) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::empty(file);
            }
            Err(error) => {
                warn!(path = %file.display(), %error, "could not read the download history");
                return Self::empty(file);
            }
        };
        match serde_json::from_slice::<DownloadIndex>(&data) {
            Ok(index) => {
                let mut records = index.records;
                normalize_records(&mut records);
                Self { file, records }
            }
            Err(error) => {
                warn!(path = %file.display(), %error, "could not parse the download history");
                let backup = file.with_extension("json.broken");
                if let Err(backup_error) = write_private_file(&backup, &data) {
                    warn!(
                        path = %backup.display(),
                        %backup_error,
                        "could not back up the download history"
                    );
                }
                Self::empty(file)
            }
        }
    }

    pub fn records(&self) -> &[DownloadRecord] {
        &self.records
    }

    pub fn get(&self, id: &str) -> Option<&DownloadRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub fn active_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.is_active())
            .count()
    }

    /// Inserts a newly started download. A replayed id is ignored so a
    /// duplicated GTK event cannot create a second row.
    pub fn start(&mut self, record: DownloadRecord) -> bool {
        if self.records.iter().any(|existing| existing.id == record.id) {
            return false;
        }
        self.records.insert(0, record);
        enforce_history_limit(&mut self.records);
        true
    }

    pub fn progress(&mut self, id: &str, received: u64, total: Option<u64>) -> bool {
        let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == id && record.is_active())
        else {
            return false;
        };
        let total_changed = total.is_some_and(|total| record.total_bytes != Some(total));
        if record.received_bytes == received && !total_changed {
            return false;
        }
        record.received_bytes = received;
        if let Some(total) = total {
            record.total_bytes = Some(total);
        }
        true
    }

    /// Applies a terminal event. Late progress and late failure events never
    /// overwrite a completed record, and a completed file wins over a cancel
    /// request that raced with WebKit finishing the write.
    pub fn finish(&mut self, id: &str, success: bool, cancelled: bool, at: u64) -> bool {
        let Some(record) = self.records.iter_mut().find(|record| record.id == id) else {
            return false;
        };
        if success {
            if record.status == DownloadStatus::Completed || record.status == DownloadStatus::Failed
            {
                return false;
            }
            record.status = DownloadStatus::Completed;
            if let Some(total) = record.total_bytes {
                record.received_bytes = record.received_bytes.max(total);
            }
        } else if cancelled {
            if record.status != DownloadStatus::InProgress {
                return false;
            }
            record.status = DownloadStatus::Cancelled;
        } else {
            if record.status != DownloadStatus::InProgress {
                return false;
            }
            record.status = DownloadStatus::Failed;
        }
        record.finished_at = Some(at);
        true
    }

    /// Marks a download as cancelled as soon as the user asks. The failed GTK
    /// event that follows is ignored because the state is no longer active.
    pub fn cancel(&mut self, id: &str, at: u64) -> bool {
        let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == id && record.is_active())
        else {
            return false;
        };
        record.status = DownloadStatus::Cancelled;
        record.finished_at = Some(at);
        true
    }

    /// Removes a finished record from the history. Active downloads stay until
    /// they finish so their terminal event still has a row to update.
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.records.iter().position(|record| record.id == id) else {
            return false;
        };
        if self.records[index].is_active() {
            return false;
        }
        self.records.remove(index);
        true
    }

    pub fn snapshot(&self) -> Vec<DownloadRecord> {
        self.records.clone()
    }

    #[cfg(test)]
    pub fn save(&self) -> Result<()> {
        save_records(&self.file, &self.records)
    }

    pub fn file(&self) -> &Path {
        &self.file
    }
}

fn normalize_records(records: &mut Vec<DownloadRecord>) {
    let mut seen = HashSet::new();
    records.retain(|record| seen.insert(record.id.clone()));
    for record in records.iter_mut() {
        if record.status == DownloadStatus::InProgress {
            // The previous process exited while the transfer was running.
            record.status = DownloadStatus::Failed;
            record.finished_at = None;
        }
    }
    enforce_history_limit(records);
}

fn enforce_history_limit(records: &mut Vec<DownloadRecord>) {
    while records.len() > DOWNLOAD_HISTORY_LIMIT {
        match records.iter().rposition(|record| !record.is_active()) {
            Some(index) => {
                records.remove(index);
            }
            None => break,
        }
    }
}

fn save_records(path: &Path, records: &[DownloadRecord]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let index = DownloadIndex {
        version: DOWNLOAD_INDEX_VERSION,
        records: records.to_vec(),
    };
    let data = serde_json::to_vec_pretty(&index).context("could not serialize downloads")?;
    let tmp = path.with_extension("json.tmp");
    write_private_file(&tmp, &data)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
    restrict_file(path).with_context(|| format!("could not restrict {}", path.display()))?;
    Ok(())
}

/// Serializes history writes on a worker thread so neither JSON encoding nor
/// disk I/O runs on the GPUI thread. A single worker keeps the last snapshot
/// authoritative.
pub struct DownloadWriter {
    tx: Sender<DownloadWrite>,
}

struct DownloadWrite {
    records: Vec<DownloadRecord>,
    path: PathBuf,
    done: Option<Sender<()>>,
}

impl DownloadWriter {
    pub fn start() -> Self {
        let (tx, rx) = unbounded::<DownloadWrite>();
        let thread = std::thread::Builder::new()
            .name("oxidezap-downloads".to_string())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    if let Err(error) = save_records(&request.path, &request.records) {
                        warn!(path = %request.path.display(), %error, "could not save downloads");
                    }
                    if let Some(done) = request.done {
                        let _ = done.send(());
                    }
                }
            });
        if let Err(error) = thread {
            warn!(%error, "could not start the downloads writer thread");
        }
        Self { tx }
    }

    pub fn save(&self, records: Vec<DownloadRecord>, path: PathBuf) {
        let request = DownloadWrite {
            records,
            path,
            done: None,
        };
        if let Err(error) = self.tx.send(request) {
            let request = error.0;
            let _ = save_records(&request.path, &request.records);
        }
    }

    /// Queues the final save and waits briefly for the worker. Called on quit
    /// so the process does not exit before the last state is stored.
    pub fn save_and_wait(&self, records: Vec<DownloadRecord>, path: PathBuf) {
        let (done_tx, done_rx) = bounded(1);
        let request = DownloadWrite {
            records,
            path,
            done: Some(done_tx),
        };
        if let Err(error) = self.tx.send(request) {
            let request = error.0;
            let _ = save_records(&request.path, &request.records);
            return;
        }
        if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            warn!("downloads writer did not confirm the final save");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, status: DownloadStatus) -> DownloadRecord {
        DownloadRecord {
            id: id.to_string(),
            account_id: "account".to_string(),
            account_name: "Account".to_string(),
            filename: "file.txt".to_string(),
            path: Some(PathBuf::from("/tmp/file.txt")),
            source_url: Some("https://example.com/file.txt".to_string()),
            received_bytes: 0,
            total_bytes: Some(100),
            status,
            started_at: 1,
            finished_at: None,
        }
    }

    fn manager() -> (tempfile::TempDir, DownloadManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = DownloadManager::empty(dir.path().join("downloads.json"));
        (dir, manager)
    }

    #[test]
    fn sanitizes_download_names() {
        assert_eq!(sanitize_filename("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename("."), "download");
        assert_eq!(sanitize_filename("a/b\\c\0d.txt"), "a_b_c_d.txt");
    }

    #[test]
    fn sanitized_names_are_single_path_components() {
        let dir = tempfile::tempdir().unwrap();
        for attack in [
            "../../etc/passwd",
            "..",
            ".",
            "/etc/passwd",
            "a/../../b",
            "..\\..\\windows",
        ] {
            let name = sanitize_filename(attack);
            let path = dir.path().join(&name);
            assert_eq!(path.parent(), Some(dir.path()), "{attack:?} escaped");
            assert!(
                path_is_within(dir.path(), &path),
                "{attack:?} produced {path:?}"
            );
        }
    }

    #[test]
    fn long_names_are_truncated_but_keep_their_extension() {
        let name = format!("{}.txt", "a".repeat(500));
        let sanitized = sanitize_filename(&name);
        assert!(sanitized.len() <= MAX_FILENAME_BYTES);
        assert!(sanitized.ends_with(".txt"));
        assert!(sanitized.starts_with('a'));
    }

    #[test]
    fn reserves_an_empty_download_file() {
        let dir = tempfile::tempdir().unwrap();

        let path = reserve_download_path(dir.path(), "file.txt").unwrap();

        assert_eq!(path, dir.path().join("file.txt"));
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn download_reservations_do_not_overwrite_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), b"keep").unwrap();

        let first = reserve_download_path(dir.path(), "file.txt").unwrap();
        let second = reserve_download_path(dir.path(), "file.txt").unwrap();

        assert_eq!(first, dir.path().join("file (1).txt"));
        assert_eq!(second, dir.path().join("file (2).txt"));
        assert_eq!(fs::read(dir.path().join("file.txt")).unwrap(), b"keep");
    }

    #[test]
    fn concurrent_download_reservations_get_distinct_paths() {
        const DOWNLOADS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(DOWNLOADS));
        let handles: Vec<_> = (0..DOWNLOADS)
            .map(|_| {
                let dir = dir.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_download_path(&dir, "file.txt").unwrap()
                })
            })
            .collect();
        let mut paths: Vec<PathBuf> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        paths.sort();
        paths.dedup();

        assert_eq!(paths.len(), DOWNLOADS);
        for path in paths {
            assert!(path.exists(), "{} was not reserved", path.display());
            assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        }
    }

    #[test]
    fn failed_download_removes_the_empty_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let path = reserve_download_path(dir.path(), "file.txt").unwrap();

        discard_empty_reservation(&path);

        assert!(!path.exists());
    }

    #[test]
    fn failed_download_keeps_partially_written_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = reserve_download_path(dir.path(), "file.txt").unwrap();
        fs::write(&path, b"partial").unwrap();

        discard_empty_reservation(&path);

        assert_eq!(fs::read(&path).unwrap(), b"partial");
    }

    #[test]
    fn history_is_capped_at_the_limit() {
        let (_dir, mut manager) = manager();
        for index in 0..DOWNLOAD_HISTORY_LIMIT {
            assert!(manager.start(record(&format!("id-{index}"), DownloadStatus::Completed)));
        }
        assert_eq!(manager.records().len(), DOWNLOAD_HISTORY_LIMIT);

        assert!(manager.start(record("newest", DownloadStatus::Completed)));

        assert_eq!(manager.records().len(), DOWNLOAD_HISTORY_LIMIT);
        assert_eq!(manager.records()[0].id, "newest");
        assert!(
            manager.records().iter().all(|record| record.id != "id-0"),
            "the oldest record should have been evicted"
        );
    }

    #[test]
    fn the_history_cap_never_evicts_an_active_download() {
        let (_dir, mut manager) = manager();
        for index in 0..DOWNLOAD_HISTORY_LIMIT {
            let status = if index == DOWNLOAD_HISTORY_LIMIT - 1 {
                DownloadStatus::InProgress
            } else {
                DownloadStatus::Completed
            };
            assert!(manager.start(record(&format!("id-{index}"), status)));
        }

        assert!(manager.start(record("newest", DownloadStatus::Completed)));

        assert_eq!(manager.records().len(), DOWNLOAD_HISTORY_LIMIT);
        assert!(
            manager
                .records()
                .iter()
                .any(|record| record.id == format!("id-{}", DOWNLOAD_HISTORY_LIMIT - 1))
        );
    }

    #[test]
    fn duplicate_ids_are_deduplicated() {
        let (_dir, mut manager) = manager();

        assert!(manager.start(record("same", DownloadStatus::InProgress)));
        assert!(!manager.start(record("same", DownloadStatus::Completed)));

        assert_eq!(manager.records().len(), 1);
        assert_eq!(manager.records()[0].status, DownloadStatus::InProgress);
    }

    #[test]
    fn state_transitions_follow_the_download_lifecycle() {
        let (_dir, mut manager) = manager();
        manager.start(record("a", DownloadStatus::InProgress));

        assert!(manager.progress("a", 50, Some(100)));
        assert!(!manager.progress("a", 50, Some(100)));
        assert!(manager.progress("a", 75, None));
        assert_eq!(manager.get("a").unwrap().total_bytes, Some(100));
        assert!(manager.finish("a", true, false, 10));
        assert!(!manager.progress("a", 90, Some(100)));
        assert!(!manager.finish("a", false, false, 11));

        let record = manager.get("a").unwrap();
        assert_eq!(record.status, DownloadStatus::Completed);
        assert_eq!(record.finished_at, Some(10));
        assert_eq!(record.received_bytes, 100);
    }

    #[test]
    fn cancelling_a_download_ignores_the_following_failure() {
        let (_dir, mut manager) = manager();
        manager.start(record("a", DownloadStatus::InProgress));

        assert!(manager.cancel("a", 5));
        assert!(!manager.cancel("a", 6));
        assert!(!manager.finish("a", false, true, 7));

        let record = manager.get("a").unwrap();
        assert_eq!(record.status, DownloadStatus::Cancelled);
        assert_eq!(record.finished_at, Some(5));
    }

    #[test]
    fn a_completed_file_wins_over_a_racing_cancel() {
        let (_dir, mut manager) = manager();
        manager.start(record("a", DownloadStatus::InProgress));
        manager.cancel("a", 5);

        assert!(manager.finish("a", true, false, 6));

        assert_eq!(manager.get("a").unwrap().status, DownloadStatus::Completed);
    }

    #[test]
    fn active_records_cannot_be_removed() {
        let (_dir, mut manager) = manager();
        manager.start(record("a", DownloadStatus::InProgress));
        assert!(!manager.remove("a"));
        assert!(manager.finish("a", true, false, 2));
        assert!(manager.remove("a"));
        assert!(manager.records().is_empty());
        assert!(!manager.remove("a"));
    }

    #[test]
    fn stale_in_progress_records_become_failed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.json");
        let index = DownloadIndex {
            version: DOWNLOAD_INDEX_VERSION,
            records: vec![record("stale", DownloadStatus::InProgress)],
        };
        fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();

        let manager = DownloadManager::load(path);

        let record = manager.get("stale").unwrap();
        assert_eq!(record.status, DownloadStatus::Failed);
        assert_eq!(record.finished_at, None);
    }

    #[test]
    fn history_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.json");
        let mut manager = DownloadManager::empty(path.clone());
        manager.start(record("a", DownloadStatus::InProgress));
        manager.finish("a", true, false, 42);
        manager.save().unwrap();

        let loaded = DownloadManager::load(path);

        assert_eq!(loaded.records().len(), 1);
        assert_eq!(loaded.get("a").unwrap().status, DownloadStatus::Completed);
        assert_eq!(loaded.get("a").unwrap().finished_at, Some(42));
    }

    #[test]
    fn broken_history_is_backed_up_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.json");
        fs::write(&path, b"{not json").unwrap();

        let manager = DownloadManager::load(path.clone());

        assert!(manager.records().is_empty());
        assert!(path.with_extension("json.broken").exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_history_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.json");
        let mut manager = DownloadManager::empty(path.clone());
        manager.start(record("a", DownloadStatus::Completed));
        manager.save().unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let temporary_mode = fs::metadata(path.with_extension("json.tmp"))
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777);
        assert_eq!(
            temporary_mode, None,
            "the temporary file must be renamed away"
        );
    }

    #[test]
    fn layout_organizes_by_account_and_month() {
        let base = PathBuf::from("/tmp/base");
        let layout = DownloadLayout::new(base.clone())
            .with_options(true, true)
            .with_accounts([("a".to_string(), "Work / Team".to_string())]);
        let at = UNIX_EPOCH + Duration::from_secs(1_794_441_600);

        let dir = layout.destination_dir("a", at);

        assert_eq!(dir, base.join("Work _ Team").join(year_month(at)));
        assert_eq!(year_month(at), "2026-11");
    }

    #[test]
    fn layout_uses_the_default_folder_flags() {
        let layout = DownloadLayout::new(PathBuf::from("/tmp/base"));
        let at = UNIX_EPOCH + Duration::from_secs(1_794_441_600);

        assert_eq!(layout.destination_dir("a", at), PathBuf::from("/tmp/base"));
    }

    #[test]
    fn layout_reservations_avoid_collisions_inside_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        let layout = DownloadLayout::new(dir.path().to_path_buf()).with_options(true, false);
        let at = UNIX_EPOCH;

        let first = layout.reserve("a", "file.txt", at).unwrap();
        let second = layout.reserve("a", "file.txt", at).unwrap();

        assert_eq!(first, dir.path().join("Account").join("file.txt"));
        assert_eq!(second, dir.path().join("Account").join("file (1).txt"));
    }

    #[test]
    fn layout_rejects_a_symlinked_account_folder_that_escapes_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(outside.path(), base.join("Account")).unwrap();
        let layout = DownloadLayout::new(base).with_options(true, false);

        let error = layout
            .reserve("a", "file.txt", UNIX_EPOCH)
            .expect_err("an escaping symlink must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn base_directory_must_be_absolute_and_a_directory() {
        assert_eq!(
            valid_base_dir(" /tmp/downloads "),
            Some(PathBuf::from("/tmp/downloads"))
        );
        assert_eq!(valid_base_dir("relative/path"), None);
        assert_eq!(valid_base_dir(""), None);
        assert_eq!(valid_base_dir("/tmp/downloads\nrm -rf"), None);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert_eq!(valid_base_dir(file.to_str().unwrap()), None);
    }

    #[test]
    fn retry_only_accepts_finished_http_sources() {
        let mut record = record("a", DownloadStatus::Failed);
        assert_eq!(record.retry_url(), Some("https://example.com/file.txt"));

        record.status = DownloadStatus::Completed;
        assert_eq!(record.retry_url(), None);

        record.status = DownloadStatus::Cancelled;
        assert_eq!(record.retry_url(), Some("https://example.com/file.txt"));

        record.status = DownloadStatus::InProgress;
        assert_eq!(record.retry_url(), None);

        record.status = DownloadStatus::Failed;
        record.source_url = Some("blob:https://web.whatsapp.com/abc".to_string());
        assert_eq!(record.retry_url(), None);
        record.source_url = None;
        assert_eq!(record.retry_url(), None);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn progress_is_bounded() {
        let mut record = record("a", DownloadStatus::InProgress);
        record.total_bytes = Some(100);
        record.received_bytes = 50;
        assert_eq!(record.progress(), Some(0.5));
        record.received_bytes = 200;
        assert_eq!(record.progress(), Some(1.0));
        record.total_bytes = None;
        assert_eq!(record.progress(), None);
        record.total_bytes = Some(0);
        assert_eq!(record.progress(), None);
    }
}
