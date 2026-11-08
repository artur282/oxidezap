use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, bounded};
use tracing::{debug, warn};

/// Largest single-instance message accepted over the socket. Account names are
/// user-provided, so the listener refuses unbounded lines instead of growing
/// one allocation without limit.
const MAX_MESSAGE_LEN: usize = 8 * 1024;

/// How long the socket thread waits for the UI thread to answer a request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a CLI call waits for the running instance to answer.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Do Not Disturb options accepted by `--dnd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DndMode {
    On,
    Off,
    OneHour,
    Tomorrow,
}

impl DndMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            "1h" => Some(Self::OneHour),
            "tomorrow" => Some(Self::Tomorrow),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::OneHour => "1h",
            Self::Tomorrow => "tomorrow",
        }
    }

    /// Scheduling belongs to the Do Not Disturb issue. Until it lands, every
    /// non-`off` value drives the existing `Settings::do_not_disturb` flag.
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// `1h` and `tomorrow` need the scheduling model from the Do Not Disturb
    /// issue. The CLI warns that they currently stay on until disabled.
    pub fn is_scheduled(self) -> bool {
        matches!(self, Self::OneHour | Self::Tomorrow)
    }
}

/// A phone number or phone-based WhatsApp id (`<number>@c.us`) opened by
/// `--chat`. Group ids cannot be opened through a WhatsApp Web URL, so they
/// are rejected instead of silently doing nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatTarget {
    phone: String,
}

impl ChatTarget {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("--chat requires a phone number or a phone-based chat id".to_string());
        }

        let number = match raw.rsplit_once('@') {
            Some((number, suffix)) => {
                let suffix = suffix.to_ascii_lowercase();
                if suffix != "c.us" && suffix != "s.whatsapp.net" {
                    return Err(format!(
                        "--chat cannot open '{raw}': only phone numbers and <number>@c.us ids are supported"
                    ));
                }
                number
            }
            None => raw,
        };

        if number
            .chars()
            .any(|ch| !(ch.is_ascii_digit() || matches!(ch, '+' | '-' | ' ' | '(' | ')' | '.')))
        {
            return Err("--chat accepts only a phone number or a phone-based chat id".to_string());
        }
        let digits: String = number.chars().filter(char::is_ascii_digit).collect();
        if !(6..=15).contains(&digits.len()) {
            return Err("--chat needs between 6 and 15 digits".to_string());
        }
        Ok(Self { phone: digits })
    }

    pub fn phone(&self) -> &str {
        &self.phone
    }

    /// Opens the chat without a `text` parameter, so WhatsApp Web focuses the
    /// conversation and never sends a message.
    pub fn web_url(&self) -> String {
        format!("https://web.whatsapp.com/send?phone={}", self.phone)
    }
}

/// Typed actions a second invocation can forward to the running instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceMessage {
    Show,
    Toggle,
    Lock,
    Hide,
    Quit,
    OpenUrl(String),
    OpenAccount(String),
    OpenChat {
        account: Option<String>,
        chat: ChatTarget,
    },
    SetDoNotDisturb(DndMode),
    OpenDownloads,
    Status,
    ListAccounts,
}

