use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use tracing::{debug, warn};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceMessage {
    Show,
    Toggle,
    Lock,
    Quit,
    OpenUrl(String),
}

impl InstanceMessage {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        match line {
            "" | "show" | "--show" => Some(Self::Show),
            "toggle" | "--toggle" => Some(Self::Toggle),
            "lock" | "--lock" => Some(Self::Lock),
            "quit" | "--quit" => Some(Self::Quit),
            other
                if other.starts_with("whatsapp://")
                    || other.starts_with("https://")
                    || other.starts_with("http://") =>
            {
                Some(Self::OpenUrl(other.to_string()))
            }
            other => other
                .strip_prefix("url ")
                .map(|url| Self::OpenUrl(url.to_string())),
        }
    }
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

/// Send the current invocation to an already-running instance. Returns `true`
/// when a live instance accepted the message.
pub fn send_to_running(path: &Path, args: &[String]) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let messages = if args.is_empty() {
        vec!["show".to_string()]
    } else {
        args.to_vec()
    };
    for message in messages {
        if writeln!(stream, "{message}").is_err() {
            return false;
        }
    }
    let _ = stream.flush();
    true
}

pub fn listen(path: &Path, tx: Sender<InstanceMessage>) -> Result<InstanceListener> {
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

fn handle_stream(stream: UnixStream, tx: Sender<InstanceMessage>) {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                if let Some(message) = InstanceMessage::parse(&line) {
                    debug!(?message, "single-instance message");
                    let _ = tx.send(message);
                }
            }
            Err(error) => {
                warn!(%error, "could not read a single-instance message");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_messages() {
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
        assert_eq!(InstanceMessage::parse("nonsense"), None);
    }

    #[test]
    fn sends_and_receives_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.sock");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let _listener = listen(&path, tx).unwrap();
        assert!(send_to_running(&path, &["toggle".to_string()]));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            InstanceMessage::Toggle
        );
    }
}
