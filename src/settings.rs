use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

/// On-disk account storage inspection and cleanup. The implementation lives in
/// `src/storage.rs`; declaring it here keeps the settings window, the app model,
/// and the WebKit thread on the same module without a crate-root declaration.
#[path = "storage.rs"]
pub mod storage;

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
/// Global shortcut that hides every window for immediate privacy; a second
/// press restores the windows that were visible when it fired.
pub const DEFAULT_BOSS_KEY: &str = "Ctrl+Shift+H";
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LockSettings {
    pub enabled: bool,
    pub hash: Option<String>,
    pub lock_on_launch: bool,
    pub lock_on_hide: bool,
    /// Best-effort lock when logind reports `PrepareForSleep`. Enabled by
    /// default because a suspended session exposes the content when resumed.
    pub lock_on_suspend: bool,
    /// Best-effort lock when a desktop ScreenSaver implementation reports an
    /// active lock. Enabled by default; desktops without the interface simply
    /// never send the signal.
    pub lock_on_screen_lock: bool,
    pub idle_minutes: u32,
}

impl Default for LockSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hash: None,
            lock_on_launch: false,
            lock_on_hide: false,
            lock_on_suspend: true,
            lock_on_screen_lock: true,
            idle_minutes: 0,
        }
    }
}

/// Languages offered for the WhatsApp composer. `Automatic` delegates to the
/// desktop locale; `Disabled` turns WebKitGTK spell checking off without
/// discarding the other writing preferences.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpellCheckLanguage {
    #[default]
    Automatic,
    Spanish,
    English,
    Disabled,
}

impl SpellCheckLanguage {
    pub fn label(self) -> &'static str {
        match self {
            SpellCheckLanguage::Automatic => "Automatic",
            SpellCheckLanguage::Spanish => "Spanish",
            SpellCheckLanguage::English => "English",
            SpellCheckLanguage::Disabled => "Disabled",
        }
    }

    /// Parses a persisted or locale-style identifier. Unknown values fall back
    /// to `Automatic` instead of discarding the whole settings file.
    pub fn parse(value: &str) -> Self {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        let language = normalized.split(['.', '_']).next().unwrap_or_default();
        match language {
            "es" | "spanish" => SpellCheckLanguage::Spanish,
            "en" | "english" => SpellCheckLanguage::English,
            "disabled" | "off" | "none" => SpellCheckLanguage::Disabled,
            _ => SpellCheckLanguage::Automatic,
        }
    }

    /// Locale identifiers passed to WebKitGTK. An empty list means the
    /// desktop locale, which is resolved by `WritingSettings`.
    pub fn codes(self) -> &'static [&'static str] {
        match self {
            SpellCheckLanguage::Automatic | SpellCheckLanguage::Disabled => &[],
            SpellCheckLanguage::Spanish => &["es"],
            SpellCheckLanguage::English => &["en"],
        }
    }
}

impl<'de> Deserialize<'de> for SpellCheckLanguage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::parse(&value))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WritingSettings {
    /// The "Correct while I type" toggle. It stays independent from the
    /// language so turning it off does not erase the selected language.
    pub spell_check: bool,
    pub language: SpellCheckLanguage,
}

impl Default for WritingSettings {
    fn default() -> Self {
        Self {
            spell_check: true,
            language: SpellCheckLanguage::Automatic,
        }
    }
}

impl WritingSettings {
    /// Selects a language. Choosing `Disabled` turns checking off; choosing
    /// any other language turns it back on so the two controls cannot leave
    /// the UI in a state where the toggle looks enabled but nothing is
    /// checked.
    pub fn set_language(&mut self, language: SpellCheckLanguage) {
        self.language = language;
        self.spell_check = language != SpellCheckLanguage::Disabled;
    }

    /// Toggles checking. Turning it back on while the language is `Disabled`
    /// falls back to `Automatic` instead of leaving checking silently off.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.spell_check = enabled;
        if enabled && self.language == SpellCheckLanguage::Disabled {
            self.language = SpellCheckLanguage::Automatic;
        }
    }

    pub fn normalize(&mut self) {
        if self.language == SpellCheckLanguage::Disabled {
            self.spell_check = false;
        }
    }

    pub fn spell_checking_enabled(&self) -> bool {
        self.spell_check && self.language != SpellCheckLanguage::Disabled
    }

    /// Resolves the language list for WebKitGTK. Automatic uses the desktop
    /// locale list, filtered to identifiers the platform spell checker can
    /// load; explicit choices use the ISO-639 codes.
    pub fn spell_checking_languages(&self, system_locales: &[String]) -> Vec<String> {
        if !self.spell_checking_enabled() {
            return Vec::new();
        }
        match self.language {
            SpellCheckLanguage::Automatic => normalize_locales(system_locales),
            language => language
                .codes()
                .iter()
                .map(|code| (*code).to_string())
                .collect(),
        }
    }
}

/// How WhatsApp Web reaches the network.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Follow the desktop's proxy configuration (WebKitGTK default).
    #[default]
    System,
    /// Ignore the desktop proxy and connect directly.
    #[serde(rename = "none")]
    NoProxy,
    /// Use the endpoint configured in [`ManualProxy`].
    Manual,
}

impl ProxyMode {
    pub fn label(self) -> &'static str {
        match self {
            ProxyMode::System => "System proxy",
            ProxyMode::NoProxy => "No proxy",
            ProxyMode::Manual => "Manual proxy",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ProxyMode::System => "Use the desktop network settings",
            ProxyMode::NoProxy => "Always connect directly",
            ProxyMode::Manual => "Enter an HTTP, HTTPS, or SOCKS proxy",
        }
    }
}

/// Protocol spoken between OxideZap and the proxy.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyProtocol {
    #[default]
    Http,
    Https,
    Socks,
}