impl InstanceMessage {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        match line {
            "" | "show" | "--show" => Some(Self::Show),
            "toggle" | "--toggle" => Some(Self::Toggle),
            "lock" | "--lock" => Some(Self::Lock),
            "hide" | "--hide" => Some(Self::Hide),
            "quit" | "--quit" => Some(Self::Quit),
            "downloads" | "--downloads" => Some(Self::OpenDownloads),
            "status" | "--status" => Some(Self::Status),
            "list-accounts" | "--list-accounts" => Some(Self::ListAccounts),
            "dnd" | "--dnd" => Some(Self::SetDoNotDisturb(DndMode::On)),
            other => Self::parse_prefixed(other),
        }
    }

    fn parse_prefixed(line: &str) -> Option<Self> {
        if line.starts_with("whatsapp://")
            || line.starts_with("https://")
            || line.starts_with("http://")
        {
            return Some(Self::OpenUrl(line.to_string()));
        }
        if let Some(url) = line.strip_prefix("url ") {
            return (!url.is_empty()).then(|| Self::OpenUrl(url.to_string()));
        }
        if let Some(query) = line.strip_prefix("account ") {
            return (!query.trim().is_empty()).then(|| Self::OpenAccount(query.trim().to_string()));
        }
        if let Some(rest) = line.strip_prefix("chat ") {
            return Self::parse_chat(rest);
        }
        if let Some(mode) = line
            .strip_prefix("dnd ")
            .and_then(|value| DndMode::parse(value))
        {
            return Some(Self::SetDoNotDisturb(mode));
        }
        None
    }

    fn parse_chat(rest: &str) -> Option<Self> {
        let (account, target) = rest.split_once(' ')?;
        let account = if account == "-" {
            None
        } else if account.is_empty() {
            return None;
        } else {
            Some(account.to_string())
        };
        let chat = ChatTarget::parse(target).ok()?;
        Some(Self::OpenChat { account, chat })
    }

    /// One line for the single-instance socket. `parse` accepts every value
    /// this method produces, which the round-trip tests enforce.
    pub fn encode(&self) -> String {
        match self {
            Self::Show => "show".to_string(),
            Self::Toggle => "toggle".to_string(),
            Self::Lock => "lock".to_string(),
            Self::Hide => "hide".to_string(),
            Self::Quit => "quit".to_string(),
            Self::OpenUrl(url) => format!("url {url}"),
            Self::OpenAccount(query) => format!("account {query}"),
            Self::OpenChat { account, chat } => format!(
                "chat {} {}",
                account.as_deref().unwrap_or("-"),
                chat.phone()
            ),
            Self::SetDoNotDisturb(mode) => format!("dnd {}", mode.as_str()),
            Self::OpenDownloads => "downloads".to_string(),
            Self::Status => "status".to_string(),
            Self::ListAccounts => "list-accounts".to_string(),
        }
    }

    pub fn is_query(&self) -> bool {
        matches!(self, Self::Status | Self::ListAccounts)
    }
}

/// Answer the UI thread sends back to a waiting CLI invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceReply {
    Ok,
    Output(String),
    Error(String),
}

impl InstanceReply {
    pub fn encode(&self) -> String {
        match self {
            Self::Ok => "ok\n".to_string(),
            Self::Output(text) => {
                let text = if text.ends_with('\n') {
                    text.clone()
                } else {
                    format!("{text}\n")
                };
                format!("ok\n{text}")
            }
            Self::Error(error) => format!("error: {}\n", single_line(error)),
        }
    }

    fn decode(first_line: &str, payload: String) -> Result<Self, ForwardError> {
        let first_line = first_line.trim_end_matches(['\r', '\n']);
        if first_line == "ok" {
            return Ok(if payload.is_empty() {
                Self::Ok
            } else {
                Self::Output(payload)
            });
        }
        if let Some(error) = first_line.strip_prefix("error: ") {
            return Err(ForwardError::Rejected(error.to_string()));
        }
        Err(ForwardError::Io(format!(
            "unexpected response from the running instance: {first_line}"
        )))
    }
}

/// A message forwarded over the socket plus the channel used to answer it.
#[derive(Debug)]
pub struct InstanceRequest {
    pub message: InstanceMessage,
    pub reply: Option<Sender<InstanceReply>>,
}

/// Why a CLI invocation could not talk to the running instance.
#[derive(Debug, PartialEq, Eq)]
pub enum ForwardError {
    /// Nothing is listening on the socket.
    NotRunning,
    /// The running instance understood the message and rejected it.
    Rejected(String),
    /// The socket existed but the exchange failed.
    Io(String),
}

impl fmt::Display for ForwardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => formatter.write_str("no running OxideZap instance"),
            Self::Rejected(error) => formatter.write_str(error),
            Self::Io(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ForwardError {}

pub struct InstanceLock {
    _file: fs::File,
}

/// Take an exclusive, non-blocking lock for this user's OxideZap instance.
///
/// The lock is released automatically when the file handle is dropped, so a
/// crashed process cannot leave a lock that blocks the next start. The socket
/// alone is not enough: two processes can both observe a missing socket and
/// race to bind it, which produced two application windows.
pub fn acquire_lock(path: &Path) -> Result<Option<InstanceLock>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not open the instance lock {}", path.display()))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(InstanceLock { _file: file }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(None);
    }
    Err(error).with_context(|| format!("could not lock {}", path.display()))
}

