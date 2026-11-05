use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString,
    StatefulInteractiveElement, Window, div, prelude::*, px,
};

use crate::app::{AppModel, AppModelEntityExt};
use crate::assets::{ICON_BELL, ICON_GEAR, ICON_INFO, ICON_PALETTE, ICON_SHIELD, ICON_USER};
use crate::lock::MIN_PASSWORD_LENGTH;
use crate::settings::{MAX_ZOOM, MIN_ZOOM, Settings, ThemeMode};
use crate::shortcut::Shortcut;
use crate::theme::Theme;
use crate::ui::components::{
    app_icon, button, card, divider, labeled_field, section_header, sidebar_item, toggle_row,
};
use crate::ui::text_input::TextField;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    Accounts,
    Appearance,
    Notifications,
    General,
    Security,
    About,
}

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            SettingsSection::Accounts => "Accounts",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::General => "General",
            SettingsSection::Security => "Security",
            SettingsSection::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsSection::Accounts => ICON_USER,
            SettingsSection::Appearance => ICON_PALETTE,
            SettingsSection::Notifications => ICON_BELL,
            SettingsSection::General => ICON_GEAR,
            SettingsSection::Security => ICON_SHIELD,
            SettingsSection::About => ICON_INFO,
        }
    }
}

pub struct SettingsWindow {
    model: Entity<AppModel>,
    section: SettingsSection,
    focus_handle: FocusHandle,
    renaming: Option<(String, Entity<TextField>)>,
    remove_confirm: Option<String>,
    password_field: Entity<TextField>,
    confirm_field: Entity<TextField>,
    shortcut_field: Entity<TextField>,
    security_error: Option<String>,
    shortcut_error: Option<String>,
    remove_password_confirm: bool,
}

