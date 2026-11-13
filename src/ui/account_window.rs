use std::time::Duration;

use gpui::{
    App, Context, CursorStyle, Entity, ExternalPaths, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, MouseButton, Render, ResizeEdge, SharedString, Subscription, Window,
    WindowCompositionSurface, div, prelude::*, px, rgb, rgba,
};
use raw_window_handle::RawWindowHandle;

use crate::app::{AccountViewState, AppModel, ConnectionState};
use crate::assets::{ICON_GEAR, ICON_LOCK, ICON_MINUS, ICON_PALETTE, ICON_SQUARE, ICON_X};
use crate::settings::{AccountIdentity, appearance_summary};
use crate::theme::Theme;
use crate::ui::command_palette::digit_account_index;
use crate::ui::components::{
    app_icon, button, handle_focus_traversal, titlebar_button, unread_badge,
};
use crate::webview::TITLEBAR_HEIGHT;

pub struct AccountWindow {
    model: Entity<AppModel>,
    account_id: String,
    focus_handle: FocusHandle,
    last_title: String,
    appearance_subscription: Option<Subscription>,
    drop_hover: bool,
    drop_poll: bool,
    composition_surface: Option<WindowCompositionSurface>,
    composition_xid: Option<u32>,
    composition_size: Option<(i32, i32)>,
}