pub struct InstanceListener {
    path: PathBuf,
    _thread: JoinHandle<()>,
}

impl Drop for InstanceListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Forward one message to the running instance and wait for its answer.
///
/// The write side is shut down after sending so a listener built by an older
/// release (which waits for EOF before dispatching) cannot deadlock the
/// caller. A listener that closes without an answer is treated as accepting
/// the legacy fire-and-forget message.
pub fn send_request(path: &Path, message: &InstanceMessage) -> Result<InstanceReply, ForwardError> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) => {
            return Err(match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    ForwardError::NotRunning
                }
                _ => ForwardError::Io(format!("could not reach the running instance: {error}")),
            });
        }
    };
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|error| ForwardError::Io(error.to_string()))?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|error| ForwardError::Io(error.to_string()))?;
    writeln!(stream, "{}", message.encode())
        .map_err(|error| ForwardError::Io(format!("could not send the request: {error}")))?;
    stream
        .flush()
        .map_err(|error| ForwardError::Io(format!("could not send the request: {error}")))?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| ForwardError::Io(error.to_string()))?;

    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    let read = reader
        .read_line(&mut first_line)
        .map_err(|error| ForwardError::Io(format!("could not read the reply: {error}")))?;
    if read == 0 {
        return if is_legacy_message(message) {
            Ok(InstanceReply::Ok)
        } else {
            Err(ForwardError::Io(
                "the running instance did not answer; restart OxideZap and try again".to_string(),
            ))
        };
    }
    let mut payload = String::new();
    reader
        .read_to_string(&mut payload)
        .map_err(|error| ForwardError::Io(format!("could not read the reply: {error}")))?;
    InstanceReply::decode(&first_line, payload)
}

/// Retry while the primary instance is still binding its socket. Any answer
/// other than "not running" is final.
pub fn send_request_with_retry(
    path: &Path,
    message: &InstanceMessage,
    attempts: usize,
) -> Result<InstanceReply, ForwardError> {
    for attempt in 0..attempts.max(1) {
        match send_request(path, message) {
            Ok(reply) => return Ok(reply),
            Err(ForwardError::NotRunning) => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Err(ForwardError::NotRunning)
}

pub fn listen(path: &Path, tx: Sender<InstanceRequest>) -> Result<InstanceListener> {
    if path.exists() {
        // A stale socket from a crashed instance has no listener; remove it.
        if UnixStream::connect(path).is_err() {
            let _ = fs::remove_file(path);
        }
    }

    let listener = UnixListener::bind(path).with_context(|| {
        format!(
            "could not bind the single-instance socket {}",
            path.display()
        )
    })?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    let path = path.to_path_buf();
    let thread = std::thread::Builder::new()
        .name("oxidezap-instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let tx = tx.clone();
                        std::thread::spawn(move || handle_stream(stream, tx));
                    }
                    Err(error) => warn!(%error, "single-instance accept failed"),
                }
            }
        })
        .context("could not start the single-instance listener")?;

    Ok(InstanceListener {
        path,
        _thread: thread,
    })
}

fn handle_stream(stream: UnixStream, tx: Sender<InstanceRequest>) {
    let Ok(mut writer) = stream.try_clone() else {
        warn!("could not clone the single-instance stream");
        return;
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        match line {
            Ok(line) if line.len() <= MAX_MESSAGE_LEN => {
                let Some(message) = InstanceMessage::parse(&line) else {
                    warn!("ignoring an unknown single-instance message");
                    continue;
                };
                debug!(?message, "single-instance message");
                let (reply_tx, reply_rx) = bounded(1);
                let request = InstanceRequest {
                    message,
                    reply: Some(reply_tx),
                };
                if tx.send(request).is_err() {
                    return;
                }
                let reply = reply_rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| {
                    InstanceReply::Error("the running instance did not respond".to_string())
                });
                if writer.write_all(reply.encode().as_bytes()).is_err() {
                    return;
                }
                if writer.flush().is_err() {
                    return;
                }
            }
            Ok(_) => {
                warn!("ignoring an oversized single-instance message");
                return;
            }
            Err(error) => {
                warn!(%error, "could not read a single-instance message");
                break;
            }
        }
    }
}

