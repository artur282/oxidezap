use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

/// The checker contacts GitHub at most once per day.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/artur282/oxidezap/releases/latest";
pub const USER_AGENT: &str = concat!("OxideZap/", env!("CARGO_PKG_VERSION"));

const RELEASE_URL_PREFIX: &str = "https://github.com/artur282/oxidezap/releases/";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT_SECONDS: &str = "10";
const CONNECT_TIMEOUT_SECONDS: &str = "5";

/// A release returned by the GitHub API, or reconstructed from the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub url: String,
}

/// A newer release worth showing in Settings > About.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// Everything the About section needs to render the update state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateState {
    pub current_version: String,
    pub available: Option<UpdateInfo>,
    pub latest_version: Option<String>,
    pub skipped_version: Option<String>,
    pub last_checked: Option<u64>,
    pub development: bool,
}

/// Snapshot taken on the GPUI thread and handed to the update worker.
#[derive(Clone, Debug)]
pub struct CheckConfig {
    pub current_version: String,
    pub automatic: bool,
    pub skipped_version: Option<String>,
    pub cache_file: PathBuf,
}

impl CheckConfig {
    pub fn new(
        current_version: impl Into<String>,
        automatic: bool,
        skipped_version: Option<String>,
        cache_file: PathBuf,
    ) -> Self {
        Self {
            current_version: current_version.into(),
            automatic,
            skipped_version,
            cache_file,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<Identifier>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Version {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let value = value
            .strip_prefix('v')
            .or_else(|| value.strip_prefix('V'))
            .unwrap_or(value);
        if value.is_empty() {
            return None;
        }

        let (value, build) = match value.split_once('+') {
            Some((value, build)) => (value, Some(build)),
            None => (value, None),
        };
        if build.is_some_and(|build| !valid_build_metadata(build)) {
            return None;
        }

        let (core, prerelease) = match value.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (value, None),
        };

        let mut parts = core.split('.');
        let major = parse_core_number(parts.next()?)?;
        let minor = parse_core_number(parts.next()?)?;
        let patch = parse_core_number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }

        let prerelease = match prerelease {
            Some(prerelease) => parse_prerelease(prerelease)?,
            None => Vec::new(),
        };

        Some(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, identifier) in self.prerelease.iter().enumerate() {
            formatter.write_str(if index == 0 { "-" } else { "." })?;
            match identifier {
                Identifier::Numeric(number) => write!(formatter, "{number}")?,
                Identifier::Text(text) => formatter.write_str(text)?,
            }
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_prerelease(left: &[Identifier], right: &[Identifier]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }

    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (Identifier::Numeric(left), Identifier::Numeric(right)) => left.cmp(right),
            (Identifier::Numeric(_), Identifier::Text(_)) => Ordering::Less,
            (Identifier::Text(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Text(left), Identifier::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    left.len().cmp(&right.len())
}

fn parse_core_number(value: &str) -> Option<u64> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    value.parse().ok()
}

fn parse_prerelease(value: &str) -> Option<Vec<Identifier>> {
    if value.is_empty() {
        return None;
    }
    value
        .split('.')
        .map(|identifier| {
            if identifier.is_empty()
                || !identifier
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
            {
                return None;
            }
            if identifier
                .chars()
                .all(|character| character.is_ascii_digit())
            {
                if identifier.len() > 1 && identifier.starts_with('0') {
                    return None;
                }
                return Some(Identifier::Numeric(identifier.parse().ok()?));
            }
            Some(Identifier::Text(identifier.to_string()))
        })
        .collect()
}

fn valid_build_metadata(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
}

/// Compares the installed version with the latest GitHub release tag.
///
/// Returns `None` when either version is invalid, when the release is the same
/// version, or when it is older. Development and prerelease handling is left to
/// the caller so this function stays a pure comparison.
pub fn check_for_update(current: &str, latest: &Release) -> Option<UpdateInfo> {
    let current = Version::parse(current)?;
    let latest_version = Version::parse(&latest.tag)?;
    if latest_version <= current {
        return None;
    }
    Some(UpdateInfo {
        version: latest_version.to_string(),
        url: latest.url.clone(),
    })
}

/// A prerelease or a build with local metadata is not comparable with a
/// published release.
pub fn is_development_version(version: &str) -> bool {
    version.contains('+') || Version::parse(version).is_some_and(|version| version.is_prerelease())
}

fn is_development_build(config: &CheckConfig) -> bool {
    cfg!(debug_assertions)
        || is_development_version(env!("CARGO_PKG_VERSION"))
        || is_development_version(&config.current_version)
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
}

/// Parses the `releases/latest` payload. Invalid JSON, rate-limit responses,
/// and invalid tag names are ignored silently.
pub fn parse_latest_release(data: &[u8]) -> Option<Release> {
    if data.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    let response: GitHubRelease = serde_json::from_slice(data).ok()?;
    let tag = response.tag_name.trim();
    if tag.is_empty() || Version::parse(tag).is_none() {
        return None;
    }
    let url = match response.html_url {
        Some(url) if is_trusted_release_url(&url) => url,
        _ => release_page_url(tag),
    };
    Some(Release {
        tag: tag.to_string(),
        url,
    })
}

fn is_trusted_release_url(url: &str) -> bool {
    url.starts_with(RELEASE_URL_PREFIX)
}

fn release_page_url(tag: &str) -> String {
    format!("{RELEASE_URL_PREFIX}tag/{tag}")
}

/// A check is due when there is no previous check or the last one is at least
/// one day old. A clock that moved backwards never triggers a check loop.
pub fn should_check(last_checked: Option<u64>, now: u64) -> bool {
    last_checked.is_none_or(|last| now >= last.saturating_add(CHECK_INTERVAL_SECS))
}

/// Builds the state shown in Settings from the cached check result.
pub fn cached_state(config: &CheckConfig) -> UpdateState {
    let development = is_development_build(config);
    build_state(config, &load_cache(&config.cache_file), development)
}

/// Runs one throttled check. This performs network I/O and must run on a worker
/// thread. Every failure is silent: the previous cached state is kept.
pub fn run_check(config: &CheckConfig) -> UpdateState {
    let development = is_development_build(config);
    let mut cache = load_cache(&config.cache_file);
    let now = unix_now();

    if !development && config.automatic && should_check(cache.last_checked, now) {
        if let Some(release) = fetch_latest_release() {
            cache.latest = Some(CachedRelease::from(&release));
        }
        // Record the attempt even when it failed, so an offline launch does not
        // retry the request every time the application starts.
        cache.last_checked = Some(now);
        save_cache(&config.cache_file, &cache);
    }

    build_state(config, &cache, development)
}

/// Re-applies the current skip preference to a state that arrived from a worker
/// while the user was changing settings.
pub fn apply_skipped(mut state: UpdateState, skipped: Option<&str>) -> UpdateState {
    if state
        .available
        .as_ref()
        .is_some_and(|info| skipped == Some(info.version.as_str()))
    {
        state.available = None;
    }
    state.skipped_version = skipped.map(str::to_string);
    state
}

fn build_state(config: &CheckConfig, cache: &UpdateCache, development: bool) -> UpdateState {
    let latest_version = cache
        .latest
        .as_ref()
        .and_then(|cached| Version::parse(&cached.tag))
        .map(|version| version.to_string());
    let mut available = cache.latest.as_ref().and_then(|cached| {
        check_for_update(
            &config.current_version,
            &Release {
                tag: cached.tag.clone(),
                url: cached.url.clone(),
            },
        )
    });
    if development {
        available = None;
    }
    if available
        .as_ref()
        .is_some_and(|info| config.skipped_version.as_deref() == Some(info.version.as_str()))
    {
        available = None;
    }

    UpdateState {
        current_version: config.current_version.clone(),
        available,
        latest_version,
        skipped_version: config.skipped_version.clone(),
        last_checked: cache.last_checked,
        development,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct UpdateCache {
    #[serde(default)]
    last_checked: Option<u64>,
    #[serde(default)]
    latest: Option<CachedRelease>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct CachedRelease {
    tag: String,
    url: String,
}

impl From<&Release> for CachedRelease {
    fn from(release: &Release) -> Self {
        Self {
            tag: release.tag.clone(),
            url: release.url.clone(),
        }
    }
}

fn load_cache(path: &Path) -> UpdateCache {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(_) => return UpdateCache::default(),
    };
    serde_json::from_slice(&data).unwrap_or_default()
}

fn save_cache(path: &Path, cache: &UpdateCache) {
    let data = match serde_json::to_vec(cache) {
        Ok(data) => data,
        Err(error) => {
            debug!(%error, "could not serialize the update cache");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(error) = crate::paths::create_private_dir(parent) {
            debug!(%error, "could not create the update cache directory");
            return;
        }
    }
    let temporary = path.with_extension("json.tmp");
    if let Err(error) = crate::paths::write_private_file(&temporary, &data) {
        debug!(%error, "could not write the update cache");
        return;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        debug!(%error, "could not replace the update cache");
    }
}

fn fetch_latest_release() -> Option<Release> {
    let data = fetch_with_curl().or_else(fetch_with_wget)?;
    if data.is_empty() {
        return None;
    }
    parse_latest_release(&data)
}

/// Uses the desktop's `curl`, which keeps the Rust dependency graph unchanged
/// and uses the system TLS trust store. A missing binary falls through to wget.
fn fetch_with_curl() -> Option<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--fail",
            "--location",
            "--max-time",
            REQUEST_TIMEOUT_SECONDS,
            "--connect-timeout",
            CONNECT_TIMEOUT_SECONDS,
            "--proto",
            "=https",
            "--user-agent",
            USER_AGENT,
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            LATEST_RELEASE_URL,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        debug!(
            status = ?output.status.code(),
            "the update check request failed"
        );
        return None;
    }
    Some(output.stdout)
}

fn fetch_with_wget() -> Option<Vec<u8>> {
    let output = Command::new("wget")
        .args([
            "--quiet",
            "--output-document=-",
            "--timeout=10",
            "--tries=1",
            "--user-agent",
            USER_AGENT,
            "--header",
            "Accept: application/vnd.github+json",
            LATEST_RELEASE_URL,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        debug!(
            status = ?output.status.code(),
            "the update check request failed"
        );
        return None;
    }
    Some(output.stdout)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.to_string(),
            url: "https://github.com/artur282/oxidezap/releases/tag/test".to_string(),
        }
    }

    fn config(
        current: &str,
        automatic: bool,
        skipped: Option<&str>,
        cache_file: PathBuf,
    ) -> CheckConfig {
        CheckConfig::new(current, automatic, skipped.map(str::to_string), cache_file)
    }

    #[test]
    fn versions_compare_in_numeric_order() {
        let version = |tag: &str| Version::parse(tag).unwrap();
        assert!(version("1.0.0") > version("0.9.9"));
        assert!(version("0.10.0") > version("0.9.0"));
        assert!(version("1.2.10") > version("1.2.9"));
        assert_eq!(version("v1.2.3"), version("1.2.3"));
        assert_eq!(version("1.2.3+build.7"), version("1.2.3"));
    }

    #[test]
    fn prereleases_sort_before_the_final_release() {
        let version = |tag: &str| Version::parse(tag).unwrap();
        assert!(version("1.0.0-alpha") < version("1.0.0"));
        assert!(version("1.0.0-alpha") < version("1.0.0-alpha.1"));
        assert!(version("1.0.0-alpha.1") < version("1.0.0-alpha.beta"));
        assert!(version("1.0.0-alpha.beta") < version("1.0.0-beta"));
        assert!(version("1.0.0-beta") < version("1.0.0-beta.2"));
        assert!(version("1.0.0-beta.2") < version("1.0.0-beta.11"));
        assert!(version("1.0.0-beta.11") < version("1.0.0-rc.1"));
        assert!(version("1.0.0-rc.1") < version("1.0.0"));
        assert!(version("1.0.0-1") < version("1.0.0-alpha"));
        assert!(version("1.0.0-rc.2") < version("1.0.0-rc.10"));
    }

    #[test]
    fn invalid_versions_are_rejected() {
        for value in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1.2.3-",
            "1.2.3-01",
            "1.2.3+",
            "01.2.3",
            "1.2.3-rc..1",
            "1.2.3-rc_1",
        ] {
            assert!(Version::parse(value).is_none(), "{value}");
        }
    }

    #[test]
    fn newer_release_produces_update_info() {
        let info = check_for_update("0.1.0", &release("v0.2.0")).expect("an update");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(
            info.url,
            "https://github.com/artur282/oxidezap/releases/tag/test"
        );
    }

    #[test]
    fn same_or_older_release_is_not_an_update() {
        assert!(check_for_update("0.2.0", &release("v0.2.0")).is_none());
        assert!(check_for_update("0.2.0", &release("v0.1.9")).is_none());
        assert!(check_for_update("0.2.0", &release("not-a-version")).is_none());
    }

    #[test]
    fn development_versions_are_detected() {
        assert!(is_development_version("0.1.0-dev"));
        assert!(is_development_version("0.1.0-rc.1"));
        assert!(is_development_version("0.1.0+local"));
        assert!(!is_development_version("0.1.0"));
    }

    #[test]
    fn github_payload_is_parsed() {
        let data = br#"{"tag_name":"v0.2.0","html_url":"https://github.com/artur282/oxidezap/releases/tag/v0.2.0"}"#;
        let release = parse_latest_release(data).expect("a release");
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(
            release.url,
            "https://github.com/artur282/oxidezap/releases/tag/v0.2.0"
        );
    }

    #[test]
    fn untrusted_release_urls_fall_back_to_github() {
        let data = br#"{"tag_name":"v0.2.0","html_url":"https://evil.example/release"}"#;
        let release = parse_latest_release(data).expect("a release");
        assert_eq!(
            release.url,
            "https://github.com/artur282/oxidezap/releases/tag/v0.2.0"
        );
    }

    #[test]
    fn malformed_responses_are_ignored() {
        assert!(parse_latest_release(b"{}").is_none());
        assert!(parse_latest_release(b"not json").is_none());
        assert!(parse_latest_release(br#"{"message":"API rate limit exceeded"}"#).is_none());
        assert!(parse_latest_release(br#"{"tag_name":"","html_url":null}"#).is_none());
        assert!(parse_latest_release(br#"{"tag_name":"latest","html_url":null}"#).is_none());
    }

    #[test]
    fn checks_are_throttled_to_once_a_day() {
        assert!(should_check(None, NOW));
        assert!(!should_check(Some(NOW - 60), NOW));
        assert!(!should_check(Some(NOW - CHECK_INTERVAL_SECS + 1), NOW));
        assert!(should_check(Some(NOW - CHECK_INTERVAL_SECS), NOW));
        assert!(!should_check(Some(NOW + 60), NOW));
    }

    #[test]
    fn cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let cache = UpdateCache {
            last_checked: Some(NOW),
            latest: Some(CachedRelease {
                tag: "v0.2.0".into(),
                url: "https://example.test".into(),
            }),
        };
        save_cache(&path, &cache);
        assert_eq!(load_cache(&path), cache);

        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_cache(&path), UpdateCache::default());
    }

    #[test]
    fn cached_state_reports_an_available_update() {
        let cache = UpdateCache {
            last_checked: Some(NOW),
            latest: Some(CachedRelease {
                tag: "v0.2.0".into(),
                url: "https://example.test".into(),
            }),
        };
        let state = build_state(&config("0.1.0", true, None, PathBuf::new()), &cache, false);
        assert_eq!(state.available.expect("update").version, "0.2.0");
        assert_eq!(state.latest_version.as_deref(), Some("0.2.0"));
        assert_eq!(state.last_checked, Some(NOW));
    }

    #[test]
    fn cached_state_hides_the_skipped_version() {
        let cache = UpdateCache {
            last_checked: Some(NOW),
            latest: Some(CachedRelease {
                tag: "v0.2.0".into(),
                url: "https://example.test".into(),
            }),
        };
        let state = build_state(
            &config("0.1.0", true, Some("0.2.0"), PathBuf::new()),
            &cache,
            false,
        );
        assert!(state.available.is_none());
        assert_eq!(state.skipped_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn a_newer_release_is_shown_after_the_skipped_one() {
        let cache = UpdateCache {
            last_checked: Some(NOW),
            latest: Some(CachedRelease {
                tag: "v0.3.0".into(),
                url: "https://example.test".into(),
            }),
        };
        let state = build_state(
            &config("0.1.0", true, Some("0.2.0"), PathBuf::new()),
            &cache,
            false,
        );
        assert_eq!(state.available.expect("update").version, "0.3.0");
    }

    #[test]
    fn development_builds_do_not_report_an_update() {
        let cache = UpdateCache {
            last_checked: Some(NOW),
            latest: Some(CachedRelease {
                tag: "v0.2.0".into(),
                url: "https://example.test".into(),
            }),
        };
        let state = build_state(&config("0.1.0", true, None, PathBuf::new()), &cache, true);
        assert!(state.development);
        assert!(state.available.is_none());
    }

    #[test]
    fn applying_a_skip_removes_the_in_flight_update() {
        let state = UpdateState {
            current_version: "0.1.0".into(),
            available: Some(UpdateInfo {
                version: "0.2.0".into(),
                url: "https://example.test".into(),
            }),
            latest_version: Some("0.2.0".into()),
            skipped_version: None,
            last_checked: Some(NOW),
            development: false,
        };
        let state = apply_skipped(state, Some("0.2.0"));
        assert!(state.available.is_none());
        assert_eq!(state.skipped_version.as_deref(), Some("0.2.0"));
    }
}