impl AccountWindow {
    pub fn new(model: Entity<AppModel>, account_id: String, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            account_id,
            focus_handle: cx.focus_handle(),
            last_title: String::new(),
            appearance_subscription: None,
            drop_hover: false,
            drop_poll: false,
            composition_surface: None,
            composition_xid: None,
            composition_size: None,
        }
    }

    pub fn ensure_composition(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u32> {
        if let Some(xid) = self.composition_xid {
            return Some(xid);
        }
        let composition = window.enable_window_composition().ok()?;
        let surface = composition.create_native_surface().ok()?;
        let handle = surface.platform_surface().ok()?.platform_handle().ok()?;
        let handle = *handle.downcast::<RawWindowHandle>().ok()?;
        let RawWindowHandle::Xcb(handle) = handle else {
            return None;
        };
        let xid = handle.window.get();
        self.composition_surface = Some(surface);
        self.composition_xid = Some(xid);
        let account_id = self.account_id.clone();
        self.model
            .update(cx, |model, _| model.set_composition_xid(&account_id, xid));
        Some(xid)
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let account_id = self.account_id.clone();
        self.model.update(cx, |model, cx| {
            model.close_account_window(&account_id, window, cx);
        });
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.open_settings_window(cx));
    }

    fn lock(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.lock(cx));
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let account_id = self.account_id.clone();
        self.model
            .update(cx, |model, cx| model.retry_account(&account_id, cx));
    }

    fn open_palette(&mut self, window: &Window, cx: &mut Context<Self>) {
        let account_id = self.account_id.clone();
        let bounds = window.bounds();
        self.model.update(cx, |model, cx| {
            model.open_command_palette(Some(account_id), Some(bounds), cx);
        });
    }

    fn toggle_always_on_top(&mut self, cx: &mut Context<Self>) {
        let account_id = self.account_id.clone();
        let enabled = !self.model.read(cx).account_always_on_top(&account_id);
        self.model.update(cx, |model, cx| {
            model.set_always_on_top(&account_id, enabled, cx)
        });
    }

    fn toggle_mini_mode(&mut self, cx: &mut Context<Self>) {
        let account_id = self.account_id.clone();
        let enabled = !self.model.read(cx).account_mini_mode(&account_id);
        self.model.update(cx, |model, cx| {
            model.set_mini_mode(&account_id, enabled, cx)
        });
    }

    /// Called for drops over the native titlebar and the exposed window
    /// margins. Drops over the WebKit content are handled by the GTK layer.
    fn handle_drop(&mut self, paths: &ExternalPaths, _window: &mut Window, cx: &mut Context<Self>) {
        self.drop_hover = false;
        let account_id = self.account_id.clone();
        let paths = paths.paths().to_vec();
        self.model.update(cx, |model, cx| {
            model.attach_dropped_paths(&account_id, paths, cx);
        });
    }

    /// Ctrl+K opens the palette and Ctrl+1..9 switches accounts while the
    /// native window owns the keyboard. All other keys fall through to the
    /// existing tab traversal.
    fn handle_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if !keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
            return;
        }
        if keystroke.modifiers.shift {
            match keystroke.key.to_ascii_lowercase().as_str() {
                "t" => {
                    self.toggle_always_on_top(cx);
                    cx.stop_propagation();
                }
                "m" => {
                    self.toggle_mini_mode(cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }
        if keystroke.key == "k" {
            self.open_palette(window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(index) = digit_account_index(&keystroke.key) {
            self.model
                .update(cx, |model, cx| model.activate_account_at(index, cx));
            cx.stop_propagation();
        }
    }
}

impl Focusable for AccountWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AccountWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }

        // The theme is resolved from the window appearance on every render, so a
        // repaint is enough when the system appearance changes. Register once per
        // view; a recreated window gets a new view and a new subscription.
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }

        let (
            theme,
            name,
            identity,
            unread,
            crash_failed,
            connection,
            view_state,
            always_on_top,
            mini_mode,
            theme_mode,
            zoom,
        ) = {
            let model = self.model.read(cx);
            (
                model.account_theme(&self.account_id, window),
                model.account_name(&self.account_id),
                model.account_identity(&self.account_id),
                model.account_unread(&self.account_id),
                model.account_crash_failed(&self.account_id),
                model.account_connection(&self.account_id),
                model.account_view_state(&self.account_id),
                model.account_always_on_top(&self.account_id),
                model.account_mini_mode(&self.account_id),
                model.account_theme_mode(&self.account_id),
                model.account_zoom(&self.account_id),
            )
        };
        window.set_rem_size(theme.rem_size());
        if let Some(surface) = &self.composition_surface {
            let viewport = window.viewport_size();
            let bounds = gpui::Bounds::new(
                gpui::point(px(0.), px(TITLEBAR_HEIGHT)),
                gpui::size(viewport.width, viewport.height - px(TITLEBAR_HEIGHT)),
            )
            .to_device_pixels(window.scale_factor());
            if let Ok(platform) = surface.platform_surface() {
                let _ = platform.set_bounds(bounds);
            }
            let size = (bounds.size.width.0, bounds.size.height.0);
            if self.composition_size != Some(size) {
                self.composition_size = Some(size);
                let account_id = self.account_id.clone();
                self.model.update(cx, |model, cx| {
                    model.set_embedded_bounds(&account_id, size.0, size.1, cx);
                });
            }
        }
        let appearance = appearance_summary(theme_mode, zoom);

        let drop_hover = self.drop_hover || cx.has_active_drag();
        let show_unread = self.model.read(cx).settings.show_unread_in_title;

        let title = if unread > 0 && show_unread {
            format!("({unread}) OxideZap — {name}")
        } else {
            format!("OxideZap — {name}")
        };
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }

        let (connection_message, connection_hint) = match connection {
            ConnectionState::Connecting | ConnectionState::Connected => {
                (t!("Loading WhatsApp Web…"), None)
            }
            ConnectionState::Offline => (
                t!("WhatsApp Web is offline."),
                Some(t!("OxideZap will reconnect when the network returns.")),
            ),
            ConnectionState::Reconnecting => (
                t!("Reconnecting…"),
                Some(t!("OxideZap is trying to recover the page.")),
            ),
        };

        let drag_area = div()
            .id("account-drag-area")
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(12.))
            .h_full()
            .flex_1()
            .on_mouse_down(MouseButton::Left, |_event, window, _cx| {
                window.start_window_move();
            })
            .on_click(|event, window, _cx| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .child(account_avatar(&identity, 22.0, theme))
            .child(
                div()
                    .text_size(theme.font_px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(name)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(appearance.clone())),
            )
            .when(unread > 0, |style| style.child(unread_badge(unread, theme)));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_shortcut))
            .on_key_down(handle_focus_traversal)
            .on_drop::<ExternalPaths>(cx.listener(Self::handle_drop))
            .on_drag_move::<ExternalPaths>(cx.listener(
                |this, _event: &gpui::DragMoveEvent<ExternalPaths>, _window, cx| {
                    if !this.drop_hover {
                        this.drop_hover = true;
                        cx.notify();
                    }
                    if this.drop_poll {
                        return;
                    }
                    this.drop_poll = true;
                    let view = cx.weak_entity();
                    cx.spawn(async move |_, cx| {
                        loop {
                            smol::Timer::after(Duration::from_millis(100)).await;
                            let keep_polling = view
                                .update(cx, |this, cx| {
                                    if cx.has_active_drag() {
                                        return true;
                                    }
                                    this.drop_poll = false;
                                    if this.drop_hover {
                                        this.drop_hover = false;
                                        cx.notify();
                                    }
                                    false
                                })
                                .unwrap_or(false);
                            if !keep_polling {
                                break;
                            }
                        }
                    })
                    .detach();
                },
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(TITLEBAR_HEIGHT))
                    .w_full()
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(identity.accent.map(|accent| rgb(accent.rgb())).unwrap_or(theme.border))
                    .child(drag_area)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h_full()
                            .gap(px(4.))
                            .when(view_state.is_loaded(), |element| {
                                element
                                    .child(connection_badge(connection, theme))
                                    .when(connection.retry_available(), |element| {
                                        element.child(button(
                                            "account-connection-retry",t!("Retry now"),
                                            theme,
                                            false,
                                            cx.listener(|this, _event, _window, cx| {
                                                this.retry(cx);
                                            }),
                                        ))
                                    })
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h_full()
                            .child(titlebar_button(
                                "account-command-palette",
                                ICON_PALETTE,t!("Command palette (Ctrl+K)"),
                                theme,
                                false,
                                cx.listener(|this, _event, window, cx| {
                                    this.open_palette(window, cx);
                                }),
                            ))
                            .child(button(
                                "account-pin",
                                "Pin",
                                theme,
                                always_on_top,
                                cx.listener(|this, _event, _window, cx| {
                                    this.toggle_always_on_top(cx);
                                }),
                            ))
                            .child(button(
                                "account-mini",
                                "Mini",
                                theme,
                                mini_mode,
                                cx.listener(|this, _event, _window, cx| {
                                    this.toggle_mini_mode(cx);
                                }),
                            ))
                            .child(titlebar_button(
                                "account-settings",
                                ICON_GEAR,
                                tr!("Settings — {appearance}", appearance = appearance),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.open_settings(cx);
                                }),
                            ))
                            .child(titlebar_button(
                                "account-lock",
                                ICON_LOCK,t!("Lock"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.lock(cx);
                                }),
                            ))
                            .child(titlebar_button(
                                "account-minimize",
                                ICON_MINUS,t!("Minimize"),
                                theme,
                                false,
                                cx.listener(|_this, _event, window, _cx| {
                                    window.minimize_window();
                                }),
                            ))
                            .child(titlebar_button(
                                "account-maximize",
                                ICON_SQUARE,t!("Maximize"),
                                theme,
                                false,
                                cx.listener(|_this, _event, window, _cx| {
                                    window.zoom_window();
                                }),
                            ))
                            .child(titlebar_button(
                                "account-close",
                                ICON_X,t!("Close"),
                                theme,
                                true,
                                cx.listener(|this, _event, window, cx| {
                                    this.close(window, cx);
                                }),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .relative()
                    .bg(theme.bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(14.))
                            .child(app_icon(64.0))
                            .when(view_state == AccountViewState::Suspended, |element| {
                                element
                                    .child(
                                        div()
                                            .text_size(theme.font_px(16.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child(t!("Account suspended")),
                                    )
                                    .child(
                                        div()
                                            .w(px(440.))
                                            .text_center()
                                            .text_size(theme.font_px(13.))
                                            .text_color(theme.text_muted)
                                            .child(t!("WhatsApp Web is not running, so its memory was released. Your login, chats, and settings stay on disk.")),
                                    )
                                    .child(
                                        div()
                                            .w(px(440.))
                                            .text_center()
                                            .text_size(theme.font_px(12.))
                                            .text_color(theme.text_faint)
                                            .child(t!("Reactivating reloads WhatsApp Web and can take a few seconds. The current scroll position or an unsent draft may be lost.")),
                                    )
                                    .child(button(
                                        "account-reactivate",t!("Reactivate"),
                                        theme,
                                        true,
                                        cx.listener(|this, _event, _window, cx| {
                                            let account_id = this.account_id.clone();
                                            this.model.update(cx, |model, cx| {
                                                model.resume_account(&account_id, cx);
                                            });
                                        }),
                                    ))
                            })
                            .when(view_state == AccountViewState::Resuming, |element| {
                                element
                                    .child(
                                        div()
                                            .text_size(theme.font_px(16.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child(t!("Restoring WhatsApp Web…")),
                                    )
                                    .child(
                                        div()
                                            .w(px(440.))
                                            .text_center()
                                            .text_size(theme.font_px(13.))
                                            .text_color(theme.text_muted)
                                            .child(t!("Creating a fresh WebKit view. Your session is kept and the page will reconnect automatically.")),
                                    )
                            })
                            .when(view_state.is_loaded() && !crash_failed, |element| {
                                element
                                    .child(
                                        div()
                                            .text_size(theme.font_px(13.))
                                            .text_color(theme.text_muted)
                                            .child(connection_message),
                                    )
                                    .when_some(connection_hint, |element, hint| {
                                        element.child(
                                            div()
                                                .w(px(420.))
                                                .text_center()
                                                .text_size(theme.font_px(12.))
                                                .text_color(theme.text_faint)
                                                .child(hint),
                                        )
                                    })
                                    .when(connection.retry_available(), |element| {
                                            element.child(button(
                                                "account-connection-content-retry",t!("Retry now"),
                                                theme,
                                                true,
                                                cx.listener(|this, _event, _window, cx| {
                                                    this.retry(cx);
                                                }),
                                            ))
                                        },
                                    )
                            })
                            .when(view_state.is_loaded() && crash_failed, |element| {
                                element
                                    .child(
                                        div()
                                            .text_size(theme.font_px(16.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child(t!("Could not recover WhatsApp Web")),
                                    )
                                    .child(
                                        div()
                                            .w(px(420.))
                                            .text_center()
                                            .text_size(theme.font_px(13.))
                                            .text_color(theme.text_muted)
                                            .child(t!("The page kept crashing. Your session and messages are safe. Retry when you are ready.")),
                                    )
                                    .child(button(
                                        "account-crash-retry",t!("Retry now"),
                                        theme,
                                        true,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.retry(cx);
                                        }),
                                    ))
                            }),
                    ),
            )
            .when(drop_hover, |element| element.child(drop_overlay(theme)))
            .child(resize_handle(
                "resize-top",
                ResizeEdge::Top,
                CursorStyle::ResizeUpDown,
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(0.))
                    .right(px(0.))
                    .h(px(4.)),
            ))
            .child(resize_handle(
                "resize-bottom",
                ResizeEdge::Bottom,
                CursorStyle::ResizeUpDown,
                div()
                    .absolute()
                    .bottom(px(0.))
                    .left(px(0.))
                    .right(px(0.))
                    .h(px(4.)),
            ))
            .child(resize_handle(
                "resize-left",
                ResizeEdge::Left,
                CursorStyle::ResizeLeftRight,
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .bottom(px(0.))
                    .w(px(4.)),
            ))
            .child(resize_handle(
                "resize-right",
                ResizeEdge::Right,
                CursorStyle::ResizeLeftRight,
                div()
                    .absolute()
                    .right(px(0.))
                    .top(px(0.))
                    .bottom(px(0.))
                    .w(px(4.)),
            ))
            .child(resize_handle(
                "resize-tl",
                ResizeEdge::TopLeft,
                CursorStyle::ResizeUpLeftDownRight,
                div().absolute().top(px(0.)).left(px(0.)).size(px(8.)),
            ))
            .child(resize_handle(
                "resize-tr",
                ResizeEdge::TopRight,
                CursorStyle::ResizeUpRightDownLeft,
                div().absolute().top(px(0.)).right(px(0.)).size(px(8.)),
            ))
            .child(resize_handle(
                "resize-bl",
                ResizeEdge::BottomLeft,
                CursorStyle::ResizeUpRightDownLeft,
                div().absolute().bottom(px(0.)).left(px(0.)).size(px(8.)),
            ))
            .child(resize_handle(
                "resize-br",
                ResizeEdge::BottomRight,
                CursorStyle::ResizeUpLeftDownRight,
                div().absolute().bottom(px(0.)).right(px(0.)).size(px(8.)),
            ))
    }
}

fn account_avatar(identity: &AccountIdentity, size: f32, theme: Theme) -> impl IntoElement {
    let (background, text) = match identity.accent {
        Some(accent) => (rgb(accent.rgb()), rgb(accent.readable_text().rgb())),
        None => (theme.accent, theme.accent_text),
    };
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded_full()
        .bg(background)
        .text_color(text)
        .text_size(px((size * 0.45).max(10.0)))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(SharedString::from(identity.label.clone()))
}

fn drop_overlay(theme: Theme) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .right(px(0.))
        .bottom(px(0.))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x0b141ae6))
        .occlude()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .p(px(24.))
                .border_2()
                .border_color(theme.accent)
                .rounded_md()
                .child(
                    div()
                        .text_size(theme.font_px(22.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child("Drop to attach"),
                )
                .child(
                    div()
                        .text_size(theme.font_px(12.))
                        .text_color(theme.text_muted)
                        .child("Images, videos, PDFs, and files up to 64 MB stay in the preview until you send."),
                ),
        )
}

fn connection_badge(state: ConnectionState, theme: Theme) -> impl IntoElement {
    let color = match state {
        ConnectionState::Connecting => theme.text_muted,
        ConnectionState::Connected => rgb(0x25d366),
        ConnectionState::Offline => theme.danger,
        ConnectionState::Reconnecting => rgb(0xf0b429),
    };
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .h_full()
        .px(px(8.))
        .child(div().size(px(7.)).rounded_full().bg(color))
        .child(
            div()
                .text_size(theme.font_px(11.))
                .text_color(theme.text_muted)
                .child(state.label()),
        )
}

fn resize_handle(
    id: &'static str,
    edge: ResizeEdge,
    cursor: CursorStyle,
    style: gpui::Div,
) -> impl IntoElement {
    style
        .id(id)
        .cursor(cursor)
        .on_mouse_down(MouseButton::Left, move |_event, window, _cx| {
            window.start_window_resize(edge);
        })
}