impl ProxyProtocol {
    pub fn label(self) -> &'static str {
        match self {
            ProxyProtocol::Http => "HTTP",
            ProxyProtocol::Https => "HTTPS",
            ProxyProtocol::Socks => "SOCKS",
        }
    }

    pub fn scheme(self) -> &'static str {
        match self {
            ProxyProtocol::Http => "http",
            ProxyProtocol::Https => "https",
            ProxyProtocol::Socks => "socks",
        }
    }
}

/// A manually configured proxy endpoint. The password is stored in
/// `settings.json` because this build has no keyring integration. The file is
/// private (`0600`) and exports strip the credentials, but the value is still
/// plain text on disk; the Settings UI warns about that explicitly.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ManualProxy {
    pub protocol: ProxyProtocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Default for ManualProxy {
    fn default() -> Self {
        Self {
            protocol: ProxyProtocol::Http,
            host: String::new(),
            port: Self::DEFAULT_PORT,
            username: String::new(),
            password: String::new(),
        }
    }
}

impl ManualProxy {
    pub const DEFAULT_PORT: u16 = 8080;

    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty()
    }

    /// Trims the host and username and drops a password that has no username,
    /// so an abandoned edit cannot leave an orphan secret in the file.
    pub fn normalize(&mut self) {
        self.host = self.host.trim().to_string();
        self.username = self.username.trim().to_string();
        if self.username.is_empty() {
            self.password.clear();
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_proxy_host(&self.host)?;
        if self.port == 0 {
            return Err("The proxy port must be between 1 and 65535.".to_string());
        }
        if self.username.is_empty() && !self.password.is_empty() {
            return Err("Enter a proxy username when a password is set.".to_string());
        }
        Ok(())
    }

    /// Builds the URI passed to WebKitGTK. Credentials are percent-encoded and
    /// only ever go to the proxy engine; callers must log
    /// [`Self::redacted_endpoint`] instead.
    pub fn uri(&self) -> Result<String, String> {
        self.validate()?;
        let mut authority = String::new();
        if !self.username.is_empty() {
            authority.push_str(&percent_encode(&self.username));
            authority.push(':');
            authority.push_str(&percent_encode(&self.password));
            authority.push('@');
        }
        authority.push_str(self.host.trim());
        authority.push(':');
        authority.push_str(&self.port.to_string());
        let uri = format!("{}://{authority}", self.protocol.scheme());
        url::Url::parse(&uri).map_err(|_| "The proxy address is not a valid URL.".to_string())?;
        Ok(uri)
    }

    pub fn redacted_endpoint(&self) -> String {
        if !self.is_configured() {
            "not configured".to_string()
        } else {
            format!("{}:{}", display_host(&self.host), self.port)
        }
    }

    fn endpoint(&self) -> Result<ProxyEndpoint, String> {
        self.validate()?;
        Ok(ProxyEndpoint {
            protocol: self.protocol,
            host: self.host.trim().to_string(),
            port: self.port,
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }
}

/// Redacts credentials so no log statement can accidentally print them.
impl fmt::Debug for ManualProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManualProxy")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "username",
                &(!self.username.is_empty()).then_some("<redacted>"),
            )
            .field(
                "password",
                &(!self.password.is_empty()).then_some("<redacted>"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkSettings {
    pub proxy: ProxySettings,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub manual: ManualProxy,
}

impl ProxySettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.mode == ProxyMode::Manual {
            self.manual.validate()?;
        }
        Ok(())
    }

    /// The custom proxy URI for WebKitGTK, or `None` when WebKit should use
    /// the system proxy or no proxy at all.
    pub fn uri(&self) -> Result<Option<String>, String> {
        match self.mode {
            ProxyMode::System | ProxyMode::NoProxy => Ok(None),
            ProxyMode::Manual => self.manual.uri().map(Some),
        }
    }

    /// Safe for logs and UI summaries: never contains a username or password.
    pub fn redacted_summary(&self) -> String {
        match self.mode {
            ProxyMode::System => "System proxy".to_string(),
            ProxyMode::NoProxy => "No proxy".to_string(),
            ProxyMode::Manual => format!(
                "{} proxy at {}",
                self.manual.protocol.label(),
                self.manual.redacted_endpoint()
            ),
        }
    }
}

fn validate_proxy_host(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Enter the proxy host.".to_string());
    }
    if host.starts_with('[') || host.ends_with(']') {
        if !(host.starts_with('[') && host.ends_with(']')) {
            return Err("Close the IPv6 address with a bracket, for example [::1].".to_string());
        }
        let inner = &host[1..host.len() - 1];
        let valid = !inner.is_empty()
            && inner
                .chars()
                .all(|character| character.is_ascii_hexdigit() || matches!(character, ':' | '.'));
        if !valid {
            return Err("Enter a valid IPv6 address in brackets, for example [::1].".to_string());
        }
        return Ok(());
    }
    if host.contains(':') {
        return Err("Wrap IPv6 addresses in brackets, for example [::1].".to_string());
    }
    if host.chars().any(|character| {
        character.is_whitespace() || matches!(character, '/' | '\\' | '@' | '?' | '#')
    }) {
        return Err(
            "The proxy host must be a hostname or IP address, without a scheme, path, or credentials."
                .to_string(),
        );
    }
    Ok(())
}

