use std::fmt;

use serde::{Deserialize, Serialize};

use crate::settings::DEFAULT_SHORTCUT;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamedKey {
    Space,
    Enter,
    Escape,
    Tab,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Up,
    Right,
    Down,
}

impl NamedKey {
    pub fn label(self) -> &'static str {
        match self {
            NamedKey::Space => "Space",
            NamedKey::Enter => "Enter",
            NamedKey::Escape => "Escape",
            NamedKey::Tab => "Tab",
            NamedKey::Backspace => "Backspace",
            NamedKey::Delete => "Delete",
            NamedKey::Insert => "Insert",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PageUp",
            NamedKey::PageDown => "PageDown",
            NamedKey::Left => "Left",
            NamedKey::Up => "Up",
            NamedKey::Right => "Right",
            NamedKey::Down => "Down",
        }
    }

    pub fn keysym(self) -> u32 {
        match self {
            NamedKey::Space => 0x20,
            NamedKey::Enter => 0xff0d,
            NamedKey::Escape => 0xff1b,
            NamedKey::Tab => 0xff09,
            NamedKey::Backspace => 0xff08,
            NamedKey::Delete => 0xffff,
            NamedKey::Insert => 0xff63,
            NamedKey::Home => 0xff50,
            NamedKey::End => 0xff57,
            NamedKey::PageUp => 0xff55,
            NamedKey::PageDown => 0xff56,
            NamedKey::Left => 0xff51,
            NamedKey::Up => 0xff52,
            NamedKey::Right => 0xff53,
            NamedKey::Down => 0xff54,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShortcutKey {
    Char(char),
    Function(u8),
    Named(NamedKey),
}

impl ShortcutKey {
    pub fn keysym(self) -> Option<u32> {
        match self {
            ShortcutKey::Char(ch) => char_keysym(ch),
            ShortcutKey::Function(number) if (1..=24).contains(&number) => {
                Some(0xffbe + u32::from(number) - 1)
            }
            ShortcutKey::Function(_) => None,
            ShortcutKey::Named(key) => Some(key.keysym()),
        }
    }

    pub fn label(self) -> String {
        match self {
            ShortcutKey::Char(ch) => ch.to_uppercase().to_string(),
            ShortcutKey::Function(number) => format!("F{number}"),
            ShortcutKey::Named(key) => key.label().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: ShortcutKey,
}

impl Default for Shortcut {
    fn default() -> Self {
        Self::parse(DEFAULT_SHORTCUT).expect("the default shortcut is valid")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutError(pub String);

impl fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ShortcutError {}

impl Shortcut {
    pub fn parse(input: &str) -> Result<Self, ShortcutError> {
        let parts: Vec<&str> = input
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(ShortcutError("Enter a shortcut.".to_string()));
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut super_key = false;
        let mut key = None;
        let mut key_was_uppercase = false;

        for (index, part) in parts.iter().enumerate() {
            let is_last = index + 1 == parts.len();
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                "super" | "meta" | "win" | "cmd" | "command" => super_key = true,
                _ if is_last => {
                    key_was_uppercase = part.chars().any(|ch| ch.is_ascii_uppercase());
                    key = Some(parse_key(part)?);
                }
                _ => {
                    return Err(ShortcutError(format!(
                        "Unknown modifier \"{part}\". Use Ctrl, Alt, Shift, or Super."
                    )));
                }
            }
        }

        let key = key.ok_or_else(|| ShortcutError("Add a key to the shortcut.".to_string()))?;
        if !ctrl && !alt && !shift && !super_key {
            return Err(ShortcutError(
                "A global shortcut needs at least one modifier.".to_string(),
            ));
        }

        if matches!(key, ShortcutKey::Char(_)) && key_was_uppercase {
            shift = true;
        }

        if key.keysym().is_none() {
            return Err(ShortcutError(
                "That key is not supported for global shortcuts.".to_string(),
            ));
        }

        Ok(Self {
            ctrl,
            alt,
            shift,
            super_key,
            key,
        })
    }

    pub fn keysym(self) -> Option<u32> {
        self.key.keysym()
    }

    pub fn modifiers_label(self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.super_key {
            parts.push("Super");
        }
        parts.join("+")
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let modifiers = self.modifiers_label();
        if modifiers.is_empty() {
            write!(f, "{}", self.key.label())
        } else {
            write!(f, "{}+{}", modifiers, self.key.label())
        }
    }
}

fn parse_key(input: &str) -> Result<ShortcutKey, ShortcutError> {
    let lower = input.to_ascii_lowercase();
    let named = match lower.as_str() {
        "space" => Some(NamedKey::Space),
        "enter" | "return" => Some(NamedKey::Enter),
        "escape" | "esc" => Some(NamedKey::Escape),
        "tab" => Some(NamedKey::Tab),
        "backspace" => Some(NamedKey::Backspace),
        "delete" | "del" => Some(NamedKey::Delete),
        "insert" | "ins" => Some(NamedKey::Insert),
        "home" => Some(NamedKey::Home),
        "end" => Some(NamedKey::End),
        "pageup" | "page-up" => Some(NamedKey::PageUp),
        "pagedown" | "page-down" => Some(NamedKey::PageDown),
        "left" => Some(NamedKey::Left),
        "up" => Some(NamedKey::Up),
        "right" => Some(NamedKey::Right),
        "down" => Some(NamedKey::Down),
        _ => None,
    };
    if let Some(key) = named {
        return Ok(ShortcutKey::Named(key));
    }

    if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=24).contains(&number) {
            return Ok(ShortcutKey::Function(number));
        }
    }

    let mut chars = input.chars();
    let Some(ch) = chars.next() else {
        return Err(ShortcutError("Add a key to the shortcut.".to_string()));
    };
    if chars.next().is_some() {
        return Err(ShortcutError(format!("\"{input}\" is not a single key.")));
    }
    if ch.is_ascii_alphabetic() || ch.is_ascii_digit() || is_supported_punctuation(ch) {
        Ok(ShortcutKey::Char(ch.to_ascii_lowercase()))
    } else {
        Err(ShortcutError(format!(
            "\"{ch}\" is not supported for global shortcuts."
        )))
    }
}

fn char_keysym(ch: char) -> Option<u32> {
    if ch.is_ascii_alphanumeric() {
        return Some(ch.to_ascii_lowercase() as u32);
    }
    let keysym = match ch {
        '-' => 0x2d,
        '=' => 0x3d,
        '[' => 0x5b,
        ']' => 0x5d,
        ';' => 0x3b,
        '\'' => 0x27,
        ',' => 0x2c,
        '.' => 0x2e,
        '/' => 0x2f,
        '\\' => 0x5c,
        '`' => 0x60,
        _ => return None,
    };
    Some(keysym)
}

fn is_supported_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '-' | '=' | '[' | ']' | ';' | '\'' | ',' | '.' | '/' | '\\' | '`'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_default_shortcut() {
        let shortcut = Shortcut::parse("Ctrl+Shift+W").unwrap();
        assert!(shortcut.ctrl);
        assert!(shortcut.shift);
        assert!(!shortcut.alt);
        assert_eq!(shortcut.key, ShortcutKey::Char('w'));
        assert_eq!(shortcut.to_string(), "Ctrl+Shift+W");
    }

    #[test]
    fn normalizes_uppercase_keys_to_shift() {
        let shortcut = Shortcut::parse("Ctrl+W").unwrap();
        assert!(shortcut.shift);
        assert_eq!(shortcut.key, ShortcutKey::Char('w'));
        assert_eq!(shortcut.to_string(), "Ctrl+Shift+W");
    }

    #[test]
    fn supports_function_and_named_keys() {
        assert_eq!(
            Shortcut::parse("Super+F5").unwrap().key,
            ShortcutKey::Function(5)
        );
        assert_eq!(
            Shortcut::parse("Alt+Space").unwrap().key,
            ShortcutKey::Named(NamedKey::Space)
        );
    }

    #[test]
    fn rejects_shortcuts_without_modifiers() {
        assert!(Shortcut::parse("W").is_err());
        assert!(Shortcut::parse("Ctrl+Shift").is_err());
        assert!(Shortcut::parse("Ctrl+Unknown").is_err());
    }

    #[test]
    fn keysyms_are_stable() {
        assert_eq!(
            Shortcut::parse("Ctrl+Shift+W").unwrap().keysym(),
            Some(0x77)
        );
        assert_eq!(Shortcut::parse("Ctrl+F1").unwrap().keysym(), Some(0xffbe));
        assert_eq!(
            Shortcut::parse("Ctrl+PageDown").unwrap().keysym(),
            Some(0xff56)
        );
    }
}
