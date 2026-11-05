use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

use crate::paths::Paths;

pub fn apply(enabled: bool, paths: &Paths) -> Result<()> {
    apply_to(&paths.autostart_file(), enabled)
}

pub fn apply_to(path: &Path, enabled: bool) -> Result<()> {
    if enabled {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let executable = std::env::current_exe().context("could not locate the executable")?;
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=OxideZap\n\
             Comment=WhatsApp Web desktop client\n\
             Exec={} --autostart\n\
             Icon=oxidezap\n\
             Terminal=false\n\
             Categories=Network;InstantMessaging;\n\
             StartupNotify=false\n\
             X-GNOME-Autostart-enabled=true\n",
            quote_exec(&executable)
        );
        fs::write(path, entry).with_context(|| format!("could not write {}", path.display()))?;
        info!(path = %path.display(), "autostart enabled");
    } else if path.exists() {
        fs::remove_file(path).with_context(|| format!("could not remove {}", path.display()))?;
        info!(path = %path.display(), "autostart disabled");
    }
    Ok(())
}

#[cfg(test)]
pub fn is_enabled(path: &Path) -> bool {
    path.exists()
}

fn quote_exec(path: &Path) -> String {
    let value = path.display().to_string();
    if value.contains(' ') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_removes_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autostart/oxidezap.desktop");
        apply_to(&path, true).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("Name=OxideZap"));
        assert!(contents.contains("--autostart"));
        assert!(is_enabled(&path));
        apply_to(&path, false).unwrap();
        assert!(!is_enabled(&path));
    }
}
