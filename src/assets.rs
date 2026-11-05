use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use include_dir::{Dir, include_dir};

static ASSETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/assets");

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let path = path.trim_start_matches('/');
        Ok(ASSETS
            .get_file(path)
            .map(|file| Cow::Borrowed(file.contents())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Ok(ASSETS
                .dirs()
                .map(|dir| SharedString::from(dir.path().to_string_lossy().into_owned()))
                .collect());
        }

        Ok(ASSETS
            .get_dir(path)
            .map(|dir| {
                dir.files()
                    .filter_map(|file| file.path().file_name())
                    .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default())
    }
}

pub const ICON_GEAR: &str = "icons/gear.svg";
pub const ICON_MINUS: &str = "icons/minus.svg";
pub const ICON_SQUARE: &str = "icons/square.svg";
pub const ICON_X: &str = "icons/x.svg";
pub const ICON_LOCK: &str = "icons/lock.svg";
pub const ICON_PALETTE: &str = "icons/palette.svg";
pub const ICON_BELL: &str = "icons/bell.svg";
pub const ICON_SHIELD: &str = "icons/shield.svg";
pub const ICON_INFO: &str = "icons/info.svg";
pub const ICON_USER: &str = "icons/user.svg";
