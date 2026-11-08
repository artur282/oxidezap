use gpui::{
    div, prelude::*, px, App, Context, CursorStyle, Entity, FocusHandle, Focusable, IntoElement,
    MouseButton, Render, ResizeEdge, SharedString, Subscription, Window,
};

use crate::app::AppModel;
use crate::assets::{ICON_GEAR, ICON_LOCK, ICON_MINUS, ICON_SQUARE, ICON_X};
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
}

impl AccountWindow {
    pub fn new(model: Entity<AppModel>, account_id: String, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            account_id,
            focus_handle: cx.focus_handle(),
            last_title: String::new(),
            appearance_subscription: None,
        }
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
}

impl Focusable for AccountWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AccountWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The theme is resolved from the window appearance on every render, so a
        // repaint is enough when the system appearance changes. Register once per
        // view; a recreated window gets a new view and a new subscription.
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle);
        }

        let (theme, name, unread, crash_failed) = {
            let model = self.model.read(cx);
            (
                model.theme(window),
                model.account_name(&self.account_id),
                model.account_unread(&self.account_id),
                model.account_crash_failed(&self.account_id),
            )
        };

        let title = if unread > 0 {
            format!("OxideZap — {name} ({unread})")
        } else {
            format!("OxideZap — {name}")
        };
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }

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
            .child(app_icon(20.0))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(name)),
            )
            .when(unread > 0, |style| style.child(unread_badge(unread, theme)));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .track_focus(&self.focus_handle)
            .on_key_down(handle_focus_traversal)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(TITLEBAR_HEIGHT))
                    .w_full()
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(drag_area)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h_full()
                            .child(titlebar_button(
                                "account-settings",
                                ICON_GEAR,
                                "Settings",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.open_settings(cx);
                                }),
                            ))
                            .child(titlebar_button(
                                "account-lock",
                                ICON_LOCK,
                                "Lock",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.lock(cx);
                                }),
                            ))
                            .child(titlebar_button(
                                "account-minimize",
                                ICON_MINUS,
                                "Minimize",
                                theme,
                                false,
                                cx.listener(|_this, _event, window, _cx| {
                                    window.minimize_window();
                                }),
                            ))
                            .child(titlebar_button(
                                "account-maximize",
                                ICON_SQUARE,
                                "Maximize",
                                theme,
                                false,
                                cx.listener(|_this, _event, window, _cx| {
                                    window.zoom_window();
                                }),
                            ))
                            .child(titlebar_button(
                                "account-close",
                                ICON_X,
                                "Close",
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
                            .when(!crash_failed, |element| {
                                element.child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme.text_muted)
                                        .child("Loading WhatsApp Web…"),
                                )
                            })
                            .when(crash_failed, |element| {
                                element
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child("Could not recover WhatsApp Web"),
                                    )
                                    .child(
                                        div()
                                            .w(px(420.))
                                            .text_center()
                                            .text_size(px(13.))
                                            .text_color(theme.text_muted)
                                            .child(
                                                "The page kept crashing. Your session and messages are safe. Retry when you are ready.",
                                            ),
                                    )
                                    .child(button(
                                        "account-crash-retry",
                                        "Retry now",
                                        theme,
                                        true,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.retry(cx);
                                        }),
                                    ))
                            }),
                    ),
            )
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
