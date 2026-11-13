use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use crossbeam_channel::{Sender, unbounded};
use notify_rust::Hint;
use tracing::warn;

use crate::settings::{NotificationPrivacy, Settings};

/// A themeable named sound from the freedesktop.org sound naming specification.
/// The notification server decides whether it exists and whether to play it.
pub const SYSTEM_SOUND: &str = "message-new-instant";

/// Maximum visible characters in a notification summary. Long sender names and
/// page titles are cut at a character boundary so the daemon never receives an
/// unbounded string.
pub const MAX_SUMMARY_CHARS: usize = 120;
/// Maximum visible characters in a notification body.
pub const MAX_BODY_CHARS: usize = 500;

/// Final text for a native notification. [`NotificationContent::new`] only
/// truncates; privacy masking happens in [`NotificationPrivacyPolicy`] so every
/// emitter cannot choose its own text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationContent {
    pub summary: String,
    pub body: String,
}

impl NotificationContent {
    pub fn new(summary: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            summary: truncate(&summary.into(), MAX_SUMMARY_CHARS),
            body: truncate(&body.into(), MAX_BODY_CHARS),
        }
    }
}

/// Pure privacy policy shared by the app model and the settings preview.
///
/// Message notifications can reveal the sender, the body, or nothing. System
/// notifications (downloads and errors) keep their category summary but hide
/// their details when previews are restricted. `locked` is an input so the
/// policy is deterministic and testable without a running application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationPrivacyPolicy {
    privacy: NotificationPrivacy,
    hide_when_locked: bool,
}

impl NotificationPrivacyPolicy {
    pub fn new(privacy: NotificationPrivacy, hide_when_locked: bool) -> Self {
        Self {
            privacy,
            hide_when_locked,
        }
    }

    fn effective(self, locked: bool) -> NotificationPrivacy {
        if locked && self.hide_when_locked {
            NotificationPrivacy::Hidden
        } else {
            self.privacy
        }
    }

    pub fn message(self, locked: bool, title: &str, body: &str) -> NotificationContent {
        match self.effective(locked) {
            NotificationPrivacy::Full => NotificationContent::new(title, body),
            NotificationPrivacy::SenderOnly => match sender_from_title(title) {
                Some(sender) => NotificationContent::new(
                    tr!("Message from {sender}", sender = sender),
                    t!("Open OxideZap to read it."),
                ),
                None => hidden_message(),
            },
            NotificationPrivacy::Hidden => hidden_message(),
        }
    }

    pub fn system(self, locked: bool, summary: &str, body: &str) -> NotificationContent {
        match self.effective(locked) {
            NotificationPrivacy::Full => NotificationContent::new(summary, body),
            NotificationPrivacy::SenderOnly | NotificationPrivacy::Hidden => {
                NotificationContent::new(summary, t!("Open OxideZap for details."))
            }
        }
    }
}

fn hidden_message() -> NotificationContent {
    NotificationContent::new(t!("New WhatsApp message"), t!("Open OxideZap to read it."))
}

/// WhatsApp already separates the sender into the notification title. Strip an
/// optional unread counter such as `(3) ` and keep the rest as-is: separator
/// guessing is fragile, so a title that is only the generic app name falls back
/// to the Hidden level instead of inventing a sender.
fn sender_from_title(title: &str) -> Option<&str> {
    let sender = strip_unread_count(title).trim();
    if sender.is_empty()
        || matches!(
            sender.to_ascii_lowercase().as_str(),
            "whatsapp" | "whatsapp web" | "whatsapp business"
        )
    {
        return None;
    }
    Some(sender)
}