/// Messages understood by releases before the expanded CLI. Those listeners
/// close without an answer, so an empty reply is treated as success for them.
fn is_legacy_message(message: &InstanceMessage) -> bool {
    matches!(
        message,
        InstanceMessage::Show
            | InstanceMessage::Toggle
            | InstanceMessage::Lock
            | InstanceMessage::Quit
            | InstanceMessage::OpenUrl(_)
    )
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|ch| if matches!(ch, '\n' | '\r') { ' ' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_typed_messages() {
        assert_eq!(InstanceMessage::parse(""), Some(InstanceMessage::Show));
        assert_eq!(
            InstanceMessage::parse("toggle"),
            Some(InstanceMessage::Toggle)
        );
        assert_eq!(
            InstanceMessage::parse("whatsapp://send?phone=1"),
            Some(InstanceMessage::OpenUrl(
                "whatsapp://send?phone=1".to_string()
            ))
        );
        assert_eq!(
            InstanceMessage::parse("https://web.whatsapp.com/"),
            Some(InstanceMessage::OpenUrl(
                "https://web.whatsapp.com/".to_string()
            ))
        );
        assert_eq!(
            InstanceMessage::parse("account acc-1"),
            Some(InstanceMessage::OpenAccount("acc-1".to_string()))
        );
        assert_eq!(
            InstanceMessage::parse("chat - +54 9 11 1234-5678"),
            Some(InstanceMessage::OpenChat {
                account: None,
                chat: ChatTarget::parse("+54 9 11 1234-5678").unwrap(),
            })
        );
        assert_eq!(
            InstanceMessage::parse("chat acc-1 5491112345678@c.us"),
            Some(InstanceMessage::OpenChat {
                account: Some("acc-1".to_string()),
                chat: ChatTarget::parse("5491112345678@c.us").unwrap(),
            })
        );
        assert_eq!(
            InstanceMessage::parse("dnd off"),
            Some(InstanceMessage::SetDoNotDisturb(DndMode::Off))
        );
        assert_eq!(
            InstanceMessage::parse("dnd"),
            Some(InstanceMessage::SetDoNotDisturb(DndMode::On))
        );
        assert_eq!(
            InstanceMessage::parse("downloads"),
            Some(InstanceMessage::OpenDownloads)
        );
        assert_eq!(InstanceMessage::parse("hide"), Some(InstanceMessage::Hide));
        assert_eq!(
            InstanceMessage::parse("status"),
            Some(InstanceMessage::Status)
        );
        assert_eq!(
            InstanceMessage::parse("list-accounts"),
            Some(InstanceMessage::ListAccounts)
        );
        assert_eq!(InstanceMessage::parse("nonsense"), None);
    }

    #[test]
    fn encoded_messages_round_trip() {
        let messages = [
            InstanceMessage::Show,
            InstanceMessage::Toggle,
            InstanceMessage::Lock,
            InstanceMessage::Hide,
            InstanceMessage::Quit,
            InstanceMessage::OpenUrl("https://wa.me/123456".to_string()),
            InstanceMessage::OpenAccount("acc-1".to_string()),
            InstanceMessage::OpenChat {
                account: None,
                chat: ChatTarget::parse("+54 9 11 1234-5678").unwrap(),
            },
            InstanceMessage::OpenChat {
                account: Some("acc-1".to_string()),
                chat: ChatTarget::parse("5491112345678@c.us").unwrap(),
            },
            InstanceMessage::SetDoNotDisturb(DndMode::OneHour),
            InstanceMessage::OpenDownloads,
            InstanceMessage::Status,
            InstanceMessage::ListAccounts,
        ];
        for message in messages {
            assert_eq!(
                InstanceMessage::parse(&message.encode()),
                Some(message.clone()),
                "round trip failed for {message:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_chat_and_dnd_values() {
        assert_eq!(InstanceMessage::parse("chat -"), None);
        assert_eq!(InstanceMessage::parse("chat - not a phone"), None);
        assert_eq!(InstanceMessage::parse("chat - 123"), None);
        assert_eq!(InstanceMessage::parse("chat - 1234567890123456"), None);
        assert_eq!(InstanceMessage::parse("chat - 12345@g.us"), None);
        assert_eq!(InstanceMessage::parse("chat - 123456@lid"), None);
        assert_eq!(InstanceMessage::parse("dnd maybe"), None);
        assert_eq!(InstanceMessage::parse("account "), None);
    }

    #[test]
    fn chat_targets_normalize_phone_numbers() {
        assert_eq!(
            ChatTarget::parse("+54 9 11 1234-5678").unwrap().phone(),
            "5491112345678"
        );
        assert_eq!(
            ChatTarget::parse("5491112345678@c.us").unwrap().phone(),
            "5491112345678"
        );
        assert_eq!(
            ChatTarget::parse("5491112345678@s.whatsapp.net")
                .unwrap()
                .phone(),
            "5491112345678"
        );
        assert_eq!(
            ChatTarget::parse("5491112345678").unwrap().web_url(),
            "https://web.whatsapp.com/send?phone=5491112345678"
        );
        assert!(ChatTarget::parse("120363000000000000@g.us").is_err());
        assert!(ChatTarget::parse("+54 (11) 1234.5678").is_ok());
        assert!(ChatTarget::parse("hello").is_err());
    }

    #[test]
    fn dnd_modes_map_to_the_boolean_flag() {
        assert!(DndMode::On.is_enabled());
        assert!(!DndMode::Off.is_enabled());
        assert!(DndMode::OneHour.is_enabled());
        assert!(DndMode::Tomorrow.is_enabled());
        assert!(DndMode::OneHour.is_scheduled());
        assert!(DndMode::Tomorrow.is_scheduled());
        assert_eq!(DndMode::parse("1H"), Some(DndMode::OneHour));
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.lock");
        let first = acquire_lock(&path).unwrap();
        assert!(first.is_some());
        assert!(acquire_lock(&path).unwrap().is_none());
        drop(first);
        assert!(acquire_lock(&path).unwrap().is_some());
    }

    #[test]
    fn sends_and_receives_requests() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.sock");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let listener = listen(&path, tx).unwrap();

        let reply_path = path.clone();
        let caller = std::thread::spawn(move || {
            send_request(&reply_path, &InstanceMessage::SetDoNotDisturb(DndMode::On))
        });

        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            request.message,
            InstanceMessage::SetDoNotDisturb(DndMode::On)
        );
        request.reply.unwrap().send(InstanceReply::Ok).unwrap();
        assert_eq!(caller.join().unwrap().unwrap(), InstanceReply::Ok);
        drop(listener);
    }

    #[test]
    fn forwards_status_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.sock");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let listener = listen(&path, tx).unwrap();

        let reply_path = path.clone();
        let caller =
            std::thread::spawn(move || send_request(&reply_path, &InstanceMessage::Status));

        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request.message, InstanceMessage::Status);
        request
            .reply
            .unwrap()
            .send(InstanceReply::Output(
                "version: test\nrunning: true\n".to_string(),
            ))
            .unwrap();
        assert_eq!(
            caller.join().unwrap().unwrap(),
            InstanceReply::Output("version: test\nrunning: true\n".to_string())
        );
        drop(listener);
    }

    #[test]
    fn forwards_rejections_as_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.sock");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let listener = listen(&path, tx).unwrap();

        let reply_path = path.clone();
        let caller = std::thread::spawn(move || {
            send_request(
                &reply_path,
                &InstanceMessage::OpenAccount("missing".to_string()),
            )
        });

        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        request
            .reply
            .unwrap()
            .send(InstanceReply::Error(
                "account 'missing' was not found".to_string(),
            ))
            .unwrap();
        assert_eq!(
            caller.join().unwrap().unwrap_err(),
            ForwardError::Rejected("account 'missing' was not found".to_string())
        );
        drop(listener);
    }

    #[test]
    fn missing_socket_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            send_request(&dir.path().join("missing.sock"), &InstanceMessage::Show),
            Err(ForwardError::NotRunning)
        );
    }
}