fn display_host(host: &str) -> String {
    let host = host.trim();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Endpoint used by the connection probe. This mirrors [`ManualProxy`] and
/// also holds endpoints resolved from the desktop proxy settings.
#[derive(Clone, PartialEq, Eq)]
struct ProxyEndpoint {
    protocol: ProxyProtocol,
    host: String,
    port: u16,
    username: String,
    password: String,
}

impl ProxyEndpoint {
    fn redacted_label(&self) -> String {
        format!(
            "{}://{}:{}",
            self.protocol.scheme(),
            display_host(&self.host),
            self.port
        )
    }
}

fn parse_proxy_uri(uri: &str) -> Result<ProxyEndpoint, String> {
    let url = url::Url::parse(uri)
        .map_err(|_| "The desktop proxy settings contain an invalid proxy URI.".to_string())?;
    let protocol = match url.scheme() {
        "http" => ProxyProtocol::Http,
        "https" => ProxyProtocol::Https,
        "socks" | "socks4" | "socks4a" | "socks5" | "socks5h" => ProxyProtocol::Socks,
        scheme => {
            return Err(format!(
                "The desktop proxy uses an unsupported scheme: {scheme}."
            ));
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| "The desktop proxy URI has no host.".to_string())?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let default_port = match protocol {
        ProxyProtocol::Http => 80,
        ProxyProtocol::Https => 443,
        ProxyProtocol::Socks => 1080,
    };
    let port = url.port().unwrap_or(default_port);
    if port == 0 {
        return Err("The desktop proxy URI uses port 0.".to_string());
    }
    Ok(ProxyEndpoint {
        protocol,
        host,
        port,
        username: percent_decode(url.username()),
        password: percent_decode(url.password().unwrap_or_default()),
    })
}

const PROXY_TEST_HOST: &str = "web.whatsapp.com";
const PROXY_TEST_PORT: u16 = 443;
const PROXY_TEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Exercises the configured proxy with a blocking network probe. Callers must
/// run this on a background thread so the GPUI frame loop is never blocked.
/// The result is safe to show and log: it never contains credentials.
pub fn test_proxy_connection(proxy: &ProxySettings) -> Result<String, String> {
    match proxy.mode {
        ProxyMode::System => {
            let uris = system_proxy_uris()?;
            let endpoint = uris
                .iter()
                .filter(|uri| !uri.as_str().starts_with("direct:"))
                .map(|uri| parse_proxy_uri(uri))
                .next()
                .transpose()?;
            match endpoint {
                Some(endpoint) => Ok(format!(
                    "{} Resolved from the desktop system proxy settings.",
                    probe_endpoint(&endpoint)?
                )),
                None => {
                    direct_probe()?;
                    Ok(
                        "Connected directly to web.whatsapp.com:443. The desktop has no system proxy configured."
                            .to_string(),
                    )
                }
            }
        }
        ProxyMode::NoProxy => {
            direct_probe()?;
            Ok("Connected directly to web.whatsapp.com:443. No proxy is configured.".to_string())
        }
        ProxyMode::Manual => probe_endpoint(&proxy.manual.endpoint()?),
    }
}

fn system_proxy_uris() -> Result<Vec<String>, String> {
    use gtk::gio::prelude::ProxyResolverExt;

    let resolver = gtk::gio::ProxyResolver::default();
    let uris = resolver
        .lookup("https://web.whatsapp.com", gtk::gio::Cancellable::NONE)
        .map_err(|error| format!("Could not read the desktop proxy settings: {error}"))?;
    Ok(uris.iter().map(ToString::to_string).collect())
}

fn probe_endpoint(endpoint: &ProxyEndpoint) -> Result<String, String> {
    match endpoint.protocol {
        ProxyProtocol::Http => {
            http_connect_probe(endpoint)?;
            Ok(format!(
                "The HTTP proxy at {} accepted a CONNECT tunnel to web.whatsapp.com:443.",
                endpoint.redacted_label()
            ))
        }
        ProxyProtocol::Https => {
            tcp_connect(&endpoint.host, endpoint.port, "the HTTPS proxy")?;
            Ok(format!(
                "The HTTPS proxy at {} is reachable. The test stops before the TLS handshake; opening an account verifies the full connection.",
                endpoint.redacted_label()
            ))
        }
        ProxyProtocol::Socks => {
            socks5_probe(endpoint)?;
            Ok(format!(
                "The SOCKS proxy at {} connected to web.whatsapp.com:443.",
                endpoint.redacted_label()
            ))
        }
    }
}

fn direct_probe() -> Result<(), String> {
    tcp_connect(PROXY_TEST_HOST, PROXY_TEST_PORT, PROXY_TEST_HOST)?;
    Ok(())
}

fn tcp_connect(host: &str, port: u16, label: &str) -> Result<TcpStream, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("Could not resolve {label} ({host}): {error}"))?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, PROXY_TEST_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(PROXY_TEST_TIMEOUT));
                let _ = stream.set_write_timeout(Some(PROXY_TEST_TIMEOUT));
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(format!(
        "Could not reach {label} at {host}:{port}: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no address was returned".to_string())
    ))
}

fn http_connect_probe(endpoint: &ProxyEndpoint) -> Result<(), String> {
    let mut stream = tcp_connect(&endpoint.host, endpoint.port, "the HTTP proxy")?;
    let mut request = format!(
        "CONNECT {PROXY_TEST_HOST}:{PROXY_TEST_PORT} HTTP/1.1\r\nHost: {PROXY_TEST_HOST}:{PROXY_TEST_PORT}\r\nProxy-Connection: keep-alive\r\n"
    );
    if !endpoint.username.is_empty() {
        let credentials = format!("{}:{}", endpoint.username, endpoint.password);
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64_encode(credentials.as_bytes())
        ));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("Could not send the CONNECT request to the proxy: {error}"))?;

    let mut response = Vec::with_capacity(1024);
    let mut buffer = [0u8; 512];
    while response.len() < 8192 {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("The proxy did not answer the CONNECT request: {error}"))?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let status = parse_http_status(&response)
        .ok_or_else(|| "The proxy returned an unexpected response to CONNECT.".to_string())?;
    match status {
        200..=299 => Ok(()),
        407 if endpoint.username.is_empty() => {
            Err("The proxy requires a username and password (HTTP 407).".to_string())
        }
        407 => Err("The proxy rejected the credentials (HTTP 407).".to_string()),
        _ => Err(format!(
            "The proxy returned HTTP {status} while opening the tunnel to web.whatsapp.com."
        )),
    }
}

