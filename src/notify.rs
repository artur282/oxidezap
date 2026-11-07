use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crossbeam_channel::{Sender, unbounded};
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopNotification {
    summary: String,
    body: String,
    icon: Option<PathBuf>,
}

impl DesktopNotification {
    fn new(summary: &str, body: &str, icon: Option<&Path>) -> Self {
        Self {
            summary: summary.to_string(),
            body: body.to_string(),
            icon: icon.map(Path::to_path_buf),
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
    let notification = DesktopNotification::new(summary, body, icon);
    let Some(queue) = queue() else {
        show(notification);
        return;
    };
    if let Err(notification) = queue.send(notification) {
        show(notification);
    }
}

pub fn notify_download(filename: &str, destination: &Path, icon: Option<&Path>) {
    notify(
        "Download complete",
        &format!("{filename}\nSaved to {}", destination.display()),
        icon,
    );
}

fn show(notification: DesktopNotification) {
    let mut desktop = notify_rust::Notification::new();
    desktop
        .appname("OxideZap")
        .summary(&notification.summary)
        .body(&notification.body)
        .timeout(notify_rust::Timeout::Milliseconds(6000));
    if let Some(icon) = &notification.icon {
        desktop.icon(&icon.to_string_lossy());
    }
    if let Err(error) = desktop.show() {
        warn!(%error, "could not show a desktop notification");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn notification(summary: &str) -> DesktopNotification {
        DesktopNotification::new(summary, "body", None)
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
