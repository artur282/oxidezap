use std::collections::HashMap;
use std::thread::JoinHandle;

use crossbeam_channel::{Sender, bounded};
use tracing::{debug, warn};
use zbus::zvariant::Value;

pub const LAUNCHER_ENTRY_PATH: &str = "/com/canonical/Unity/LauncherEntry";
pub const LAUNCHER_ENTRY_INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
pub const APP_URI: &str = "application://oxidezap.desktop";

/// Publishes the unread count through the Unity LauncherEntry D-Bus API, which
/// is understood by Unity, KDE, and several other Linux desktops. Failures are
/// non-fatal: the tray tooltip is always available.
pub struct Badge {
    tx: Sender<u32>,
    _thread: JoinHandle<()>,
}

impl Badge {
    pub fn start() -> Self {
        let (tx, rx) = bounded::<u32>(8);
        let thread = std::thread::Builder::new()
            .name("oxidezap-badge".into())
            .spawn(move || {
                let connection = match zbus::blocking::Connection::session() {
                    Ok(connection) => connection,
                    Err(error) => {
                        warn!(%error, "launcher badge unavailable: no session bus");
                        return;
                    }
                };

                for count in rx {
                    let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
                    properties.insert("count", Value::from(count));
                    properties.insert("count-visible", Value::from(count > 0));
                    let body = (APP_URI, properties);
                    if let Err(error) = connection.emit_signal(
                        None::<&str>,
                        LAUNCHER_ENTRY_PATH,
                        LAUNCHER_ENTRY_INTERFACE,
                        "Update",
                        &body,
                    ) {
                        debug!(%error, "could not update the launcher badge");
                    }
                }
            })
            .expect("could not start the launcher badge thread");

        Self {
            tx,
            _thread: thread,
        }
    }

    pub fn set(&self, count: u32) {
        let _ = self.tx.try_send(count);
    }
}