fn parse_http_status(response: &[u8]) -> Option<u16> {
    let response = String::from_utf8_lossy(response);
    let line = response.lines().next()?;
    let mut parts = line.split_whitespace();
    if !parts.next()?.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let bits = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        encoded.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(bits >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[bits as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
}

fn socks5_probe(endpoint: &ProxyEndpoint) -> Result<(), String> {
    let mut stream = tcp_connect(&endpoint.host, endpoint.port, "the SOCKS proxy")?;
    let has_credentials = !endpoint.username.is_empty();
    let mut greeting = vec![0x05];
    if has_credentials {
        greeting.extend_from_slice(&[0x02, 0x00]);
    } else {
        greeting.push(0x00);
    }
    stream
        .write_all(&greeting)
        .map_err(|error| format!("Could not start the SOCKS negotiation: {error}"))?;
    let mut selection = [0u8; 2];
    stream
        .read_exact(&mut selection)
        .map_err(|error| format!("The SOCKS proxy did not answer: {error}"))?;
    if selection[0] != 0x05 {
        return Err("The proxy did not speak SOCKS5.".to_string());
    }
    match selection[1] {
        0x00 => {}
        0x02 if has_credentials => {
            let username = endpoint.username.as_bytes();
            let password = endpoint.password.as_bytes();
            if username.len() > 255 || password.len() > 255 {
                return Err(
                    "The SOCKS username and password must each be at most 255 bytes.".to_string(),
                );
            }
            let mut auth = vec![0x01, username.len() as u8];
            auth.extend_from_slice(username);
            auth.push(password.len() as u8);
            auth.extend_from_slice(password);
            stream
                .write_all(&auth)
                .map_err(|error| format!("Could not authenticate with the SOCKS proxy: {error}"))?;
            let mut reply = [0u8; 2];
            stream
                .read_exact(&mut reply)
                .map_err(|error| format!("The SOCKS proxy did not answer the login: {error}"))?;
            if reply[1] != 0x00 {
                return Err("The SOCKS proxy rejected the username or password.".to_string());
            }
        }
        0xff => {
            return Err("The SOCKS proxy rejected the offered authentication methods.".to_string());
        }
        _ if has_credentials => {
            return Err(
                "The SOCKS proxy did not accept username and password authentication.".to_string(),
            );
        }
        _ => return Err("The SOCKS proxy requires authentication.".to_string()),
    }

    let host = PROXY_TEST_HOST.as_bytes();
    let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    request.extend_from_slice(host);
    request.extend_from_slice(&PROXY_TEST_PORT.to_be_bytes());
    stream
        .write_all(&request)
        .map_err(|error| format!("Could not send the SOCKS CONNECT request: {error}"))?;

    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .map_err(|error| format!("The SOCKS proxy did not answer CONNECT: {error}"))?;
    if header[0] != 0x05 {
        return Err("The proxy did not speak SOCKS5.".to_string());
    }
    if header[1] != 0x00 {
        return Err(socks_error(header[1]));
    }
    let address_length = match header[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut length = [0u8; 1];
            stream
                .read_exact(&mut length)
                .map_err(|error| format!("Could not read the SOCKS address: {error}"))?;
            usize::from(length[0])
        }
        _ => return Err("The SOCKS proxy returned an unknown address type.".to_string()),
    };
    let mut address_and_port = vec![0u8; address_length + 2];
    stream
        .read_exact(&mut address_and_port)
        .map_err(|error| format!("Could not read the SOCKS reply: {error}"))?;
    Ok(())
}

fn socks_error(code: u8) -> String {
    let message = match code {
        0x01 => "The SOCKS proxy reported a general failure.",
        0x02 => "The SOCKS proxy denied the connection.",
        0x03 => "The SOCKS proxy could not reach the network.",
        0x04 => "The SOCKS proxy could not reach web.whatsapp.com.",
        0x05 => "The SOCKS proxy refused the connection to web.whatsapp.com.",
        0x06 => "The SOCKS proxy connection timed out.",
        0x07 => "The SOCKS proxy does not support CONNECT.",
        0x08 => "The SOCKS proxy does not support this address type.",
        _ => "The SOCKS proxy returned an unknown error.",
    };
    message.to_string()
}

/// Converts GLib locale names such as `es_AR.UTF-8` into the `lang_COUNTRY`
/// identifiers WebKitGTK expects, preserving preference order and dropping
/// the placeholder `C`/`POSIX` locales.
fn normalize_locales(locales: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::new();
    for locale in locales {
        let name = locale.split(['.', '@']).next().unwrap_or_default().trim();
        if name.is_empty() || name == "C" || name == "POSIX" {
            continue;
        }
        if !normalized.iter().any(|existing| existing == name) {
            normalized.push(name.to_string());
        }
    }
    normalized
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
    pub connection_notifications: bool,
    pub do_not_disturb: bool,
    pub close_to_tray: bool,
    pub start_at_login: bool,
    pub automatic_update_checks: bool,
    pub skipped_update_version: Option<String>,
    /// RFC 3339 UTC timestamp of the last successful storage cleanup, shown in
    /// Settings > Storage. `None` means the user has never cleaned anything.
    pub last_storage_cleanup: Option<String>,
    pub downloads_directory: Option<String>,
    pub downloads_account_folders: bool,
    pub downloads_date_folders: bool,
    pub remember_bounds: bool,
    pub shortcut: String,
    pub boss_key: String,
    pub lock: LockSettings,
    pub writing: WritingSettings,
    pub network: NetworkSettings,
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
            connection_notifications: true,
            do_not_disturb: false,
            close_to_tray: true,
            start_at_login: false,
            automatic_update_checks: true,
            skipped_update_version: None,
            last_storage_cleanup: None,
            downloads_directory: None,
            downloads_account_folders: true,
            downloads_date_folders: false,
            remember_bounds: true,
            shortcut: DEFAULT_SHORTCUT.to_string(),
            boss_key: DEFAULT_BOSS_KEY.to_string(),
            lock: LockSettings::default(),
            writing: WritingSettings::default(),
            network: NetworkSettings::default(),
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
        if self.boss_key.trim().is_empty() {
            self.boss_key = DEFAULT_BOSS_KEY.to_string();
        }
        if self.boss_key.trim() == self.shortcut.trim() {
            self.boss_key = if self.shortcut.trim() == DEFAULT_BOSS_KEY {
                DEFAULT_SHORTCUT.to_string()
            } else {
                DEFAULT_BOSS_KEY.to_string()
            };
        }
        self.lock.idle_minutes = self.lock.idle_minutes.min(240);
        if self.lock.hash.is_none() {
            self.lock.enabled = false;
        }
        self.writing.normalize();
        self.network.proxy.manual.normalize();
        if let Some(directory) = self.downloads_directory.take() {
            self.downloads_directory = crate::paths::valid_directory_setting(&directory)
                .map(|path| path.to_string_lossy().to_string());
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

    /// Connection notifications are opt-in on top of the global notification
    /// switch and obey the same do-not-disturb and per-account mute rules as
    /// message notifications.
    pub fn connection_notifications_enabled(&self, account_id: &str) -> bool {
        self.connection_notifications && self.message_notifications_enabled(account_id)
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
        scrub_proxy_credentials(&mut settings);
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
    settings.lock.lock_on_suspend = false;
    settings.lock.lock_on_screen_lock = false;
    settings.lock.idle_minutes = 0;
}

/// Proxy credentials stay in the main settings file but never travel through
/// an export or an import. Automatic backups still contain them, just like the
/// app-lock hash, and are written with `0600` permissions.
fn scrub_proxy_credentials(settings: &mut Settings) {
    settings.network.proxy.manual.username.clear();
    settings.network.proxy.manual.password.clear();
}

fn exportable_settings(settings: &Settings) -> Settings {
    let mut settings = settings.clone();
    scrub_lock_password(&mut settings);
    scrub_proxy_credentials(&mut settings);
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
    if current.connection_notifications != imported.connection_notifications {
        changes.push(format!(
            "Connection notifications: {}",
            on_off(imported.connection_notifications)
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
    if current.boss_key != imported.boss_key {
        changes.push(format!(
            "Boss key: {} -> {}",
            current.boss_key, imported.boss_key
        ));
    }
    if current.network.proxy.redacted_summary() != imported.network.proxy.redacted_summary() {
        changes.push(format!(
            "Network proxy: {} -> {}",
            current.network.proxy.redacted_summary(),
            imported.network.proxy.redacted_summary()
        ));
    } else if current.network.proxy.manual.username != imported.network.proxy.manual.username
        || current.network.proxy.manual.password != imported.network.proxy.manual.password
    {
        changes.push("Network proxy credentials: removed".to_string());
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
    if value { "on" } else { "off" }
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
        assert!(settings.connection_notifications);
        assert!(!settings.do_not_disturb);
        assert!(settings.close_to_tray);
        assert!(!settings.lock.enabled);
    }

    #[test]
    fn lock_on_suspend_and_screen_lock_default_to_enabled() {
        let settings = Settings::default();
        assert!(settings.lock.lock_on_suspend);
        assert!(settings.lock.lock_on_screen_lock);
    }

    #[test]
    fn legacy_lock_settings_enable_suspend_and_screen_lock() {
        let json = br#"{"version":1,"accounts":[],"lock":{"enabled":true,"hash":"$argon2id$v=19$m=19456,t=2,p=1$abc$def"}}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert!(settings.lock.lock_on_suspend);
        assert!(settings.lock.lock_on_screen_lock);
    }

    #[test]
    fn boss_key_defaults_and_normalizes() {
        let mut settings = Settings::default();
        assert_eq!(settings.boss_key, DEFAULT_BOSS_KEY);

        settings.boss_key = "   ".to_string();
        settings.normalize();
        assert_eq!(settings.boss_key, DEFAULT_BOSS_KEY);

        settings.boss_key = DEFAULT_SHORTCUT.to_string();
        settings.normalize();
        assert_eq!(settings.boss_key, DEFAULT_BOSS_KEY);

        settings.shortcut = DEFAULT_BOSS_KEY.to_string();
        settings.normalize();
        assert_eq!(settings.boss_key, DEFAULT_SHORTCUT);
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
    fn connection_notifications_are_optional_and_respect_quiet_modes() {
        let mut settings = Settings::default();
        settings.ensure_first_account();
        let id = settings.accounts[0].id.clone();

        assert!(settings.connection_notifications_enabled(&id));

        settings.connection_notifications = false;
        assert!(!settings.connection_notifications_enabled(&id));

        settings.connection_notifications = true;
        assert!(settings.set_do_not_disturb(true));
        assert!(!settings.connection_notifications_enabled(&id));

        assert!(settings.set_do_not_disturb(false));
        assert!(settings.set_muted(&id, true));
        assert!(!settings.connection_notifications_enabled(&id));

        assert!(settings.set_muted(&id, false));
        settings.notifications = false;
        assert!(!settings.connection_notifications_enabled(&id));
        assert!(!settings.connection_notifications_enabled("missing"));
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
        assert!(settings.connection_notifications);
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
    fn storage_cleanup_timestamp_defaults_to_none_and_round_trips() {
        let json = br#"{"version":1,"accounts":[{"id":"a","name":"A"}]}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();
        assert!(settings.last_storage_cleanup.is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.ensure_first_account();
        settings.last_storage_cleanup = Some("2026-11-12T10:00:00Z".to_string());
        settings.save(&path).unwrap();

        assert_eq!(
            Settings::load(&path)
                .unwrap()
                .last_storage_cleanup
                .as_deref(),
            Some("2026-11-12T10:00:00Z")
        );
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
    fn spell_check_language_parses_locale_identifiers() {
        assert_eq!(
            SpellCheckLanguage::parse("automatic"),
            SpellCheckLanguage::Automatic
        );
        assert_eq!(
            SpellCheckLanguage::parse("auto"),
            SpellCheckLanguage::Automatic
        );
        assert_eq!(SpellCheckLanguage::parse("es"), SpellCheckLanguage::Spanish);
        assert_eq!(
            SpellCheckLanguage::parse("es_AR"),
            SpellCheckLanguage::Spanish
        );
        assert_eq!(
            SpellCheckLanguage::parse("es-419"),
            SpellCheckLanguage::Spanish
        );
        assert_eq!(
            SpellCheckLanguage::parse("Spanish"),
            SpellCheckLanguage::Spanish
        );
        assert_eq!(
            SpellCheckLanguage::parse("en_US"),
            SpellCheckLanguage::English
        );
        assert_eq!(
            SpellCheckLanguage::parse("en-GB"),
            SpellCheckLanguage::English
        );
        assert_eq!(
            SpellCheckLanguage::parse("English"),
            SpellCheckLanguage::English
        );
        assert_eq!(
            SpellCheckLanguage::parse("disabled"),
            SpellCheckLanguage::Disabled
        );
        assert_eq!(
            SpellCheckLanguage::parse("off"),
            SpellCheckLanguage::Disabled
        );
        assert_eq!(
            SpellCheckLanguage::parse("klingon"),
            SpellCheckLanguage::Automatic
        );
        assert_eq!(SpellCheckLanguage::parse(""), SpellCheckLanguage::Automatic);
    }

    #[test]
    fn locale_normalization_strips_encodings_and_modifiers() {
        let locales = vec![
            "es_AR.UTF-8".to_string(),
            "es_AR".to_string(),
            "sr_RS@latin".to_string(),
            "C".to_string(),
            "POSIX".to_string(),
            "".to_string(),
            "es_AR".to_string(),
        ];

        assert_eq!(
            normalize_locales(&locales),
            vec!["es_AR".to_string(), "sr_RS".to_string()]
        );
    }

    #[test]
    fn spell_check_language_selection_maps_to_webkit_locales() {
        let system = vec![
            "es_AR.UTF-8".to_string(),
            "es_AR".to_string(),
            "es".to_string(),
            "C".to_string(),
        ];

        let automatic = WritingSettings::default();
        assert_eq!(
            automatic.spell_checking_languages(&system),
            vec!["es_AR".to_string(), "es".to_string()]
        );

        let spanish = WritingSettings {
            language: SpellCheckLanguage::Spanish,
            ..WritingSettings::default()
        };
        assert_eq!(
            spanish.spell_checking_languages(&system),
            vec!["es".to_string()]
        );

        let english = WritingSettings {
            language: SpellCheckLanguage::English,
            ..WritingSettings::default()
        };
        assert_eq!(
            english.spell_checking_languages(&system),
            vec!["en".to_string()]
        );

        let disabled = WritingSettings {
            language: SpellCheckLanguage::Disabled,
            ..WritingSettings::default()
        };
        assert!(!disabled.spell_checking_enabled());
        assert!(disabled.spell_checking_languages(&system).is_empty());
    }

    #[test]
    fn disabling_spell_check_keeps_the_selected_language() {
        let writing = WritingSettings {
            spell_check: false,
            language: SpellCheckLanguage::Spanish,
        };
        let system = vec!["es_ES".to_string()];

        assert!(!writing.spell_checking_enabled());
        assert!(writing.spell_checking_languages(&system).is_empty());
        assert_eq!(writing.language, SpellCheckLanguage::Spanish);
    }

    #[test]
    fn selecting_the_disabled_language_turns_spell_check_off() {
        let mut writing = WritingSettings::default();

        writing.set_language(SpellCheckLanguage::Disabled);

        assert!(!writing.spell_check);
        assert!(!writing.spell_checking_enabled());

        writing.set_enabled(true);

        assert_eq!(writing.language, SpellCheckLanguage::Automatic);
        assert!(writing.spell_checking_enabled());
    }

    #[test]
    fn normalize_turns_off_checking_for_the_disabled_language() {
        let mut writing = WritingSettings {
            spell_check: true,
            language: SpellCheckLanguage::Disabled,
        };

        writing.normalize();

        assert!(!writing.spell_check);
        assert!(!writing.spell_checking_enabled());
    }

    #[test]
    fn legacy_settings_default_to_automatic_spell_check() {
        let json = br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a"}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert!(settings.writing.spell_check);
        assert_eq!(settings.writing.language, SpellCheckLanguage::Automatic);
    }

    #[test]
    fn legacy_settings_file_loads_with_default_writing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a"}"#,
        )
        .unwrap();

        let settings = Settings::load(&path).unwrap();

        assert!(settings.writing.spell_check);
        assert_eq!(settings.writing.language, SpellCheckLanguage::Automatic);
    }

    #[test]
    fn loading_normalizes_the_disabled_language() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a","writing":{"spell_check":true,"language":"disabled"}}"#,
        )
        .unwrap();

        let settings = Settings::load(&path).unwrap();

        assert!(!settings.writing.spell_check);
        assert!(!settings.writing.spell_checking_enabled());
    }

    #[test]
    fn unknown_spell_check_language_falls_back_to_automatic() {
        let json =
            br#"{"version":1,"accounts":[],"writing":{"spell_check":false,"language":"klingon"}}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert!(!settings.writing.spell_check);
        assert_eq!(settings.writing.language, SpellCheckLanguage::Automatic);
    }

    #[test]
    fn writing_settings_round_trip() {
        let settings = Settings {
            writing: WritingSettings {
                spell_check: false,
                language: SpellCheckLanguage::English,
            },
            ..Settings::default()
        };

        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains(r#""language":"english""#));

        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.writing, settings.writing);
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
            boss_key: "Ctrl+Alt+B".to_string(),
            lock: LockSettings {
                enabled: true,
                hash: Some("$argon2id$v=19$m=19456,t=2,p=1$secret$hash".to_string()),
                lock_on_launch: true,
                lock_on_hide: true,
                lock_on_suspend: true,
                lock_on_screen_lock: true,
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
        assert_eq!(restored.boss_key, "Ctrl+Alt+B");
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
        assert!(!restored.lock.lock_on_suspend);
        assert!(!restored.lock.lock_on_screen_lock);
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
        assert!(
            preview
                .changes
                .contains(&"Theme: System -> Dark".to_string())
        );
        assert!(preview.changes.contains(&"Zoom: 100% -> 125%".to_string()));
        assert!(
            preview
                .changes
                .iter()
                .any(|change| change.starts_with("Accounts:"))
        );
    }

    #[test]
    fn import_preview_reports_an_identical_file() {
        let mut current = Settings::default();
        current.ensure_first_account();
        let json = SettingsExport::new(&current, UNIX_EPOCH).to_json().unwrap();

        let preview = ImportPreview::from_bytes(json.as_bytes(), &current).unwrap();
        assert_eq!(preview.changes, vec!["No preference changes."]);
    }

    fn manual_proxy(
        protocol: ProxyProtocol,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> ManualProxy {
        ManualProxy {
            protocol,
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    #[test]
    fn proxy_defaults_to_system_mode() {
        let settings = Settings::default();
        assert_eq!(settings.network.proxy.mode, ProxyMode::System);
        assert_eq!(settings.network.proxy.manual.protocol, ProxyProtocol::Http);
        assert_eq!(
            settings.network.proxy.manual.port,
            ManualProxy::DEFAULT_PORT
        );
        assert!(settings.network.proxy.validate().is_ok());
        assert_eq!(settings.network.proxy.uri().unwrap(), None);
    }

    #[test]
    fn legacy_settings_use_the_system_proxy() {
        let json = br#"{"version":1,"accounts":[{"id":"a","name":"A"}],"active_account":"a"}"#;
        let settings: Settings = serde_json::from_slice(json).unwrap();

        assert_eq!(settings.network.proxy.mode, ProxyMode::System);
        assert!(settings.network.proxy.uri().unwrap().is_none());
    }

    #[test]
    fn proxy_modes_serialize_with_stable_names() {
        for (mode, encoded) in [
            (ProxyMode::System, "\"system\""),
            (ProxyMode::NoProxy, "\"none\""),
            (ProxyMode::Manual, "\"manual\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), encoded);
            let decoded: ProxyMode = serde_json::from_str(encoded).unwrap();
            assert_eq!(decoded, mode);
        }
    }

    #[test]
    fn manual_proxy_uri_encodes_credentials() {
        let manual = manual_proxy(
            ProxyProtocol::Http,
            "proxy.example",
            3128,
            "alice",
            "p@ss word",
        );

        assert_eq!(
            manual.uri().unwrap(),
            "http://alice:p%40ss%20word@proxy.example:3128"
        );
    }

    #[test]
    fn manual_proxy_uri_uses_the_selected_protocol() {
        for (protocol, scheme) in [
            (ProxyProtocol::Http, "http"),
            (ProxyProtocol::Https, "https"),
            (ProxyProtocol::Socks, "socks"),
        ] {
            let manual = manual_proxy(protocol, "127.0.0.1", 8080, "", "");
            assert_eq!(manual.uri().unwrap(), format!("{scheme}://127.0.0.1:8080"));
        }
    }

    #[test]
    fn manual_proxy_validation_rejects_incomplete_or_unsafe_values() {
        assert!(
            manual_proxy(ProxyProtocol::Http, "proxy.example", 8080, "", "")
                .validate()
                .is_ok()
        );
        for host in [
            "",
            "   ",
            "http://proxy",
            "proxy example",
            "user@proxy",
            "proxy/path",
        ] {
            let manual = manual_proxy(ProxyProtocol::Http, host, 8080, "", "");
            assert!(manual.validate().is_err(), "accepted host {host:?}");
        }

        let zero_port = manual_proxy(ProxyProtocol::Http, "proxy.example", 0, "", "");
        assert!(zero_port.validate().is_err());

        let password_without_username =
            manual_proxy(ProxyProtocol::Http, "proxy.example", 8080, "", "secret");
        assert!(password_without_username.validate().is_err());

        let ipv6 = manual_proxy(ProxyProtocol::Http, "[::1]", 8080, "", "");
        assert!(ipv6.validate().is_ok());
        assert_eq!(ipv6.uri().unwrap(), "http://[::1]:8080");

        let unbracketed_ipv6 = manual_proxy(ProxyProtocol::Http, "::1", 8080, "", "");
        assert!(unbracketed_ipv6.validate().is_err());
    }

    #[test]
    fn non_manual_modes_ignore_manual_validation() {
        let invalid = manual_proxy(ProxyProtocol::Http, "http://bad", 0, "", "secret");
        for mode in [ProxyMode::System, ProxyMode::NoProxy] {
            let proxy = ProxySettings {
                mode,
                manual: invalid.clone(),
            };
            assert!(proxy.validate().is_ok());
            assert_eq!(proxy.uri().unwrap(), None);
        }
    }

    #[test]
    fn manual_proxy_normalization_trims_the_host_and_drops_orphan_credentials() {
        let mut manual = manual_proxy(
            ProxyProtocol::Socks,
            "  proxy.example  ",
            1080,
            "  ",
            "secret",
        );
        manual.normalize();

        assert_eq!(manual.host, "proxy.example");
        assert!(manual.username.is_empty());
        assert!(manual.password.is_empty());
    }

    #[test]
    fn manual_proxy_debug_output_redacts_credentials() {
        let manual = manual_proxy(
            ProxyProtocol::Http,
            "proxy.example",
            3128,
            "alice",
            "p@ss word",
        );
        let debug = format!("{manual:?}");

        assert!(!debug.contains("alice"), "{debug}");
        assert!(!debug.contains("p@ss word"), "{debug}");
        assert!(debug.contains("proxy.example"), "{debug}");
    }

    #[test]
    fn settings_debug_output_redacts_proxy_credentials() {
        let mut settings = Settings::default();
        settings.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Http,
                "proxy.example",
                3128,
                "alice",
                "p@ss word",
            ),
        };

        let debug = format!("{settings:?}");
        assert!(!debug.contains("alice"), "{debug}");
        assert!(!debug.contains("p@ss word"), "{debug}");
    }

    #[test]
    fn proxy_summary_redacts_credentials() {
        let proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Http,
                "proxy.example",
                3128,
                "alice",
                "p@ss word",
            ),
        };
        let summary = proxy.redacted_summary();

        assert!(summary.contains("proxy.example:3128"), "{summary}");
        assert!(!summary.contains("alice"), "{summary}");
        assert!(!summary.contains("p@ss word"), "{summary}");
    }

    #[test]
    fn proxy_settings_round_trip_through_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut settings = Settings::default();
        settings.ensure_first_account();
        settings.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Socks,
                "proxy.example",
                1080,
                "alice",
                "s3cret",
            ),
        };

        settings.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded, settings);
        assert_eq!(loaded.network.proxy.manual.password, "s3cret");
    }

    #[test]
    fn exports_strip_proxy_credentials() {
        let mut settings = Settings::default();
        settings.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Http,
                "proxy.example",
                3128,
                "alice",
                "p@ss word",
            ),
        };

        let export = SettingsExport::new(&settings, UNIX_EPOCH);
        let proxy = &export.settings.network.proxy;

        assert_eq!(proxy.mode, ProxyMode::Manual);
        assert_eq!(proxy.manual.host, "proxy.example");
        assert!(proxy.manual.username.is_empty());
        assert!(proxy.manual.password.is_empty());
    }

    #[test]
    fn imports_strip_proxy_credentials() {
        let mut settings = Settings::default();
        settings.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Http,
                "proxy.example",
                3128,
                "alice",
                "p@ss word",
            ),
        };
        let export = SettingsExport {
            version: SETTINGS_EXPORT_VERSION,
            exported_at: "2026-01-01T00:00:00Z".to_string(),
            settings,
        };
        let bytes = serde_json::to_vec(&export).unwrap();

        let imported = SettingsExport::from_bytes(&bytes).unwrap();

        assert_eq!(imported.network.proxy.mode, ProxyMode::Manual);
        assert_eq!(imported.network.proxy.manual.host, "proxy.example");
        assert!(imported.network.proxy.manual.username.is_empty());
        assert!(imported.network.proxy.manual.password.is_empty());
    }

    #[test]
    fn system_proxy_uris_are_parsed() {
        let endpoint = parse_proxy_uri("socks://alice:p%40ss@127.0.0.1:1080").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::Socks);
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 1080);
        assert_eq!(endpoint.username, "alice");
        assert_eq!(endpoint.password, "p@ss");

        let endpoint = parse_proxy_uri("http://proxy.example").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::Http);
        assert_eq!(endpoint.host, "proxy.example");
        assert_eq!(endpoint.port, 80);

        let endpoint = parse_proxy_uri("https://proxy.example:8443").unwrap();
        assert_eq!(endpoint.protocol, ProxyProtocol::Https);
        assert_eq!(endpoint.port, 8443);

        let endpoint = parse_proxy_uri("socks://[::1]:1080").unwrap();
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.redacted_label(), "socks://[::1]:1080");

        assert!(parse_proxy_uri("ftp://proxy.example:21").is_err());
        assert!(parse_proxy_uri("not a url").is_err());
    }

    #[test]
    fn http_proxy_basic_auth_uses_standard_base64() {
        assert_eq!(base64_encode(b"alice:secret"), "YWxpY2U6c2VjcmV0");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn http_status_parser_handles_proxy_responses() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 200 Connection established\r\n\r\n"),
            Some(200)
        );
        assert_eq!(
            parse_http_status(b"HTTP/1.1 407 Proxy Authentication Required\r\n"),
            Some(407)
        );
        assert_eq!(parse_http_status(b"garbage"), None);
    }

    #[test]
    fn import_preview_reports_removed_proxy_credentials() {
        let mut current = Settings::default();
        current.ensure_first_account();
        current.network.proxy = ProxySettings {
            mode: ProxyMode::Manual,
            manual: manual_proxy(
                ProxyProtocol::Http,
                "proxy.example",
                3128,
                "alice",
                "s3cret",
            ),
        };
        let json = SettingsExport::new(&current, UNIX_EPOCH).to_json().unwrap();

        let preview = ImportPreview::from_bytes(json.as_bytes(), &current).unwrap();

        assert!(
            preview
                .changes
                .contains(&"Network proxy credentials: removed".to_string()),
            "{:?}",
            preview.changes
        );
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
        assert!(
            first
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("settings-2023-11-14T22-13-20Z")
        );
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
