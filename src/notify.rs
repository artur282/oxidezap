use std::path::Path;

use tracing::warn;

pub fn notify(summary: &str, body: &str, icon: Option<&Path>) {
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("OxideZap")
        .summary(summary)
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(6000));
    if let Some(icon) = icon {
        notification.icon(&icon.to_string_lossy());
    }
    if let Err(error) = notification.show() {
        warn!(%error, "could not show a desktop notification");
    }
}

pub fn notify_download(filename: &str, destination: &Path, icon: Option<&Path>) {
    notify(
        "Download complete",
        &format!("{filename}\nSaved to {}", destination.display()),
        icon,
    );
}
