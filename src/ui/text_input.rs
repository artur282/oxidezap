use std::rc::Rc;

use gpui::{
    App, ClipboardItem, Context, CursorStyle, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    Render, SharedString, Window, div, prelude::*, px, rgb,
};

type EnterCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

pub struct TextField {
    content: String,
    placeholder: SharedString,
    masked: bool,
    max_length: usize,
    focus_handle: FocusHandle,
    on_enter: Option<EnterCallback>,
}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            content: String::new(),
            placeholder: placeholder.into(),
            masked: false,
            max_length: 512,
            focus_handle: cx.focus_handle(),
            on_enter: None,
        }
    }

    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = content.into();
        self
    }

    pub fn on_enter(mut self, callback: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_enter = Some(Rc::new(callback));
        self
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, content: impl Into<String>, cx: &mut Context<Self>) {
        self.content = content.into();
        self.content.truncate(self.max_length);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_content("", cx);
    }

    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut length = self.content.chars().count();
        for ch in text.chars() {
            if length >= self.max_length {
                break;
            }
            if ch.is_control() {
                continue;
            }
            self.content.push(ch);
            length += 1;
        }
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control || keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.insert(&text.replace('\n', " "), cx);
                    }
                }
                "a" => {}
                "c" => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.content.clone()));
                }
                "x" => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.content.clone()));
                    self.content.clear();
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        match keystroke.key.as_str() {
            "backspace" => {
                self.content.pop();
                cx.notify();
            }
            "enter" => {
                if let Some(callback) = self.on_enter.clone() {
                    window.defer(cx, move |window, cx| callback(window, cx));
                }
            }
            "escape" => {}
            _ => {
                if let Some(text) = &keystroke.key_char {
                    self.insert(text, cx);
                }
            }
        }
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let display = if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.clone()
        };
        let empty = display.is_empty();
        let text = if empty {
            self.placeholder.clone()
        } else {
            SharedString::from(display)
        };
        let color = if empty { rgb(0x8696a0) } else { rgb(0x111b21) };

        div()
            .flex()
            .items_center()
            .h(px(38.))
            .w_full()
            .px(px(12.))
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(if focused {
                rgb(0x00a884)
            } else {
                rgb(0xd1d7db)
            })
            .rounded_md()
            .text_color(color)
            .text_size(px(14.))
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, _cx| {
                    window.focus(&this.focus_handle);
                }),
            )
            .on_key_down(cx.listener(Self::handle_key))
            .child(text)
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