fn strip_unread_count(title: &str) -> &str {
    let title = title.trim_start();
    let Some(rest) = title.strip_prefix('(') else {
        return title;
    };
    let Some(end) = rest.find(')') else {
        return title;
    };
    let count = &rest[..end];
    if count.is_empty() || !count.chars().all(|character| character.is_ascii_digit()) {
        return title;
    }
    rest[end + 1..].trim_start()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

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
    #[cfg(test)]
    fn new(summary: &str, body: &str, icon: Option<&Path>, sound: Option<bool>) -> Self {
        Self {
            summary: summary.to_string(),
            body: body.to_string(),
            icon: icon.map(Path::to_path_buf),
            sound,
        }
    }

    fn from_content(
        content: NotificationContent,
        icon: Option<&Path>,
        sound: Option<bool>,
    ) -> Self {
        Self {
            summary: content.summary,
            body: content.body,
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
    notify_content(NotificationContent::new(summary, body), icon);
}

pub fn notify_content(content: NotificationContent, icon: Option<&Path>) {
    enqueue(DesktopNotification::from_content(content, icon, None));
}

pub fn notify_content_with_sound(content: NotificationContent, icon: Option<&Path>, sound: bool) {
    enqueue(DesktopNotification::from_content(
        content,
        icon,
        Some(sound),
    ));
}

/// Prefixes a message notification title with the account's avatar label.
pub fn account_summary(avatar: &str, summary: &str) -> String {
    let avatar = avatar.trim();
    if avatar.is_empty() {
        summary.to_string()
    } else {
        format!("{avatar} {summary}")
    }
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::settings::{DoNotDisturbMode, DoNotDisturbSettings, NotificationPrivacy, Settings};

    fn notification(summary: &str) -> DesktopNotification {
        DesktopNotification::new(summary, "body", None, None)
    }

    fn policy(privacy: NotificationPrivacy) -> NotificationPrivacyPolicy {
        NotificationPrivacyPolicy::new(privacy, true)
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
    fn message_privacy_levels_choose_sender_body_or_generic_text() {
        let full = policy(NotificationPrivacy::Full).message(false, "Alice", "See you at 18:00.");
        assert_eq!(full.summary, "Alice");
        assert_eq!(full.body, "See you at 18:00.");

        let sender = policy(NotificationPrivacy::SenderOnly).message(false, "Alice", "a secret");
        assert_eq!(
            sender.summary,
            tr!("Message from {sender}", sender = "Alice")
        );
        assert_eq!(sender.body, t!("Open OxideZap to read it."));
        assert!(!sender.body.contains("a secret"));

        let hidden = policy(NotificationPrivacy::Hidden).message(false, "Alice", "a secret");
        assert_eq!(hidden, hidden_message());
        assert!(!hidden.summary.contains("Alice"));
        assert!(!hidden.body.contains("a secret"));
    }

    #[test]
    fn sender_only_strips_unread_counters_and_falls_back_without_a_sender() {
        let sender = policy(NotificationPrivacy::SenderOnly).message(false, "(3) Alice", "body");
        assert_eq!(
            sender.summary,
            tr!("Message from {sender}", sender = "Alice")
        );

        for title in ["", "   ", "WhatsApp", "(4) WhatsApp", "whatsapp web"] {
            let content = policy(NotificationPrivacy::SenderOnly).message(false, title, "body");
            assert_eq!(content, hidden_message(), "title {title:?}");
        }
    }

    #[test]
    fn full_preview_keeps_an_empty_body() {
        let content = policy(NotificationPrivacy::Full).message(false, "Alice", "");
        assert_eq!(content.summary, "Alice");
        assert!(content.body.is_empty());
    }

    #[test]
    fn locked_previews_downgrade_to_hidden_when_enabled() {
        let locked = NotificationPrivacyPolicy::new(NotificationPrivacy::Full, true)
            .message(true, "Alice", "a secret");
        assert_eq!(locked, hidden_message());

        let unlocked = NotificationPrivacyPolicy::new(NotificationPrivacy::Full, false)
            .message(true, "Alice", "a secret");
        assert_eq!(unlocked.summary, "Alice");
        assert_eq!(unlocked.body, "a secret");
    }

    #[test]
    fn system_notifications_keep_their_summary_and_hide_details() {
        let summary = "Download complete";
        let details = "file.zip\nSaved to /home/alice/Downloads";
        let full = policy(NotificationPrivacy::Full).system(false, summary, details);
        assert_eq!(full, NotificationContent::new(summary, details));

        let masked = policy(NotificationPrivacy::SenderOnly).system(false, summary, details);
        assert_eq!(masked.summary, summary);
        assert_eq!(masked.body, t!("Open OxideZap for details."));
        assert!(!masked.body.contains("file.zip"));

        let locked = policy(NotificationPrivacy::Full).system(true, summary, details);
        assert_eq!(locked, masked);
    }

    #[test]
    fn summaries_and_bodies_are_truncated_at_character_boundaries() {
        let summary = "é".repeat(MAX_SUMMARY_CHARS + 50);
        let body = "ñ".repeat(MAX_BODY_CHARS + 50);
        let content = policy(NotificationPrivacy::Full).message(false, &summary, &body);

        assert_eq!(content.summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(content.summary.ends_with('…'));
        assert!(content.summary.starts_with('é'));
        assert_eq!(content.body.chars().count(), MAX_BODY_CHARS);
        assert!(content.body.ends_with('…'));

        let short = NotificationContent::new("short", "body");
        assert_eq!(short.summary, "short");
        assert_eq!(short.body, "body");
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