impl SettingsWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        tracing::info!("SettingsWindow::new");
        let password_field = cx.new(|cx| TextField::new("New password", cx).masked(true));
        let confirm_field = cx.new(|cx| TextField::new("Confirm password", cx).masked(true));
        let shortcut = model.read(cx).settings.shortcut.clone();
        let shortcut_field = cx.new(|cx| TextField::new("Ctrl+Shift+W", cx).with_content(shortcut));
        Self {
            model,
            section: SettingsSection::Accounts,
            focus_handle: cx.focus_handle(),
            renaming: None,
            remove_confirm: None,
            password_field,
            confirm_field,
            shortcut_field,
            security_error: None,
            shortcut_error: None,
            remove_password_confirm: false,
        }
    }

    fn set_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    fn render_accounts(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = div().flex().flex_col().gap(px(10.));
        for account in &settings.accounts {
            let id = account.id.clone();
            let name = account.name.clone();
            let initial = account.initial();
            let unread = account.unread;

            if let Some((renaming_id, field)) = &self.renaming {
                if renaming_id == &id {
                    let field = field.clone();
                    let save_id = id.clone();
                    let cancel_id = id.clone();
                    list = list.child(
                        card(theme).child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .child(avatar(&initial, theme))
                                .child(field)
                                .child(button(
                                    SharedString::from(format!("rename-save-{save_id}")),
                                    "Save",
                                    theme,
                                    true,
                                    cx.listener(move |this, _event, _window, cx| {
                                        let Some((account_id, field)) = this.renaming.take() else {
                                            return;
                                        };
                                        let name = field.read(cx).content().to_string();
                                        this.model.update(cx, |model, cx| {
                                            model.rename_account(&account_id, &name, cx);
                                        });
                                        cx.notify();
                                    }),
                                ))
                                .child(button(
                                    SharedString::from(format!("rename-cancel-{cancel_id}")),
                                    "Cancel",
                                    theme,
                                    false,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.renaming = None;
                                        cx.notify();
                                    }),
                                )),
                        ),
                    );
                    continue;
                }
            }

            if self.remove_confirm.as_deref() == Some(id.as_str()) {
                let remove_id = id.clone();
                let cancel_id = id.clone();
                list = list.child(
                    card(theme)
                        .border_color(theme.danger)
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.text)
                                .child("Remove this account?"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text_muted)
                                .child(
                                    "The local WhatsApp session, cookies, and cached files for this account will be deleted. This cannot be undone.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button(
                                    SharedString::from(format!("remove-yes-{remove_id}")),
                                    "Remove account",
                                    theme,
                                    true,
                                    cx.listener(move |this, _event, _window, cx| {
                                        let account_id = remove_id.clone();
                                        this.remove_confirm = None;
                                        this.model.update(cx, |model, cx| {
                                            model.remove_account(&account_id, cx);
                                        });
                                        cx.notify();
                                    }),
                                ))
                                .child(button(
                                    SharedString::from(format!("remove-no-{cancel_id}")),
                                    "Cancel",
                                    theme,
                                    false,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.remove_confirm = None;
                                        cx.notify();
                                    }),
                                )),
                        ),
                );
                continue;
            }

            let open_id = id.clone();
            let rename_id = id.clone();
            let rename_name = name.clone();
            let remove_id = id.clone();
            list = list.child(
                card(theme).child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(14.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(12.))
                                .child(avatar(&initial, theme))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .text_size(px(14.))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(theme.text)
                                                .child(SharedString::from(name)),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(theme.text_muted)
                                                .child(if unread > 0 {
                                                    format!("{unread} unread")
                                                } else {
                                                    "No unread messages".to_string()
                                                }),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(button(
                                    SharedString::from(format!("open-{open_id}")),
                                    "Open",
                                    theme,
                                    true,
                                    cx.listener(move |this, _event, _window, cx| {
                                        let account_id = open_id.clone();
                                        this.model.update(cx, |model, cx| {
                                            model.open_account_window(&account_id, cx);
                                        });
                                    }),
                                ))
                                .child(button(
                                    SharedString::from(format!("rename-{rename_id}")),
                                    "Rename",
                                    theme,
                                    false,
                                    cx.listener(move |this, _event, _window, cx| {
                                        let field = cx.new(|cx| {
                                            TextField::new("Account name", cx)
                                                .with_content(rename_name.clone())
                                        });
                                        this.renaming = Some((rename_id.clone(), field));
                                        cx.notify();
                                    }),
                                ))
                                .when(settings.accounts.len() > 1, |style| {
                                    style.child(button(
                                        SharedString::from(format!("remove-{remove_id}")),
                                        "Remove",
                                        theme,
                                        false,
                                        cx.listener(move |this, _event, _window, cx| {
                                            this.remove_confirm = Some(remove_id.clone());
                                            cx.notify();
                                        }),
                                    ))
                                }),
                        ),
                ),
            );
        }

        let add_button = button(
            "add-account",
            "Add account",
            theme,
            false,
            cx.listener(|this, _event, _window, cx| {
                this.model.update(cx, |model, cx| model.add_account(cx));
            }),
        );

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "Accounts",
                "Every account has its own isolated WhatsApp Web session and window.",
                theme,
            ))
            .child(list)
            .child(div().flex().gap(px(8.)).child(add_button))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child("Adding an account opens a new window with a fresh QR code."),
            )
            .into_any_element()
    }

    fn render_appearance(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut theme_cards = div().flex().gap(px(10.));
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            let selected = settings.theme == mode;
            let id = SharedString::from(format!("theme-{}", mode.label().to_lowercase()));
            theme_cards = theme_cards.child(
                div()
                    .id(id)
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(14.))
                    .w(px(140.))
                    .rounded_lg()
                    .bg(if selected {
                        theme.surface_hover
                    } else {
                        theme.surface
                    })
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.model.update(cx, |model, cx| model.set_theme(mode, cx));
                    }))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(mode.label()),
                    )
                    .child(div().text_size(px(11.)).text_color(theme.text_muted).child(
                        match mode {
                            ThemeMode::System => "Follow the desktop",
                            ThemeMode::Light => "Bright surfaces",
                            ThemeMode::Dark => "Low-light surfaces",
                        },
                    )),
            );
        }

        let zooms = [
            (0.8, "80%"),
            (0.9, "90%"),
            (1.0, "100%"),
            (1.1, "110%"),
            (1.25, "125%"),
            (1.5, "150%"),
        ];
        let mut zoom_row = div().flex().flex_wrap().gap(px(8.));
        for (zoom, label) in zooms {
            let selected = (settings.zoom - zoom).abs() < 0.001;
            zoom_row = zoom_row.child(button(
                SharedString::from(format!("zoom-{label}")),
                label,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| model.set_zoom(zoom, cx));
                }),
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "Appearance",
                "Choose how the native shell looks and how large WhatsApp Web is rendered.",
                theme,
            ))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Theme"),
                    )
                    .child(theme_cards),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Display size"),
                    )
                    .child(zoom_row)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "WhatsApp Web zoom is limited to {}%–{}%. The native UI keeps its own size.",
                                (MIN_ZOOM * 100.0) as u32,
                                (MAX_ZOOM * 100.0) as u32
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_notifications(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "Notifications",
                "WhatsApp Web notifications are delivered by the desktop notification service.",
                theme,
            ))
            .child(
                card(theme).child(toggle_row(
                    "notifications-enabled",
                    "Desktop notifications",
                    "Show a native notification when a message arrives while OxideZap is in the background.",
                    settings.notifications,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.notifications = !settings.notifications;
                        });
                    }),
                )),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child("Notifications are suppressed while the app is locked."),
            )
            .into_any_element()
    }

    fn render_general(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shortcut_field = self.shortcut_field.clone();
        let error = self.shortcut_error.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "General",
                "Window behavior, startup, and the global show/hide shortcut.",
                theme,
            ))
            .child(
                card(theme)
                    .child(toggle_row(
                        "close-to-tray",
                        "Close to tray",
                        "Closing an account window hides it instead of quitting OxideZap.",
                        settings.close_to_tray,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.close_to_tray = !settings.close_to_tray;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "start-at-login",
                        "Launch at login",
                        "Start OxideZap automatically when you sign in to the desktop.",
                        settings.start_at_login,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.start_at_login = !settings.start_at_login;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "remember-bounds",
                        "Remember window size and position",
                        "Restore each account window where you left it.",
                        settings.remember_bounds,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.remember_bounds = !settings.remember_bounds;
                            });
                        }),
                    )),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Global shortcut"),
                    )
                    .child(labeled_field(
                        "Show or hide OxideZap",
                        Some(SharedString::from(
                            "Use modifiers plus one key, for example Ctrl+Shift+W.",
                        )),
                        theme,
                        shortcut_field,
                    ))
                    .when_some(error, |style, error| {
                        style.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(div().flex().gap(px(8.)).child(button(
                        "apply-shortcut",
                        "Apply shortcut",
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let value = this.shortcut_field.read(cx).content().to_string();
                            match Shortcut::parse(&value) {
                                Ok(shortcut) => {
                                    this.shortcut_error = None;
                                    this.model.update(cx, |model, cx| {
                                        model.set_shortcut(shortcut, cx);
                                    });
                                }
                                Err(error) => {
                                    this.shortcut_error = Some(error.to_string());
                                    cx.notify();
                                }
                            }
                        }),
                    )))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child("Global shortcuts require an X11 or XWayland session."),
                    ),
            )
            .into_any_element()
    }

    fn render_security(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_password = settings.lock.hash.is_some();
        let password_field = self.password_field.clone();
        let confirm_field = self.confirm_field.clone();
        let error = self.security_error.clone();

        let mut security_card = card(theme).child(toggle_row(
            "lock-enabled",
            "Require a password to unlock",
            "Hide every account window behind a lock screen.",
            settings.lock.enabled,
            theme,
            cx.listener(|this, _event, _window, cx| {
                this.model.update_settings(cx, |settings| {
                    settings.lock.enabled = !settings.lock.enabled;
                });
            }),
        ));
        if settings.lock.enabled {
            security_card = security_card
                .child(divider(theme))
                .child(toggle_row(
                    "lock-on-launch",
                    "Lock on launch",
                    "Ask for the password when OxideZap starts.",
                    settings.lock.lock_on_launch,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.lock.lock_on_launch = !settings.lock.lock_on_launch;
                        });
                    }),
                ))
                .child(divider(theme))
                .child(toggle_row(
                    "lock-on-hide",
                    "Lock when hidden",
                    "Lock automatically when the window is hidden to the tray.",
                    settings.lock.lock_on_hide,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.lock.lock_on_hide = !settings.lock.lock_on_hide;
                        });
                    }),
                ))
                .child(divider(theme))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child("Lock after idle time"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(idle_button(0, settings, theme, cx))
                                .child(idle_button(5, settings, theme, cx))
                                .child(idle_button(15, settings, theme, cx))
                                .child(idle_button(30, settings, theme, cx)),
                        ),
                );
        }

        let mut password_card = card(theme).child(
            div()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(if has_password {
                    "Change password"
                } else {
                    "Set a password"
                }),
        );
        password_card = password_card
            .child(labeled_field(
                "Password",
                Some(SharedString::from(format!(
                    "At least {MIN_PASSWORD_LENGTH} characters."
                ))),
                theme,
                password_field,
            ))
            .child(labeled_field(
                "Confirm password",
                None,
                theme,
                confirm_field,
            ))
            .when_some(error, |style, error| {
                style.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button(
                        "save-password",
                        if has_password {
                            "Update password"
                        } else {
                            "Set password"
                        },
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let password = this.password_field.read(cx).content().to_string();
                            let confirm = this.confirm_field.read(cx).content().to_string();
                            if password != confirm {
                                this.security_error =
                                    Some("The passwords do not match.".to_string());
                                cx.notify();
                                return;
                            }
                            match this
                                .model
                                .update(cx, |model, cx| model.set_password(&password, cx))
                            {
                                Ok(()) => {
                                    this.security_error = None;
                                    this.password_field.update(cx, |field, cx| field.clear(cx));
                                    this.confirm_field.update(cx, |field, cx| field.clear(cx));
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.security_error = Some(error);
                                    cx.notify();
                                }
                            }
                        }),
                    ))
                    .when(has_password, |style| {
                        style.child(button(
                            "remove-password",
                            "Remove password",
                            theme,
                            false,
                            cx.listener(|this, _event, _window, cx| {
                                this.remove_password_confirm = true;
                                cx.notify();
                            }),
                        ))
                    }),
            );

        if self.remove_password_confirm {
            password_card = password_card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded_md()
                    .bg(theme.surface_alt)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child("Remove password protection?"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                "remove-password-yes",
                                "Remove",
                                theme,
                                true,
                                cx.listener(|this, _event, _window, cx| {
                                    this.remove_password_confirm = false;
                                    this.model.update(cx, |model, cx| model.remove_password(cx));
                                    cx.notify();
                                }),
                            ))
                            .child(button(
                                "remove-password-no",
                                "Cancel",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.remove_password_confirm = false;
                                    cx.notify();
                                }),
                            )),
                    ),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "Security",
                "The app lock uses Argon2id. The password never leaves this device.",
                theme,
            ))
            .child(security_card)
            .child(password_card)
            .when(settings.lock.enabled && has_password, |style| {
                style.child(div().flex().gap(px(8.)).child(button(
                    "lock-now",
                    "Lock now",
                    theme,
                    false,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update(cx, |model, cx| model.lock(cx));
                    }),
                )))
            })
            .into_any_element()
    }

    fn render_about(&mut self, theme: Theme) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "About OxideZap",
                "A native Linux shell around the official WhatsApp Web experience.",
                theme,
            ))
            .child(
                card(theme)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(app_icon(52.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.))
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child("OxideZap"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme.text_muted)
                                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(theme.text_faint)
                                            .child("Rust · GPUI · WebKitGTK"),
                                    ),
                            ),
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text_muted)
                            .child(
                                "OxideZap is an unofficial, independent project. It is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. WhatsApp is a trademark of Meta Platforms, Inc.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                "open-repo",
                                "GitHub repository",
                                theme,
                                false,
                                |_event, _window, _cx| {
                                    crate::links::open_external(
                                        "https://github.com/artur282/oxidezap",
                                    );
                                },
                            ))
                            .child(button(
                                "open-issues",
                                "Report an issue",
                                theme,
                                false,
                                |_event, _window, _cx| {
                                    crate::links::open_external(
                                        "https://github.com/artur282/oxidezap/issues",
                                    );
                                },
                            )),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Built with"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child("GPUI for the native shell · WebKitGTK for WhatsApp Web · Rust for everything in between."),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child("MIT licensed. The source code is available on GitHub."),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, settings) = {
            let model = self.model.read(cx);
            (model.theme(window), model.settings.clone())
        };

        let content = match self.section {
            SettingsSection::Accounts => self.render_accounts(&settings, theme, cx),
            SettingsSection::Appearance => self.render_appearance(&settings, theme, cx),
            SettingsSection::Notifications => self.render_notifications(&settings, theme, cx),
            SettingsSection::General => self.render_general(&settings, theme, cx),
            SettingsSection::Security => self.render_security(&settings, theme, cx),
            SettingsSection::About => self.render_about(theme),
        };

        let mut sidebar = div()
            .flex()
            .flex_col()
            .w(px(212.))
            .h_full()
            .p(px(14.))
            .gap(px(4.))
            .bg(theme.surface)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .px(px(8.))
                    .py(px(10.))
                    .child(app_icon(26.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("OxideZap"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.text_faint)
                                    .child("Settings"),
                            ),
                    ),
            )
            .child(div().h(px(10.)));

        for section in [
            SettingsSection::Accounts,
            SettingsSection::Appearance,
            SettingsSection::Notifications,
            SettingsSection::General,
            SettingsSection::Security,
            SettingsSection::About,
        ] {
            let active = self.section == section;
            let id = SharedString::from(format!("settings-nav-{}", section.label()));
            sidebar = sidebar.child(sidebar_item(
                id,
                section.icon(),
                section.label(),
                active,
                theme,
                cx.listener(move |this, _event, _window, cx| {
                    this.set_section(section, cx);
                }),
            ));
        }

        sidebar = sidebar.child(div().flex_1()).child(
            div()
                .px(px(8.))
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
        );

        div().flex().size_full().bg(theme.bg).child(sidebar).child(
            div()
                .id("settings-content")
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .overflow_y_scroll()
                .p(px(28.))
                .gap(px(18.))
                .child(div().flex().justify_end().child(button(
                    "close-settings",
                    "Close",
                    theme,
                    false,
                    cx.listener(|_this, _event, window, _cx| {
                        window.remove_window();
                    }),
                )))
                .child(content),
        )
    }
}

fn avatar(initial: &str, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(38.))
        .rounded_full()
        .bg(theme.accent)
        .text_color(theme.accent_text)
        .text_size(px(15.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(initial.to_uppercase())
}

fn idle_button(
    minutes: u32,
    settings: &Settings,
    theme: Theme,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let selected = settings.lock.idle_minutes == minutes;
    let label = if minutes == 0 {
        "Off".to_string()
    } else {
        format!("{minutes} min")
    };
    button(
        SharedString::from(format!("idle-{minutes}")),
        label,
        theme,
        selected,
        cx.listener(move |this, _event, _window, cx| {
            this.model.update_settings(cx, |settings| {
                settings.lock.idle_minutes = minutes;
            });
        }),
    )
}
