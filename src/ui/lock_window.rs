use gpui::{App, Context, Entity, FocusHandle, Focusable, Render, Window, div, prelude::*, px};

use crate::app::AppModel;
use crate::assets::ICON_SHIELD;
use crate::ui::components::{app_icon, button, icon};
use crate::ui::text_input::TextField;

pub struct LockWindow {
    model: Entity<AppModel>,
    password_field: Entity<TextField>,
    error: Option<String>,
    focus_handle: FocusHandle,
}

impl LockWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let password_field = cx.new(|cx| {
            TextField::new("Password", cx)
                .masked(true)
                .on_enter(move |window, cx| {
                    let _ = weak.update(cx, |view, cx| view.submit(window, cx));
                })
        });
        Self {
            model,
            password_field,
            error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn password_focus(&self, cx: &App) -> FocusHandle {
        self.password_field.read(cx).focus_handle(cx)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = self.password_field.read(cx).content().to_string();
        if password.is_empty() {
            self.error = Some("Enter your password.".to_string());
            cx.notify();
            return;
        }
        let unlocked = self
            .model
            .update(cx, |model, cx| model.try_unlock(&password, cx));
        if unlocked {
            self.error = None;
        } else {
            self.error = Some("Incorrect password.".to_string());
            self.password_field.update(cx, |field, cx| field.clear(cx));
            let focus = self.password_field.read(cx).focus_handle(cx);
            window.focus(&focus);
            cx.notify();
        }
    }

    fn quit(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.quit(cx));
    }
}

impl Focusable for LockWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LockWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.model.read(cx).theme(window);
        let password_field = self.password_field.clone();
        let error = self.error.clone();

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.bg)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .w(px(390.))
                    .p(px(28.))
                    .rounded_lg()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(10.))
                            .child(app_icon(60.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.))
                                    .child(icon(ICON_SHIELD, 16.0, theme.accent))
                                    .child(
                                        div()
                                            .text_size(px(18.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child("OxideZap is locked"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.text_muted)
                                    .child("Enter your password to return to WhatsApp."),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(password_field)
                            .when_some(error, |style, error| {
                                style.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme.danger)
                                        .child(error),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(div().flex_1().child(button(
                                "unlock",
                                "Unlock",
                                theme,
                                true,
                                cx.listener(|this, _event, window, cx| {
                                    this.submit(window, cx);
                                }),
                            )))
                            .child(button(
                                "quit-locked",
                                "Quit",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.quit(cx);
                                }),
                            )),
                    )
                    .child(div().text_size(px(10.)).text_color(theme.text_faint).child(
                        "The password is stored as an Argon2id hash in your config directory.",
                    )),
            )
    }
}
