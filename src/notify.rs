use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crossbeam_channel::{Sender, unbounded};
use notify_rust::Hint;
use tracing::warn;

use crate::settings::Settings;

/// A themeable named sound from the freedesktop.org sound naming specification.
/// The notification server decides whether it exists and whether to play it.
pub const SYSTEM_SOUND: &str = "message-new-instant";

/// Central Do Not Disturb gate shared by every native notification call site.
/// Message and connection notifications additionally apply per-account mute and
/// sound preferences in [`Settings`].
pub fn do_not_disturb_suppressed(settings: &Settings, now: SystemTime) -> bool {
    settings.do_not_disturb.is_active(now)
}

/// Gate for notifications that already honor the global switch, such as
/// downloads, connection events, and message events.
pub fn notifications_allowed(settings: &Settings, now: SystemTime) -> bool {
    settings.native_notifications_enabled_at(now)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopNotification {
    summary: String,
    body: String,
    icon: Option<PathBuf>,
    sound: Option<bool>,
}

impl DesktopNotification {
    fn new(summary: &str, body: &str, icon: Option<&Path>, sound: Option<bool>) -> Self {
        Self {
            summary: summary.to_string(),
            body: body.to_string(),
            icon: icon.map(Path::to_path_buf),
            sound,
        }
    }
}

type Sink = Box<dyn Fn(DesktopNotification) + Send + 'static>;

/// Delivers notifications on a dedicated thread so the DBus round trip never
/// blocks the GPUI or GTK thread.
struct NotificationQueue {
    tx: Sender<DesktopNotification>,
}

impl NotificationQueue {
    fn start(sink: Sink) -> Option<Self> {
        let (tx, rx) = unbounded::<DesktopNotification>();
        let thread = std::thread::Builder::new()
            .name("oxidezap-notify".to_string())
            .spawn(move || {
                for notification in rx {
                    sink(notification);
                }
            });
        match thread {
            Ok(_) => Some(Self { tx }),
            Err(error) => {
                warn!(%error, "could not start the notification thread");
                None
            }
        }
    }

    fn send(&self, notification: DesktopNotification) -> Result<(), DesktopNotification> {
        self.tx.send(notification).map_err(|error| error.0)
    }
}

fn queue() -> Option<&'static NotificationQueue> {
    static QUEUE: OnceLock<Option<NotificationQueue>> = OnceLock::new();
    QUEUE
        .get_or_init(|| NotificationQueue::start(Box::new(show)))
        .as_ref()
}

pub fn notify(summary: &str, body: &str, icon: Option<&Path>) {
    enqueue(DesktopNotification::new(summary, body, icon, None));
}

pub fn notify_with_sound(summary: &str, body: &str, icon: Option<&Path>, sound: bool) {
    enqueue(DesktopNotification::new(summary, body, icon, Some(sound)));
}

fn enqueue(notification: DesktopNotification) {
    let Some(queue) = queue() else {
        show(notification);
        return;
    };
    if let Err(notification) = queue.send(notification) {
        show(notification);
    }
}

fn show(notification: DesktopNotification) {
    let mut desktop = build(
        &notification.summary,
        &notification.body,
        notification.icon.as_deref(),
    );
    if let Some(sound) = notification.sound {
        apply_sound_preference(&mut desktop, sound);
    }
    if let Err(error) = desktop.show() {
        warn!(%error, "could not show a desktop notification");
    }
}

fn build(summary: &str, body: &str, icon: Option<&Path>) -> notify_rust::Notification {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("OxideZap")
        .summary(summary)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(6000));
    if let Some(icon) = icon {
        notification.icon(&icon.to_string_lossy());
    }
    notification
}

fn apply_sound_preference(notification: &mut notify_rust::Notification, sound: bool) {
    if sound {
        notification.sound_name(SYSTEM_SOUND);
    } else {
        notification.hint(Hint::SuppressSound(true));
    }
}

pub fn notify_download(filename: &str, destination: &Path, icon: Option<&Path>) {
    notify(
        "Download complete",
        &format!("{filename}\nSaved to {}", destination.display()),
        icon,
    );
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::settings::{DoNotDisturbMode, DoNotDisturbSettings, Settings};

    fn notification(summary: &str) -> DesktopNotification {
        DesktopNotification::new(summary, "body", None, None)
    }

    #[test]
    fn do_not_disturb_and_the_global_switch_gate_native_notifications() {
        let now = SystemTime::now();
        let mut settings = Settings::default();
        assert!(notifications_allowed(&settings, now));
        assert!(!do_not_disturb_suppressed(&settings, now));

        settings.notifications = false;
        assert!(!notifications_allowed(&settings, now));
        assert!(!do_not_disturb_suppressed(&settings, now));

        settings.notifications = true;
        settings.do_not_disturb = DoNotDisturbSettings {
            mode: DoNotDisturbMode::On,
            ..DoNotDisturbSettings::default()
        };
        assert!(!notifications_allowed(&settings, now));
        assert!(do_not_disturb_suppressed(&settings, now));
    }

    #[test]
    fn plain_notifications_keep_their_default_sound_behavior() {
        let notification = build("Download complete", "file", None);
        assert!(!notification.hints.iter().any(|hint| matches!(
            hint,
            notify_rust::Hint::SoundName(_) | notify_rust::Hint::SuppressSound(_)
        )));
    }

    #[test]
    fn sound_preference_maps_to_notification_hints() {
        let mut audible = notify_rust::Notification::new();
        apply_sound_preference(&mut audible, true);
        assert!(
            audible
                .hints
                .contains(&notify_rust::Hint::SoundName(SYSTEM_SOUND.to_string()))
        );
        assert!(
            !audible
                .hints
                .contains(&notify_rust::Hint::SuppressSound(true))
        );

        let mut silent = notify_rust::Notification::new();
        apply_sound_preference(&mut silent, false);
        assert!(
            silent
                .hints
                .contains(&notify_rust::Hint::SuppressSound(true))
        );
        assert!(
            !silent
                .hints
                .iter()
                .any(|hint| matches!(hint, notify_rust::Hint::SoundName(_)))
        );
    }

    #[test]
    fn notification_queue_preserves_order() {
        let (tx, rx) = mpsc::channel();
        let queue = NotificationQueue::start(Box::new(move |notification| {
            tx.send(notification.summary).unwrap();
        }))
        .expect("the notification queue starts");

        for summary in ["one", "two", "three"] {
            queue.send(notification(summary)).unwrap();
        }

        for summary in ["one", "two", "three"] {
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), summary);
        }
    }

    #[test]
    fn notification_queue_does_not_wait_for_a_slow_sink() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let queue = NotificationQueue::start(Box::new(move |_| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        }))
        .expect("the notification queue starts");

        queue.send(notification("first")).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let sender = queue.tx.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(sender.send(notification("second")).is_ok());
        });
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());

        let _ = release_tx.send(());
    }
}
