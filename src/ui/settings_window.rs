use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Render, SharedString, StatefulInteractiveElement, Subscription, Timer, Window,
    div, prelude::*, px,
};

use crate::app::downloads::{DOWNLOAD_HISTORY_LIMIT, valid_base_dir};
use crate::app::{AppModel, AppModelEntityExt};
use crate::assets::{ICON_BELL, ICON_GEAR, ICON_INFO, ICON_PALETTE, ICON_SHIELD, ICON_USER};
use crate::diagnostics;
use crate::lock::MIN_PASSWORD_LENGTH;
use crate::notify;
use crate::settings::performance::PerformanceMode;
use crate::settings::storage::{self, ConfirmationLevel, StorageOperation};
use crate::settings::{
    Account, AccountColor, AccountTheme, DoNotDisturbMode, EVERY_DAY, FontScale, ImportPreview,
    MAX_CUSTOM_CSS_BYTES, MAX_ZOOM, MIN_ZOOM, ManualProxy, NotificationPrivacy, ProxyMode,
    ProxyProtocol, ProxySettings, SUSPEND_DELAY_OPTIONS, Settings, SettingsExport,
    SpellCheckLanguage, ThemeMode, WEEKDAYS, WEEKENDS, ZOOM_LEVELS, appearance_summary,
    export_file_name, format_time_of_day, json_path, normalize_avatar_label, parse_time_of_day,
    test_proxy_connection, validate_custom_css, write_settings_backup,
};
use crate::shortcut::Shortcut;
use crate::theme::Theme;
use crate::ui::components::{
    TAB_INDEX, app_icon, button, card, divider, handle_focus_traversal, labeled_field,
    register_field_tab_stop, section_header, sidebar_item, toggle_row,
};
use crate::ui::text_input::TextField;
use crate::updates::UpdateState;

use gtk::prelude::{FileChooserExt, NativeDialogExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileChooserMode {
    Open,
    Save,
    Folder,
    Css,
    Diagnostics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticsAction {
    Preview,
    Copy,
    Save,
}

/// A storage operation waiting for the user to confirm it.
#[derive(Clone)]
enum StorageConfirm {
    SignOut {
        account_id: String,
        account_name: String,
    },
    DeleteData {
        account_id: String,
        account_name: String,
        field: Entity<TextField>,
    },
}

impl StorageConfirm {
    fn account_id(&self) -> &str {
        match self {
            StorageConfirm::SignOut { account_id, .. } => account_id,
            StorageConfirm::DeleteData { account_id, .. } => account_id,
        }
    }
}

/// Inline editor for the optional avatar and custom accent of one account.
#[derive(Clone)]
struct IdentityEdit {
    account_id: String,
    hex: Entity<TextField>,
    avatar: Entity<TextField>,
    error: Option<String>,
}

const ICON_STORAGE: &str = "icons/download.svg";

#[derive(Clone, Debug)]
struct Notice {
    error: bool,
    text: String,
}

impl Notice {
    fn success(text: impl Into<String>) -> Self {
        Self {
            error: false,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            error: true,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    Accounts,
    Storage,
    Appearance,
    Accessibility,
    Notifications,
    Network,
    General,
    Performance,
    Security,
    About,
}

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            SettingsSection::Accounts => t!("Accounts"),
            SettingsSection::Storage => t!("Storage"),
            SettingsSection::Appearance => t!("Appearance"),
            SettingsSection::Accessibility => t!("Accessibility"),
            SettingsSection::Notifications => t!("Notifications"),
            SettingsSection::Network => t!("Network"),
            SettingsSection::General => t!("General"),
            SettingsSection::Performance => t!("Performance"),
            SettingsSection::Security => t!("Security"),
            SettingsSection::About => t!("About"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsSection::Accounts => ICON_USER,
            SettingsSection::Storage => ICON_STORAGE,
            SettingsSection::Appearance => ICON_PALETTE,
            SettingsSection::Accessibility => ICON_INFO,
            SettingsSection::Notifications => ICON_BELL,
            SettingsSection::Network => ICON_INFO,
            SettingsSection::General => ICON_GEAR,
            SettingsSection::Performance => ICON_INFO,
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
    identity_edit: Option<IdentityEdit>,
    remove_confirm: Option<String>,
    apply_appearance_confirm: Option<String>,
    account_appearance_feedback: Option<Notice>,
    password_field: Entity<TextField>,
    confirm_field: Entity<TextField>,
    shortcut_field: Entity<TextField>,
    boss_key_field: Entity<TextField>,
    palette_shortcut_field: Entity<TextField>,
    dnd_start_field: Entity<TextField>,
    dnd_end_field: Entity<TextField>,
    dnd_error: Option<String>,
    downloads_dir_field: Entity<TextField>,
    downloads_error: Option<String>,
    proxy_mode: ProxyMode,
    proxy_protocol: ProxyProtocol,
    proxy_host_field: Entity<TextField>,
    proxy_port_field: Entity<TextField>,
    proxy_username_field: Entity<TextField>,
    proxy_password_field: Entity<TextField>,
    proxy_feedback: Option<Notice>,
    proxy_test_running: bool,
    css_feedback: Option<Notice>,
    security_error: Option<String>,
    shortcut_error: Option<String>,
    boss_key_error: Option<String>,
    palette_shortcut_error: Option<String>,
    remove_password_confirm: bool,
    pending_import: Option<ImportPreview>,
    reset_confirm: bool,
    storage_confirm: Option<StorageConfirm>,
    storage_confirm_error: Option<String>,
    feedback: Option<Notice>,
    memory_feedback: Option<Notice>,
    diagnostics_preview: Option<String>,
    diagnostics_feedback: Option<Notice>,
    diagnostics_running: bool,
    file_dialog_open: bool,
    operation_running: bool,
    appearance_subscription: Option<Subscription>,
}

impl SettingsWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        tracing::info!("SettingsWindow::new");
        let password_field = cx.new(|cx| TextField::new(t!("New password"), cx).masked(true));
        let confirm_field = cx.new(|cx| TextField::new(t!("Confirm password"), cx).masked(true));
        let shortcut = model.read(cx).settings.shortcut.clone();
        let boss_key = model.read(cx).settings.boss_key.clone();
        let palette_shortcut = model.read(cx).settings.command_palette_shortcut.clone();
        let shortcut_field = cx.new(|cx| TextField::new("Ctrl+Shift+W", cx).with_content(shortcut));
        let boss_key_field = cx.new(|cx| TextField::new("Ctrl+Shift+H", cx).with_content(boss_key));
        let palette_shortcut_field = cx.new(|cx| {
            TextField::new(t!("Optional, e.g. Ctrl+Shift+K"), cx).with_content(palette_shortcut)
        });
        let quiet_hours = model.read(cx).settings.do_not_disturb.schedule.clone();
        let dnd_start_field =
            cx.new(|cx| TextField::new("22:00", cx).with_content(quiet_hours.start.clone()));
        let dnd_end_field =
            cx.new(|cx| TextField::new("07:00", cx).with_content(quiet_hours.end.clone()));
        let downloads_dir = model
            .read(cx)
            .settings
            .downloads_directory
            .clone()
            .unwrap_or_else(|| model.read(cx).paths.downloads_dir.display().to_string());
        let downloads_dir_field =
            cx.new(|cx| TextField::new(t!("Download folder"), cx).with_content(downloads_dir));
        let proxy = model.read(cx).settings.network.proxy.clone();
        let proxy_mode = proxy.mode;
        let proxy_protocol = proxy.manual.protocol;
        let proxy_host_field = cx.new(|cx| {
            TextField::new("proxy.example.com", cx).with_content(proxy.manual.host.clone())
        });
        let proxy_port_text = if proxy.manual.port == 0 {
            String::new()
        } else {
            proxy.manual.port.to_string()
        };
        let proxy_port_field =
            cx.new(|cx| TextField::new("8080", cx).with_content(proxy_port_text));
        let proxy_username_field = cx.new(|cx| {
            TextField::new(t!("Optional username"), cx).with_content(proxy.manual.username.clone())
        });
        let proxy_password_field = cx.new(|cx| {
            TextField::new(t!("Optional password"), cx)
                .masked(true)
                .with_content(proxy.manual.password.clone())
        });
        register_field_tab_stop(&password_field, cx);
        register_field_tab_stop(&confirm_field, cx);
        register_field_tab_stop(&shortcut_field, cx);
        register_field_tab_stop(&boss_key_field, cx);
        register_field_tab_stop(&palette_shortcut_field, cx);
        register_field_tab_stop(&dnd_start_field, cx);
        register_field_tab_stop(&dnd_end_field, cx);
        register_field_tab_stop(&downloads_dir_field, cx);
        register_field_tab_stop(&proxy_host_field, cx);
        register_field_tab_stop(&proxy_port_field, cx);
        register_field_tab_stop(&proxy_username_field, cx);
        register_field_tab_stop(&proxy_password_field, cx);
        Self {
            model,
            section: SettingsSection::Accounts,
            focus_handle: cx.focus_handle(),
            renaming: None,
            identity_edit: None,
            remove_confirm: None,
            apply_appearance_confirm: None,
            account_appearance_feedback: None,
            password_field,
            confirm_field,
            shortcut_field,
            boss_key_field,
            palette_shortcut_field,
            dnd_start_field,
            dnd_end_field,
            dnd_error: None,
            downloads_dir_field,
            downloads_error: None,
            proxy_mode,
            proxy_protocol,
            proxy_host_field,
            proxy_port_field,
            proxy_username_field,
            proxy_password_field,
            proxy_feedback: None,
            proxy_test_running: false,
            css_feedback: None,
            security_error: None,
            shortcut_error: None,
            boss_key_error: None,
            palette_shortcut_error: None,
            remove_password_confirm: false,
            pending_import: None,
            reset_confirm: false,
            storage_confirm: None,
            storage_confirm_error: None,
            feedback: None,
            memory_feedback: None,
            diagnostics_preview: None,
            diagnostics_feedback: None,
            diagnostics_running: false,
            file_dialog_open: false,
            operation_running: false,
            appearance_subscription: None,
        }
    }

    fn set_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if section != SettingsSection::Storage {
            self.storage_confirm = None;
            self.storage_confirm_error = None;
        }
        if section != SettingsSection::Accounts {
            self.apply_appearance_confirm = None;
        }
        self.section = section;
        if section == SettingsSection::Storage {
            self.model.update(cx, |model, cx| model.refresh_storage(cx));
        }
        cx.notify();
    }

    fn handle_escape(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key != "escape" {
            return;
        }
        let closed_rename = self.renaming.is_some();
        if self.cancel_pending_dialog(cx) {
            if closed_rename {
                window.defer(cx, |window, _cx| window.focus_next());
            }
            return;
        }
        window.remove_window();
    }

    fn cancel_pending_dialog(&mut self, cx: &mut Context<Self>) -> bool {
        let pending = self.renaming.is_some()
            || self.remove_confirm.is_some()
            || self.apply_appearance_confirm.is_some()
            || self.remove_password_confirm
            || self.pending_import.is_some()
            || self.reset_confirm
            || self.storage_confirm.is_some();
        if !pending {
            return false;
        }
        self.renaming = None;
        self.remove_confirm = None;
        self.apply_appearance_confirm = None;
        self.remove_password_confirm = false;
        self.pending_import = None;
        self.reset_confirm = false;
        self.storage_confirm = None;
        self.storage_confirm_error = None;
        cx.notify();
        true
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
            let identity = account.identity();
            let initial = account.initial();
            let unread = account.unread;
            let appearance =
                appearance_summary(settings.effective_theme(&id), settings.effective_zoom(&id));

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
                                .child(avatar(&identity.label, theme))
                                .child(field)
                                .child(button(
                                    SharedString::from(format!("rename-save-{save_id}")),
                                    t!("Save"),
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
                                    t!("Cancel"),
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

            if let Some(edit) = &self.identity_edit {
                if edit.account_id == id {
                    let edit = edit.clone();
                    let save_id = id.clone();
                    let reset_id = id.clone();
                    let cancel_id = id.clone();
                    let error = edit.error.clone();
                    list = list.child(
                        card(theme)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(12.))
                                    .child(avatar(&identity.label, theme))
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
                                                    .child(t!("Identity")),
                                            ),
                                    ),
                            )
                            .child(labeled_field(
                                t!("Custom color"),
                                Some(SharedString::from(
                                    t!("Use a hex color such as #1d4ed8."),
                                )),
                                theme,
                                edit.hex.clone(),
                            ))
                            .child(labeled_field(
                                t!("Avatar"),
                                Some(SharedString::from(
                                    t!("Use up to two characters or one emoji. Empty shows the account initial."),
                                )),
                                theme,
                                edit.avatar.clone(),
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
                                        SharedString::from(format!("identity-save-{save_id}")),
                                        t!("Save"),
                                        theme,
                                        true,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.save_identity(cx);
                                        }),
                                    ))
                                    .child(button(
                                        SharedString::from(format!("identity-reset-{reset_id}")),
                                        t!("Reset identity"),
                                        theme,
                                        false,
                                        cx.listener(move |this, _event, _window, cx| {
                                            this.reset_account_identity(&reset_id, cx);
                                        }),
                                    ))
                                    .child(button(
                                        SharedString::from(format!("identity-cancel-{cancel_id}")),
                                        t!("Cancel"),
                                        theme,
                                        false,
                                        cx.listener(|this, _event, _window, cx| {
                                            this.identity_edit = None;
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
                                .text_size(theme.font_px(13.))
                                .text_color(theme.text)
                                .child(t!("Remove this account?")),
                        )
                        .child(
                            div()
                                .text_size(theme.font_px(12.))
                                .text_color(theme.text_muted)
                                .child(t!("The local WhatsApp session, cookies, and cached files for this account will be deleted. This cannot be undone.")),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button(
                                    SharedString::from(format!("remove-yes-{remove_id}")),t!("Remove account"),
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
                                    SharedString::from(format!("remove-no-{cancel_id}")),t!("Cancel"),
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
                card(theme)
                    .child(
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
                                                        tn!(
                                                            unread,
                                                            "{count} unread message",
                                                            "{count} unread messages"
                                                        )
                                                    } else {
                                                        t!("No unread messages").to_string()
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(theme.text_faint)
                                                    .child(SharedString::from(appearance.clone())),
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
                                        t!("Open"),
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
                                        t!("Rename"),
                                        theme,
                                        false,
                                        cx.listener(move |this, _event, _window, cx| {
                                            let field = cx.new(|cx| {
                                                TextField::new(t!("Account name"), cx)
                                                    .with_content(rename_name.clone())
                                            });
                                            register_field_tab_stop(&field, cx);
                                            this.renaming = Some((rename_id.clone(), field));
                                            cx.notify();
                                        }),
                                    ))
                                    .when(settings.accounts.len() > 1, |style| {
                                        style.child(button(
                                            SharedString::from(format!("remove-{remove_id}")),
                                            t!("Remove"),
                                            theme,
                                            false,
                                            cx.listener(move |this, _event, _window, cx| {
                                                this.remove_confirm = Some(remove_id.clone());
                                                cx.notify();
                                            }),
                                        ))
                                    }),
                            ),
                    )
                    .child(divider(theme))
                    .child(self.render_account_appearance(account, settings, theme, cx)),
            );
        }

        let add_button = button(
            "add-account",
            t!("Add account"),
            theme,
            false,
            cx.listener(|this, _event, _window, cx| {
                this.model.update(cx, |model, cx| model.add_account(cx));
            }),
        );

        let mut page = div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                t!("Accounts"),
                t!("Every account has its own isolated WhatsApp Web session and window."),
                theme,
            ))
            .child(list);
        if let Some(notice) = &self.account_appearance_feedback {
            page = page.child(
                card(theme).child(
                    div()
                        .text_size(px(11.))
                        .text_color(if notice.error {
                            theme.danger
                        } else {
                            theme.text_muted
                        })
                        .child(notice.text.clone()),
                ),
            );
        }
        page.child(div().flex().gap(px(8.)).child(add_button))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Adding an account opens a new window with a fresh QR code."
                    )),
            )
            .into_any_element()
    }

    /// Theme and zoom controls for one account, shown inside its card. The
    /// effective values and the global fallback are visible without opening
    /// the Appearance section.
    fn render_account_appearance(
        &self,
        account: &Account,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let account_id = account.id.clone();
        let global_appearance = appearance_summary(settings.theme, settings.zoom);
        let effective_appearance = appearance_summary(
            settings.effective_theme(&account_id),
            settings.effective_zoom(&account_id),
        );

        let mut theme_row = div().flex().flex_wrap().gap(px(6.));
        for option in [
            AccountTheme::Inherit,
            AccountTheme::System,
            AccountTheme::Light,
            AccountTheme::Dark,
        ] {
            let selected = account.appearance.theme == option;
            let label = if option.is_inherit() {
                tr!("Use global ({value})", value = settings.theme.label())
            } else {
                option.label().to_string()
            };
            let id =
                SharedString::from(format!("account-theme-{account_id}-{option:?}").to_lowercase());
            let account_id = account_id.clone();
            theme_row = theme_row.child(button(
                id,
                label,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.set_account_theme(&account_id, option, cx);
                    });
                    this.account_appearance_feedback = None;
                    cx.notify();
                }),
            ));
        }

        let mut zoom_row = div().flex().flex_wrap().gap(px(6.));
        let inherit_id = account_id.clone();
        let global_zoom = format!("{}%", (settings.zoom * 100.0).round() as u32);
        zoom_row = zoom_row.child(button(
            SharedString::from(format!("account-zoom-{account_id}-global")),
            tr!("Use global ({value})", value = global_zoom),
            theme,
            account.appearance.zoom.is_none(),
            cx.listener(move |this, _event, _window, cx| {
                this.model.update(cx, |model, cx| {
                    model.set_account_zoom(&inherit_id, None, cx)
                });
                this.account_appearance_feedback = None;
                cx.notify();
            }),
        ));
        for zoom in ZOOM_LEVELS {
            let selected = account
                .appearance
                .zoom
                .is_some_and(|current| (current - zoom).abs() < 0.001);
            let label = format!("{}%", (zoom * 100.0).round() as u32);
            let id = SharedString::from(format!("account-zoom-{account_id}-{label}"));
            let account_id = account_id.clone();
            zoom_row = zoom_row.child(button(
                id,
                label,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.set_account_zoom(&account_id, Some(zoom), cx);
                    });
                    this.account_appearance_feedback = None;
                    cx.notify();
                }),
            ));
        }

        let mut apply_row = div().flex().flex_wrap().items_center().gap(px(8.));
        if self.apply_appearance_confirm.as_deref() == Some(account_id.as_str()) {
            let confirm_id = account_id.clone();
            let cancel_id = account_id.clone();
            apply_row = apply_row
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child(t!("Apply this account's appearance to all accounts?")),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(t!(
                            "Other accounts will use the same theme and zoom overrides. Their sessions and window bounds are kept."
                        )),
                )
                .child(button(
                    SharedString::from(format!("apply-appearance-confirm-{confirm_id}")),
                    t!("Apply to all accounts"),
                    theme,
                    true,
                    cx.listener(move |this, _event, _window, cx| {
                        this.apply_appearance_confirm = None;
                        this.model.update(cx, |model, cx| {
                            model.apply_account_appearance_to_all(&confirm_id, cx);
                        });
                        this.account_appearance_feedback =
                            Some(Notice::success(t!("Appearance applied to all accounts.")));
                        cx.notify();
                    }),
                ))
                .child(button(
                    SharedString::from(format!("apply-appearance-cancel-{cancel_id}")),
                    t!("Cancel"),
                    theme,
                    false,
                    cx.listener(|this, _event, _window, cx| {
                        this.apply_appearance_confirm = None;
                        cx.notify();
                    }),
                ));
        } else {
            let apply_id = account_id.clone();
            apply_row = apply_row.child(button(
                SharedString::from(format!("apply-appearance-{apply_id}")),
                t!("Apply to all accounts"),
                theme,
                false,
                cx.listener(move |this, _event, _window, cx| {
                    this.apply_appearance_confirm = Some(apply_id.clone());
                    cx.notify();
                }),
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(t!("Account appearance")),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(t!("Theme")),
            )
            .child(theme_row)
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(t!("Display size")),
            )
            .child(zoom_row)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(tr!(
                        "Global appearance: {appearance}",
                        appearance = global_appearance
                    )),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(tr!(
                        "This account: {appearance}",
                        appearance = effective_appearance
                    )),
            )
            .child(apply_row)
            .into_any_element()
    }

    /// Validates the inline identity editor and applies both values in one
    /// settings update, so a rejected color never leaves a half-applied avatar.
    fn save_identity(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.identity_edit.clone() else {
            return;
        };
        let hex = edit.hex.read(cx).content().trim().to_string();
        let avatar = edit.avatar.read(cx).content().trim().to_string();
        let accent = if hex.is_empty() {
            None
        } else {
            match AccountColor::parse(&hex) {
                Some(color) => Some(color.to_hex()),
                None => {
                    self.set_identity_error(t!("Use a hex color such as #1d4ed8."), cx);
                    return;
                }
            }
        };
        let avatar = if avatar.is_empty() {
            None
        } else {
            match normalize_avatar_label(&avatar) {
                Some(label) => Some(label),
                None => {
                    self.set_identity_error(t!("Use up to two characters or one emoji."), cx);
                    return;
                }
            }
        };
        let account_id = edit.account_id;
        self.model.update(cx, |model, cx| {
            model.update_settings(cx, |settings| {
                settings.set_account_accent(&account_id, accent.as_deref());
                settings.set_account_avatar(&account_id, avatar.as_deref());
            });
        });
        self.identity_edit = None;
        cx.notify();
    }

    fn set_identity_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(edit) = self.identity_edit.as_mut() {
            edit.error = Some(message.to_string());
        }
        cx.notify();
    }

    fn reset_account_identity(&mut self, account_id: &str, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.update_settings(cx, |settings| {
                settings.reset_account_identity(account_id);
            });
        });
        self.identity_edit = None;
        cx.notify();
    }

    fn request_storage_action(
        &mut self,
        account_id: &str,
        account_name: &str,
        operation: StorageOperation,
        cx: &mut Context<Self>,
    ) {
        match operation.confirmation_level() {
            ConfirmationLevel::None => {
                self.model.update(cx, |model, cx| {
                    model.start_storage_operation(account_id, operation, cx);
                });
            }
            ConfirmationLevel::Simple => {
                self.storage_confirm = Some(StorageConfirm::SignOut {
                    account_id: account_id.to_string(),
                    account_name: account_name.to_string(),
                });
                self.storage_confirm_error = None;
            }
            ConfirmationLevel::TypedName => {
                let field = cx.new(|cx| TextField::new(t!("Account name"), cx));
                register_field_tab_stop(&field, cx);
                self.storage_confirm = Some(StorageConfirm::DeleteData {
                    account_id: account_id.to_string(),
                    account_name: account_name.to_string(),
                    field,
                });
                self.storage_confirm_error = None;
            }
        }
        cx.notify();
    }

    fn render_storage(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (snapshot, scanning, running, notice, data_dir) = {
            let model = self.model.read(cx);
            (
                model.storage.snapshot.clone(),
                model.storage.scanning,
                model.storage.running.clone(),
                model.storage.notice.clone(),
                model.paths.data_dir.clone(),
            )
        };
        let total = snapshot
            .as_ref()
            .map(|snapshot| storage::format_bytes(snapshot.total_bytes))
            .unwrap_or_else(|| {
                if scanning {
                    t!("Scanning…").to_string()
                } else {
                    t!("Not scanned yet").to_string()
                }
            });
        let free = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.free_bytes)
            .map(storage::format_bytes)
            .unwrap_or_else(|| t!("Unavailable").to_string());
        let last_cleanup = settings
            .last_storage_cleanup
            .clone()
            .unwrap_or_else(|| t!("Never").to_string());

        let mut summary = card(theme)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(28.))
                    .child(storage_stat(t!("Total account data"), total, theme))
                    .child(storage_stat(t!("Free disk space"), free, theme))
                    .child(storage_stat(t!("Last cleanup"), last_cleanup, theme)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button(
                        "storage-refresh",
                        if scanning {
                            t!("Scanning…")
                        } else {
                            t!("Refresh")
                        },
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update(cx, |model, cx| model.refresh_storage(cx));
                            cx.notify();
                        }),
                    ))
                    .child(button(
                        "storage-open-folder",
                        t!("Open data folder"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.model
                                .update(cx, |model, cx| model.open_storage_folder(cx));
                        }),
                    )),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(tr!("Data folder: {path}", path = data_dir.display())),
            );
        if let Some(notice) = notice {
            summary = summary.child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(if notice.error {
                        theme.danger
                    } else {
                        theme.text_muted
                    })
                    .child(notice.text),
            );
        }

        let pending = self.storage_confirm.clone();
        let mut accounts = div().flex().flex_col().gap(px(10.));
        for account in &settings.accounts {
            let account_id = account.id.clone();
            if let Some(confirm) = pending
                .as_ref()
                .filter(|confirm| confirm.account_id() == account_id.as_str())
            {
                accounts = accounts.child(self.render_storage_confirm(confirm, theme, cx));
                continue;
            }

            let name = account.name.clone();
            let clear_name = name.clone();
            let sign_out_name = name.clone();
            let delete_name = name.clone();
            let info_line = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.account(&account_id))
                .map(|info| {
                    let partial = if info.complete() {
                        ""
                    } else {
                        t!(" · partial")
                    };
                    tr!(
                        "Data {data} · Cache {cache} · Total {total}{partial}",
                        data = storage::format_bytes(info.data.bytes),
                        cache = storage::format_bytes(info.cache.bytes),
                        total = storage::format_bytes(info.total_bytes()),
                        partial = partial
                    )
                })
                .unwrap_or_else(|| {
                    if scanning {
                        t!("Scanning…").to_string()
                    } else {
                        t!("Not scanned yet").to_string()
                    }
                });
            let open = self.model.read(cx).is_account_window_open(&account_id);
            let working = running
                .as_ref()
                .is_some_and(|running| running.account_id == account_id);

            let mut entry = card(theme).child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(theme.font_px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(theme.font_px(11.))
                                    .text_color(theme.text_muted)
                                    .child(info_line),
                            ),
                    )
                    .when(open, |style| {
                        style.child(
                            div()
                                .text_size(theme.font_px(10.))
                                .text_color(theme.text_faint)
                                .child(t!("Window open")),
                        )
                    }),
            );

            if working {
                entry = entry.child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.text_muted)
                        .child(t!("Running storage operation…")),
                );
            } else {
                let clear_id = account_id.clone();
                let sign_out_id = account_id.clone();
                let delete_id = account_id.clone();
                entry = entry.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(button(
                            SharedString::from(format!("storage-clear-cache-{clear_id}")),
                            t!("Clear cache"),
                            theme,
                            false,
                            cx.listener(move |this, _event, _window, cx| {
                                this.request_storage_action(
                                    &clear_id,
                                    &clear_name,
                                    StorageOperation::ClearCache,
                                    cx,
                                );
                            }),
                        ))
                        .child(button(
                            SharedString::from(format!("storage-sign-out-{sign_out_id}")),
                            t!("Sign out"),
                            theme,
                            false,
                            cx.listener(move |this, _event, _window, cx| {
                                this.request_storage_action(
                                    &sign_out_id,
                                    &sign_out_name,
                                    StorageOperation::ClearSession,
                                    cx,
                                );
                            }),
                        ))
                        .child(button(
                            SharedString::from(format!("storage-delete-{delete_id}")),
                            t!("Delete account data"),
                            theme,
                            false,
                            cx.listener(move |this, _event, _window, cx| {
                                this.request_storage_action(
                                    &delete_id,
                                    &delete_name,
                                    StorageOperation::RemoveData,
                                    cx,
                                );
                            }),
                        )),
                );
            }
            accounts = accounts.child(entry);
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(t!("Storage"),t!("Inspect and clean the WebKit profile behind each account."),
                theme,
            ))
            .child(summary)
            .child(accounts)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Clearing data closes and recreates the account's webview. Clearing the cache keeps the WhatsApp session; signing out requires the QR code again. Sizes are approximate while WhatsApp Web is running and stop at a bounded scan limit.",
                    ))
                    .child(t!(
                        "Deleting account data requires typing the account name and removes the account from OxideZap. The last account cannot be deleted.",
                    )),
            )
            .into_any_element()
    }

    fn render_storage_confirm(
        &mut self,
        confirm: &StorageConfirm,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let error = self.storage_confirm_error.clone();
        match confirm.clone() {
            StorageConfirm::SignOut {
                account_id,
                account_name,
            } => {
                let confirm_id = account_id.clone();
                let cancel_id = account_id.clone();
                let name = account_name.clone();
                card(theme)
                    .border_color(theme.danger)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text)
                            .child(tr!("Sign out of “{name}”?", name = name)),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_muted)
                            .child(t!(
                                "The account and its settings are kept, but its WebKit data is deleted. WhatsApp Web will ask for the QR code again.",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                SharedString::from(format!("storage-sign-out-yes-{confirm_id}")),t!("Sign out"),
                                theme,
                                true,
                                cx.listener(move |this, _event, _window, cx| {
                                    this.storage_confirm = None;
                                    this.storage_confirm_error = None;
                                    this.model.update(cx, |model, cx| {
                                        model.start_storage_operation(
                                            &confirm_id,
                                            StorageOperation::ClearSession,
                                            cx,
                                        );
                                    });
                                    cx.notify();
                                }),
                            ))
                            .child(button(
                                SharedString::from(format!("storage-sign-out-no-{cancel_id}")),t!("Cancel"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.storage_confirm = None;
                                    this.storage_confirm_error = None;
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element()
            }
            StorageConfirm::DeleteData {
                account_id,
                account_name,
                field,
            } => {
                let delete_id = account_id.clone();
                let cancel_id = account_id.clone();
                let expected = account_name.clone();
                let name = account_name.clone();
                card(theme)
                    .border_color(theme.danger)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text)
                            .child(tr!("Delete all data for “{name}”?", name = name)),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_muted)
                            .child(t!(
                                "This closes the account, deletes its WebKit profile, and removes it from OxideZap. This cannot be undone.",
                            )),
                    )
                    .child(labeled_field(t!("Type the account name to confirm"),
                        None,
                        theme,
                        field.clone(),
                    ))
                    .when_some(error, |style, error| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                SharedString::from(format!("storage-delete-yes-{delete_id}")),t!("Delete account data"),
                                theme,
                                true,
                                cx.listener(move |this, _event, _window, cx| {
                                    let typed = field.read(cx).content().to_string();
                                    if !storage::typed_confirmation_matches(&typed, &expected) {
                                        this.storage_confirm_error =
                                            Some(t!("Type the account name exactly to confirm.").to_string());
                                        cx.notify();
                                        return;
                                    }
                                    this.storage_confirm = None;
                                    this.storage_confirm_error = None;
                                    this.model.update(cx, |model, cx| {
                                        model.start_storage_operation(
                                            &delete_id,
                                            StorageOperation::RemoveData,
                                            cx,
                                        );
                                    });
                                    cx.notify();
                                }),
                            ))
                            .child(button(
                                SharedString::from(format!("storage-delete-no-{cancel_id}")),t!("Cancel"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.storage_confirm = None;
                                    this.storage_confirm_error = None;
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element()
            }
        }
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
            let id = SharedString::from(format!("theme-{mode:?}").to_lowercase());
            let description = match mode {
                ThemeMode::System => t!("Follow the desktop"),
                ThemeMode::Light => t!("Bright surfaces"),
                ThemeMode::Dark => t!("Low-light surfaces"),
            };
            let description = if selected {
                tr!("Selected · {description}", description = description)
            } else {
                description.to_string()
            };
            theme_cards = theme_cards.child(
                div()
                    .id(id)
                    .tab_index(TAB_INDEX)
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
                    .focus(move |style| {
                        style
                            .border_2()
                            .border_color(theme.text)
                            .bg(theme.surface_hover)
                    })
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.model.update(cx, |model, cx| model.set_theme(mode, cx));
                    }))
                    .child(
                        div()
                            .text_size(theme.font_px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(mode.label()),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_muted)
                            .child(description),
                    ),
            );
        }

        let mut zoom_row = div().flex().flex_wrap().gap(px(8.));
        for zoom in ZOOM_LEVELS {
            let label = format!("{}%", (zoom * 100.0).round() as u32);
            let id = format!("zoom-{label}");
            let selected = (settings.zoom - zoom).abs() < 0.001;
            let text = if selected {
                format!("✓ {label}")
            } else {
                label
            };
            zoom_row = zoom_row.child(button(
                SharedString::from(id),
                text,
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
            .child(section_header(t!("Appearance"),t!("Choose how the native shell looks and how large WhatsApp Web is rendered."),
                theme,
            ))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Theme")),
                    )
                    .child(theme_cards)
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!(
                                "System follows the desktop color scheme live. On Linux this depends on the XDG desktop portal and the desktop settings service, so some window managers may not report changes.",
                            )),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Display size")),
                    )
                    .child(zoom_row)
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(tr!(
                                "WhatsApp Web zoom is limited to {min}%–{max}%. It does not resize the native UI; use Accessibility for native text size.",
                                min = (MIN_ZOOM * 100.0) as u32,
                                max = (MAX_ZOOM * 100.0) as u32
                            )),
                    ),
            )
            .child(self.render_experimental_css(settings, theme, cx))
            .into_any_element()
    }

    fn render_accessibility(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accessibility = &settings.accessibility;
        let high_contrast = accessibility.high_contrast;
        let reduce_motion = accessibility.reduce_motion;

        let mut scale_row = div().flex().flex_wrap().gap(px(8.));
        for scale in FontScale::ALL {
            let selected = accessibility.font_scale == scale;
            let label = format!("{}%", scale.percent());
            let text = if selected {
                format!("✓ {label}")
            } else {
                label
            };
            scale_row = scale_row.child(button(
                SharedString::from(format!("font-scale-{}", scale.percent())),
                text,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update_settings(cx, |settings| {
                        settings.set_font_scale(scale);
                    });
                }),
            ));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                t!("Accessibility"),
                t!("Contrast, text size, and motion preferences for the native shell. They are saved with your settings and included in exports."),
                theme,
            ))
            .child(
                card(theme)
                    .child(toggle_row(
                        "high-contrast",
                        t!("High contrast"),
                        t!("Use a stronger palette for text, backgrounds, and borders in the native shell. The System, Light, and Dark choice still picks the base palette; WhatsApp Web content is unchanged."),
                        high_contrast,
                        theme,
                        cx.listener(move |this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.set_high_contrast(!high_contrast);
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Text size")),
                    )
                    .child(scale_row)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .p(px(10.))
                            .rounded_md()
                            .bg(theme.surface_alt)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .text_size(theme.font_px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(t!("Preview text")),
                            )
                            .child(
                                div()
                                    .text_size(theme.font_px(12.))
                                    .text_color(theme.text_muted)
                                    .child(tr!(
                                        "This text is shown at {percent}% of its base size.",
                                        percent = accessibility.font_scale.percent()
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("Text size scales the Settings window, the lock screen, and the native text in account windows. Icons, spacing, window chrome, text-field contents, the command palette, and the other native windows keep their fixed sizes; WhatsApp Web zoom is separate.")),
                    ),
            )
            .child(
                card(theme)
                    .child(toggle_row(
                        "reduce-motion",
                        t!("Reduce motion"),
                        t!("Pause CSS animations and transitions in WhatsApp Web. The native shell has no animated transitions today; any future native transition must honor this setting too. It works independently of battery saver."),
                        reduce_motion,
                        theme,
                        cx.listener(move |this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.set_reduce_motion(!reduce_motion);
                            });
                        }),
                    ))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("Reduce motion is independent of battery saver and stays active when the saver is off or the device is on AC power.")),
                    ),
            )
            .child(
                card(theme).child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.text_faint)
                        .child(t!("GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so screen readers cannot inspect these native controls. Keyboard focus and the visible focus ring still work, and WhatsApp Web keeps WebKit's own accessibility tree.")),
                ),
            )
            .into_any_element()
    }

    fn render_experimental_css(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let appearance = &settings.appearance;
        let css_size = appearance.custom_css.trim().len();
        let status = if css_size == 0 {
            t!("No stylesheet loaded.").to_string()
        } else {
            tr!(
                "{size} · {state}",
                size = css_size_label(css_size),
                state = if appearance.experimental_css {
                    t!("applied")
                } else {
                    t!("not applied")
                }
            )
        };
        let preview = css_preview(&appearance.custom_css);
        let enabled = appearance.experimental_css;
        let busy = self.file_dialog_open || self.operation_running;
        let feedback = self.css_feedback.clone();

        let mut card = card(theme)
            .child(
                div()
                    .text_size(theme.font_px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(t!("Experimental CSS")),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(theme.font_px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.danger)
                            .child(t!("Unsupported and experimental")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_muted)
                            .child(t!(
                                "WhatsApp Web changes its DOM without notice. A stylesheet can break the QR login, calls, dialogs, or the chat view, and may stop working after any WhatsApp update. Do not hide security, privacy, or abuse-reporting controls. OxideZap injects CSS only: it never runs JavaScript from this file and never downloads remote themes.",
                            )),
                    ),
            )
            .child(toggle_row(
                "experimental-css-enabled",t!("Apply experimental CSS"),t!("Enable the loaded stylesheet on every open WhatsApp Web account."),
                enabled,
                theme,
                cx.listener(move |this, _event, _window, cx| {
                    let result = this
                        .model
                        .update(cx, |model, cx| model.set_custom_css_enabled(!enabled, cx));
                    this.css_feedback = Some(match result {
                        Ok(()) if enabled => Notice::success(t!("Experimental CSS disabled.")),
                        Ok(()) => Notice::success(t!("Experimental CSS enabled.")),
                        Err(error) => Notice::error(error),
                    });
                    cx.notify();
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .when(!busy, |row| {
                        row.child(button(
                            "load-custom-css",t!("Load CSS file\u{2026}"),
                            theme,
                            false,
                            cx.listener(|this, _event, _window, cx| {
                                this.start_load_css(cx);
                            }),
                        ))
                    })
                    .child(button(
                        "reset-custom-css",t!("Reset"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.reset_custom_css(cx);
                        }),
                    ))
                    .when(busy, |row| {
                        row.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.text_muted)
                                .child(t!("Waiting for the file dialog\u{2026}")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(10.))
                    .rounded_md()
                    .bg(theme.surface_alt)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(status),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_muted)
                            .child(preview),
                    ),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Restricted to https://web.whatsapp.com · 64 KiB limit · CSS only · excluded from settings exports.",
                    )),
            );

        if let Some(notice) = feedback {
            card = card.child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(if notice.error {
                        theme.danger
                    } else {
                        theme.text_muted
                    })
                    .child(notice.text),
            );
        }

        card.into_any_element()
    }

    fn render_notifications(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut per_account = div().flex().flex_col().gap(px(2.));
        for (index, account) in settings.accounts.iter().enumerate() {
            if index > 0 {
                per_account = per_account.child(divider(theme));
            }
            let account_id = account.id.clone();
            let name = account.name.clone();
            let muted = account.muted;
            let sound = account.notification_sound;
            let state = if muted {
                t!("Muted")
            } else if sound {
                t!("Notifications and sound")
            } else {
                t!("Notifications, silent")
            };
            let mute_target = account_id.clone();
            let sound_target = account_id.clone();
            per_account = per_account
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.))
                        .py(px(8.))
                        .child(
                            div()
                                .text_size(theme.font_px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.text_faint)
                                .child(state),
                        ),
                )
                .child(toggle_row(
                    SharedString::from(format!("mute-{account_id}")),
                    t!("Mute notifications"),
                    t!("Suppress native message notifications from this account."),
                    muted,
                    theme,
                    cx.listener(move |this, _event, _window, cx| {
                        let account_id = mute_target.clone();
                        this.model.update(cx, |model, cx| {
                            model.update_settings(cx, |settings| {
                                settings.set_muted(&account_id, !muted);
                            });
                        });
                    }),
                ))
                .child(toggle_row(
                    SharedString::from(format!("sound-{account_id}")),
                    t!("Notification sound"),
                    t!("Ask the desktop notification service to play a message sound."),
                    sound,
                    theme,
                    cx.listener(move |this, _event, _window, cx| {
                        let account_id = sound_target.clone();
                        this.model.update(cx, |model, cx| {
                            model.update_settings(cx, |settings| {
                                settings.set_notification_sound(&account_id, !sound);
                            });
                        });
                    }),
                ));
        }

        let dnd = &settings.do_not_disturb;
        let now = SystemTime::now();
        let dnd_active = dnd.is_active(now);
        let dnd_status = dnd.status_text(now);
        let dnd_mode = dnd.mode.label();
        let schedule_days = dnd.schedule.days;
        let wake_time = dnd.schedule.end.clone();
        let scheduled_active = dnd.schedule_is_active(now);
        let start_field = self.dnd_start_field.clone();
        let end_field = self.dnd_end_field.clone();

        let mut dnd_presets = div().flex().flex_wrap().gap(px(8.));
        dnd_presets = dnd_presets
            .child(button(
                "dnd-one-hour",
                t!("For 1 hour"),
                theme,
                false,
                cx.listener(|this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings
                                .do_not_disturb
                                .set_until(SystemTime::now() + Duration::from_secs(60 * 60));
                        });
                    });
                }),
            ))
            .child(button(
                "dnd-tomorrow",
                tr!("Until tomorrow ({wake})", wake = wake_time),
                theme,
                false,
                cx.listener(|this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            let now = SystemTime::now();
                            let until = settings.do_not_disturb.until_tomorrow(now);
                            settings.do_not_disturb.set_until(until);
                        });
                    });
                }),
            ))
            .child(button(
                "dnd-indefinite",
                t!("Indefinitely"),
                theme,
                false,
                cx.listener(|this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.do_not_disturb.set_indefinite();
                        });
                    });
                }),
            ))
            .child(button(
                "dnd-use-quiet-hours",
                if dnd.mode == DoNotDisturbMode::Scheduled {
                    format!("✓ {}", t!("Quiet hours"))
                } else {
                    t!("Quiet hours").to_string()
                },
                theme,
                dnd.mode == DoNotDisturbMode::Scheduled,
                cx.listener(|this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.do_not_disturb.set_scheduled();
                        });
                    });
                }),
            ));

        let mut days_row = div().flex().flex_wrap().gap(px(8.));
        for (id, label, days) in [
            ("dnd-days-every", t!("Every day"), EVERY_DAY),
            ("dnd-days-weekdays", t!("Weekdays"), WEEKDAYS),
            ("dnd-days-weekends", t!("Weekends"), WEEKENDS),
        ] {
            let selected = schedule_days == days;
            let text = if selected {
                format!("✓ {label}")
            } else {
                label.to_string()
            };
            days_row = days_row.child(button(
                id,
                text,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.do_not_disturb.schedule.days = days;
                            settings.do_not_disturb.set_scheduled();
                        });
                    });
                }),
            ));
        }

        let mut quiet_actions = div().flex().flex_wrap().gap(px(8.)).child(button(
            "dnd-apply-quiet-hours",
            t!("Apply quiet hours"),
            theme,
            true,
            cx.listener(|this, _event, _window, cx| this.apply_quiet_hours(cx)),
        ));
        if scheduled_active {
            quiet_actions = quiet_actions.child(button(
                "dnd-skip-quiet-hours",
                t!("Skip this quiet period"),
                theme,
                false,
                cx.listener(|this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            let now = SystemTime::now();
                            settings.do_not_disturb.override_current_period(now);
                        });
                    });
                }),
            ));
        }

        let dnd_card = card(theme)
            .child(toggle_row(
                "do-not-disturb",t!("Do not disturb"),t!("Pause native notifications and sounds from every account. Unread counts, window titles, and the tray badge keep updating."),
                dnd_active,
                theme,
                cx.listener(move |this, _event, _window, cx| {
                    let enable = !dnd_active;
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.set_do_not_disturb(enable);
                        });
                    });
                }),
            ))
            .child(
                div()
                    .text_size(theme.font_px(13.))
                    .text_color(theme.text_muted)
                    .child(tr!(
                        "Mode: {mode} · Status: {status}",
                        mode = dnd_mode,
                        status = dnd_status
                    )),
            )
            .child(dnd_presets)
            .child(divider(theme))
            .child(
                div()
                    .text_size(theme.font_px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(t!("Quiet hours")),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(tr!(
                        "Recurring window: {window}.",
                        window = dnd.schedule.summary()
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().w(px(150.)).child(labeled_field(t!("Start"),
                        Some(SharedString::from(t!("HH:MM, local time"))),
                        theme,
                        start_field,
                    )))
                    .child(div().w(px(150.)).child(labeled_field(t!("End"),
                        Some(SharedString::from(t!("HH:MM, may cross midnight"))),
                        theme,
                        end_field,
                    ))),
            )
            .child(days_row)
            .when_some(self.dnd_error.clone(), |card, error| {
                card.child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(quiet_actions)
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!("Quiet hours use the desktop's local time. Overnight ranges such as 22:00-07:00 are supported; each period belongs to the day it starts. Times are re-evaluated when the clock changes or the machine resumes, and daylight-saving transitions follow the desktop timezone.")),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!("'Until tomorrow' uses the quiet-hours end time as its wake hour. Changing the times requires Apply; toggles and presets apply immediately.")),
            );

        let hide_when_locked = settings.hide_notification_previews_when_locked;
        let mut privacy_levels = div().flex().flex_wrap().gap(px(8.));
        for level in NotificationPrivacy::ALL {
            let selected = settings.notification_privacy == level;
            let text = if selected {
                format!("✓ {}", level.label())
            } else {
                level.label().to_string()
            };
            privacy_levels = privacy_levels.child(button(
                SharedString::from(format!("notification-privacy-{}", level.slug())),
                text,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.set_notification_privacy(level);
                        });
                    });
                }),
            ));
        }

        let mut examples = div().flex().flex_col().gap(px(4.));
        for level in NotificationPrivacy::ALL {
            examples = examples.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(format!("{}: {}", level.label(), level.description())),
            );
        }

        let preview =
            notify::NotificationPrivacyPolicy::new(settings.notification_privacy, hide_when_locked)
                .message(false, "Alice", t!("See you at 18:00."));
        let preview_summary = preview.summary;
        let preview_body = preview.body;
        let mut preview_box = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(12.))
            .rounded_md()
            .bg(theme.surface_alt)
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(preview_summary),
            );
        if !preview_body.is_empty() {
            preview_box = preview_box.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(preview_body),
            );
        }

        let privacy_card = card(theme)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(t!("Notification privacy")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(t!(
                                "Notification privacy controls what the native notification shows. Unread counts, window titles, the tray badge, and the click target never change."
                            )),
                    ),
            )
            .child(privacy_levels)
            .child(examples)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Sender only falls back to Hidden when WhatsApp does not provide a sender."
                    )),
            )
            .child(divider(theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Preview")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(t!(
                                "This is what a message notification looks like with the current selection."
                            )),
                    )
                    .child(preview_box),
            )
            .child(toggle_row(
                "hide-previews-when-locked",
                t!("Hide previews while OxideZap is locked"),
                t!(
                    "While the app is locked, notifications use the Hidden level regardless of the selection above."
                ),
                hide_when_locked,
                theme,
                cx.listener(move |this, _event, _window, cx| {
                    this.model.update(cx, |model, cx| {
                        model.update_settings(cx, |settings| {
                            settings.set_hide_notification_previews_when_locked(!hide_when_locked);
                        });
                    });
                }),
            ))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "The notification daemon and the operating system may still store or show content on their own lock screen; OxideZap cannot control that."
                    )),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "The selected level applies to message and system notifications such as downloads and errors. Connection and crash notifications keep their full text."
                    )),
            );

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(t!("Notifications"),t!("WhatsApp Web notifications are delivered by the desktop notification service."),
                theme,
            ))
            .child(
                card(theme)
                    .child(toggle_row(
                        "notifications-enabled",t!("Desktop notifications"),t!("Show a native notification when a message arrives while OxideZap is in the background."),
                        settings.notifications,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.notifications = !settings.notifications;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "connection-notifications",t!("Connection notifications"),t!("Show a native notification when an account goes offline or reconnects."),
                        settings.connection_notifications,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.connection_notifications =
                                    !settings.connection_notifications;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "unread-title",
                        t!("Unread count in window title"),
                        t!("Prefix the title with the account's unread count, for example (3) OxideZap — Work."),
                        settings.show_unread_in_title,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.show_unread_in_title = !settings.show_unread_in_title;
                            });
                        }),
                    )),
            )
            .child(privacy_card)
            .child(dnd_card)
            .child(writing_card(settings, theme, cx))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Per-account")),
                    )
                    .child(per_account),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!("Muted accounts and Do not disturb have priority over per-account sounds."))
                    .child(t!("Connection notifications also follow Do not disturb and per-account mute."))
                    .child(t!("Sound playback depends on the notification service and its sound theme; some servers ignore the requested sound or always use their own.")),
            )
            .into_any_element()
    }

    fn start_export(&mut self, cx: &mut Context<Self>) {
        if self.file_dialog_open || self.operation_running {
            return;
        }
        self.file_dialog_open = true;
        self.feedback = None;
        self.pending_import = None;
        self.reset_confirm = false;
        let settings = self.model.read(cx).settings.clone();
        let (sender, receiver) = bounded(1);
        request_file_chooser(
            FileChooserMode::Save,
            export_file_name(SystemTime::now()),
            sender,
        );
        cx.spawn(async move |this, cx| {
            let Some(path) = wait_for_file_choice(receiver).await else {
                this.update(cx, |this, cx| {
                    this.file_dialog_open = false;
                    cx.notify();
                })
                .ok();
                return;
            };
            let path = json_path(path);
            let result = cx
                .background_executor()
                .spawn(async move {
                    let json = SettingsExport::new(&settings, SystemTime::now()).to_json()?;
                    std::fs::write(&path, json).map_err(|error| {
                        anyhow::anyhow!(
                            "{}",
                            tr!(
                                "Could not write {path}: {error}",
                                path = path.display(),
                                error = error
                            )
                        )
                    })?;
                    Ok::<PathBuf, anyhow::Error>(path)
                })
                .await;
            this.update(cx, |this, cx| {
                this.file_dialog_open = false;
                this.feedback = Some(match result {
                    Ok(path) => {
                        Notice::success(tr!("Settings exported to {path}.", path = path.display()))
                    }
                    Err(error) => {
                        Notice::error(tr!("Could not export settings: {error}", error = error))
                    }
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_import(&mut self, cx: &mut Context<Self>) {
        if self.file_dialog_open || self.operation_running {
            return;
        }
        self.file_dialog_open = true;
        self.feedback = None;
        self.pending_import = None;
        self.reset_confirm = false;
        let current = self.model.read(cx).settings.clone();
        let (sender, receiver) = bounded(1);
        request_file_chooser(FileChooserMode::Open, String::new(), sender);
        cx.spawn(async move |this, cx| {
            let Some(path) = wait_for_file_choice(receiver).await else {
                this.update(cx, |this, cx| {
                    this.file_dialog_open = false;
                    cx.notify();
                })
                .ok();
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move { ImportPreview::from_path(&path, &current) })
                .await;
            this.update(cx, |this, cx| {
                this.file_dialog_open = false;
                match result {
                    Ok(preview) => {
                        this.pending_import = Some(preview);
                        this.reset_confirm = false;
                    }
                    Err(error) => {
                        this.feedback = Some(Notice::error(tr!(
                            "Could not import settings: {error}",
                            error = error
                        )));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_diagnostics_action(&mut self, action: DiagnosticsAction, cx: &mut Context<Self>) {
        if self.diagnostics_running || self.file_dialog_open || self.operation_running {
            return;
        }
        let context = self.model.read(cx).diagnostics_context();
        self.diagnostics_running = true;
        self.diagnostics_feedback = None;
        if action == DiagnosticsAction::Save {
            self.file_dialog_open = true;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_executor()
                .spawn(async move { diagnostics::collect(&context).render_text() })
                .await;
            let feedback = match action {
                DiagnosticsAction::Preview => Notice::success("Diagnostics report generated."),
                DiagnosticsAction::Copy => Notice::success("Diagnostics copied to the clipboard."),
                DiagnosticsAction::Save => {
                    let (sender, receiver) = bounded(1);
                    request_file_chooser(
                        FileChooserMode::Diagnostics,
                        diagnostics::report_file_name(SystemTime::now()),
                        sender,
                    );
                    match wait_for_file_choice(receiver).await {
                        None => Notice::success("Diagnostics save cancelled."),
                        Some(path) => {
                            let path = diagnostics::report_path(path);
                            let text = report.clone();
                            let result = cx
                                .background_executor()
                                .spawn(async move {
                                    std::fs::write(&path, text).map_err(|error| {
                                        anyhow::anyhow!(
                                            "could not write {}: {error}",
                                            path.display()
                                        )
                                    })?;
                                    Ok::<PathBuf, anyhow::Error>(path)
                                })
                                .await;
                            match result {
                                Ok(path) => Notice::success(format!(
                                    "Diagnostics saved to {}.",
                                    path.display()
                                )),
                                Err(error) => {
                                    Notice::error(format!("Could not save diagnostics: {error}"))
                                }
                            }
                        }
                    }
                }
            };
            this.update(cx, |this, cx| {
                if action == DiagnosticsAction::Copy {
                    cx.write_to_clipboard(ClipboardItem::new_string(report.clone()));
                }
                this.diagnostics_running = false;
                this.file_dialog_open = false;
                this.diagnostics_preview = Some(report);
                this.diagnostics_feedback = Some(feedback);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_quiet_hours(&mut self, cx: &mut Context<Self>) {
        let start = self.dnd_start_field.read(cx).content().trim().to_string();
        let end = self.dnd_end_field.read(cx).content().trim().to_string();
        let (Some(start_minutes), Some(end_minutes)) =
            (parse_time_of_day(&start), parse_time_of_day(&end))
        else {
            self.dnd_error = Some(t!("Enter both quiet-hours times as HH:MM.").to_string());
            cx.notify();
            return;
        };
        if start_minutes == end_minutes {
            self.dnd_error =
                Some(t!("The quiet-hours start and end times must differ.").to_string());
            cx.notify();
            return;
        }
        let start = format_time_of_day(start_minutes);
        let end = format_time_of_day(end_minutes);
        self.dnd_error = None;
        self.dnd_start_field.update(cx, |field, cx| {
            field.set_content(start.clone(), cx);
        });
        self.dnd_end_field
            .update(cx, |field, cx| field.set_content(end.clone(), cx));
        self.model.update_settings(cx, |settings| {
            settings.do_not_disturb.schedule.start = start;
            settings.do_not_disturb.schedule.end = end;
        });
        cx.notify();
    }

    fn apply_downloads_directory(&mut self, cx: &mut Context<Self>) {
        let default_dir = self.model.read(cx).paths.downloads_dir.clone();
        let value = self
            .downloads_dir_field
            .read(cx)
            .content()
            .trim()
            .to_string();
        let Some(path) = valid_base_dir(&value) else {
            self.downloads_error =
                Some(t!("Enter an absolute folder path, or use the default.").to_string());
            cx.notify();
            return;
        };
        let configured = (path != default_dir).then(|| path.display().to_string());
        self.downloads_error = None;
        self.downloads_dir_field.update(cx, |field, cx| {
            field.set_content(path.display().to_string(), cx)
        });
        self.model.update_settings(cx, |settings| {
            settings.downloads_directory = configured;
        });
        cx.notify();
    }

    fn use_default_downloads_directory(&mut self, cx: &mut Context<Self>) {
        let default_dir = self.model.read(cx).paths.downloads_dir.clone();
        self.downloads_error = None;
        self.downloads_dir_field.update(cx, |field, cx| {
            field.set_content(default_dir.display().to_string(), cx);
        });
        self.model.update_settings(cx, |settings| {
            settings.downloads_directory = None;
        });
        cx.notify();
    }

    fn start_choose_downloads_directory(&mut self, cx: &mut Context<Self>) {
        if self.file_dialog_open || self.operation_running {
            return;
        }
        self.file_dialog_open = true;
        let (sender, receiver) = bounded(1);
        request_file_chooser(FileChooserMode::Folder, String::new(), sender);
        cx.spawn(async move |this, cx| {
            let chosen = wait_for_file_choice(receiver).await;
            this.update(cx, |this, cx| {
                this.file_dialog_open = false;
                if let Some(path) = chosen {
                    this.downloads_dir_field.update(cx, |field, cx| {
                        field.set_content(path.display().to_string(), cx);
                    });
                    this.apply_downloads_directory(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_load_css(&mut self, cx: &mut Context<Self>) {
        if self.file_dialog_open || self.operation_running {
            return;
        }
        self.file_dialog_open = true;
        self.css_feedback = None;
        let (sender, receiver) = bounded(1);
        request_file_chooser(FileChooserMode::Css, String::new(), sender);
        cx.spawn(async move |this, cx| {
            let Some(path) = wait_for_file_choice(receiver).await else {
                this.update(cx, |this, cx| {
                    this.file_dialog_open = false;
                    cx.notify();
                })
                .ok();
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move { read_custom_css_file(&path) })
                .await;
            this.update(cx, |this, cx| {
                this.file_dialog_open = false;
                let result = result.and_then(|css| {
                    this.model
                        .update(cx, |model, cx| model.set_custom_css(css, cx))
                });
                this.css_feedback = Some(match result {
                    Ok(()) => {
                        let enabled = this.model.read(cx).settings.appearance.experimental_css;
                        if enabled {
                            Notice::success(t!("CSS loaded and applied to open accounts."))
                        } else {
                            Notice::success(t!("CSS loaded. Use the toggle above to apply it."))
                        }
                    }
                    Err(error) => Notice::error(error),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reset_custom_css(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.reset_custom_css(cx));
        self.css_feedback = Some(Notice::success(t!(
            "Experimental CSS removed. WhatsApp Web uses its own styles again."
        )));
        cx.notify();
    }

    fn apply_pending_import(&mut self, cx: &mut Context<Self>) {
        if self.operation_running {
            return;
        }
        let Some(preview) = self.pending_import.take() else {
            return;
        };
        self.operation_running = true;
        let current = self.model.read(cx).settings.clone();
        let backup_dir = self.model.read(cx).paths.settings_backup_dir();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { write_settings_backup(&backup_dir, &current, SystemTime::now()) })
                .await;
            this.update(cx, |this, cx| {
                this.operation_running = false;
                match result {
                    Ok(path) => {
                        this.model.update(cx, |model, cx| {
                            model.apply_imported_settings(preview.settings, cx);
                        });
                        this.feedback = Some(Notice::success(tr!(
                            "Settings imported. The previous settings were backed up to {path}.",
                            path = path.display()
                        )));
                    }
                    Err(error) => {
                        this.pending_import = Some(preview);
                        this.feedback = Some(Notice::error(tr!(
                            "Could not back up the current settings, so nothing was imported: {error}",
                            error = error
                        )));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reset_settings(&mut self, cx: &mut Context<Self>) {
        if self.operation_running {
            return;
        }
        self.reset_confirm = false;
        self.pending_import = None;
        self.operation_running = true;
        let current = self.model.read(cx).settings.clone();
        let backup_dir = self.model.read(cx).paths.settings_backup_dir();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { write_settings_backup(&backup_dir, &current, SystemTime::now()) })
                .await;
            this.update(cx, |this, cx| {
                this.operation_running = false;
                match result {
                    Ok(path) => {
                        this.model.update(cx, |model, cx| model.reset_settings(cx));
                        this.feedback = Some(Notice::success(tr!(
                            "Preferences reset to their defaults. Your accounts were kept and a backup was written to {path}.",
                            path = path.display()
                        )));
                    }
                    Err(error) => {
                        this.feedback = Some(Notice::error(tr!(
                            "Could not back up the current settings, so nothing was reset: {error}",
                            error = error
                        )));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn backup_card(&mut self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let feedback = self.feedback.clone();
        let pending_import = self.pending_import.clone();
        let reset_confirm = self.reset_confirm;
        let busy = self.file_dialog_open || self.operation_running;

        let actions = div()
            .flex()
            .gap(px(8.))
            .when(!busy, |style| {
                style
                    .child(button(
                        "export-settings",
                        t!("Export settings\u{2026}"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_export(cx);
                        }),
                    ))
                    .child(button(
                        "import-settings",
                        t!("Import settings\u{2026}"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_import(cx);
                        }),
                    ))
                    .child(button(
                        "reset-settings",
                        t!("Reset settings"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.pending_import = None;
                            this.reset_confirm = true;
                            cx.notify();
                        }),
                    ))
            })
            .when(busy, |style| {
                style.child(
                    div()
                        .text_size(theme.font_px(12.))
                        .text_color(theme.text_muted)
                        .child(t!(
                            "Waiting for the file dialog or the current operation\u{2026}"
                        )),
                )
            });

        let mut card = card(theme)
            .child(
                div()
                    .text_size(theme.font_px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(t!("Backup and restore")),
            )
            .child(
                div()
                    .text_size(theme.font_px(12.))
                    .text_color(theme.text_muted)
                    .child(t!(
                        "Export the configuration to a readable JSON file, import it on another installation, or restore the defaults.",
                    )),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Exports never include account sessions, cookies, chats, or the app-lock password. Every import and reset writes a dated backup first.",
                    )),
            )
            .child(actions);

        if let Some(notice) = feedback {
            card = card.child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(if notice.error {
                        theme.danger
                    } else {
                        theme.text_muted
                    })
                    .child(notice.text),
            );
        }

        if let Some(preview) = pending_import {
            let mut summary = div().flex().flex_col().gap(px(4.));
            for change in &preview.changes {
                summary = summary.child(
                    div()
                        .text_size(theme.font_px(12.))
                        .text_color(theme.text)
                        .child(change.clone()),
                );
            }
            card = card
                .child(divider(theme))
                .child(
                    div()
                        .text_size(theme.font_px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(t!("Import summary")),
                )
                .child(summary)
                .child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.text_faint)
                        .child(t!(
                            "Accounts from the file replace the current account list; local session data is not deleted. The app-lock password is not imported, so set it again afterwards.",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button(
                            "import-apply",t!("Apply import"),
                            theme,
                            true,
                            cx.listener(|this, _event, _window, cx| {
                                this.apply_pending_import(cx);
                            }),
                        ))
                        .child(button(
                            "import-cancel",t!("Cancel"),
                            theme,
                            false,
                            cx.listener(|this, _event, _window, cx| {
                                this.pending_import = None;
                                cx.notify();
                            }),
                        )),
                );
        }

        if reset_confirm {
            card = card
                .child(divider(theme))
                .child(
                    div()
                        .text_size(theme.font_px(12.))
                        .text_color(theme.text)
                        .child(t!("Reset preferences to their defaults?")),
                )
                .child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.text_faint)
                        .child(t!(
                            "Theme, zoom, notifications, startup, the global shortcut, the app-lock password, and the first-run guide status are reset. Accounts and their WhatsApp sessions are kept, and the first-run guide opens again. A dated backup is written first.",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button(
                            "reset-apply",t!("Reset settings"),
                            theme,
                            true,
                            cx.listener(|this, _event, _window, cx| {
                                this.reset_settings(cx);
                            }),
                        ))
                        .child(button(
                            "reset-cancel",t!("Cancel"),
                            theme,
                            false,
                            cx.listener(|this, _event, _window, cx| {
                                this.reset_confirm = false;
                                cx.notify();
                            }),
                        )),
                );
        }

        card.into_any_element()
    }

    fn collect_proxy(&self, cx: &App) -> Result<ProxySettings, String> {
        let port_text = self.proxy_port_field.read(cx).content().trim().to_string();
        let port = if self.proxy_mode == ProxyMode::Manual {
            if port_text.is_empty() {
                return Err(t!("Enter the proxy port.").to_string());
            }
            port_text.parse::<u16>().map_err(|_| {
                t!("The proxy port must be a number between 1 and 65535.").to_string()
            })?
        } else {
            port_text.parse().unwrap_or(ManualProxy::DEFAULT_PORT)
        };
        let proxy = ProxySettings {
            mode: self.proxy_mode,
            manual: ManualProxy {
                protocol: self.proxy_protocol,
                host: self.proxy_host_field.read(cx).content().trim().to_string(),
                port,
                username: self
                    .proxy_username_field
                    .read(cx)
                    .content()
                    .trim()
                    .to_string(),
                password: self.proxy_password_field.read(cx).content().to_string(),
            },
        };
        proxy.validate()?;
        Ok(proxy)
    }

    fn apply_network_proxy(&mut self, cx: &mut Context<Self>) {
        match self.collect_proxy(cx) {
            Ok(proxy) => {
                self.model
                    .update(cx, |model, cx| model.set_network_proxy(proxy, cx));
                self.proxy_feedback = Some(Notice::success(t!(
                    "Proxy settings saved. Reopen an account if the new route does not take effect.",
                )));
            }
            Err(error) => {
                self.proxy_feedback = Some(Notice::error(error));
            }
        }
        cx.notify();
    }

    fn start_proxy_test(&mut self, cx: &mut Context<Self>) {
        if self.proxy_test_running {
            return;
        }
        let proxy = match self.collect_proxy(cx) {
            Ok(proxy) => proxy,
            Err(error) => {
                self.proxy_feedback = Some(Notice::error(error));
                cx.notify();
                return;
            }
        };
        self.proxy_test_running = true;
        self.proxy_feedback = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { test_proxy_connection(&proxy) })
                .await;
            this.update(cx, |this, cx| {
                this.proxy_test_running = false;
                this.proxy_feedback = Some(match result {
                    Ok(message) => Notice::success(message),
                    Err(error) => Notice::error(error),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_network(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut mode_row = div().flex().gap(px(10.));
        for mode in [ProxyMode::System, ProxyMode::NoProxy, ProxyMode::Manual] {
            let selected = self.proxy_mode == mode;
            let label = if selected {
                format!("✓ {}", mode.label())
            } else {
                mode.label().to_string()
            };
            mode_row = mode_row.child(button(
                SharedString::from(format!(
                    "proxy-mode-{}",
                    mode.label().to_lowercase().replace(' ', "-")
                )),
                label,
                theme,
                selected,
                cx.listener(move |this, _event, _window, cx| {
                    this.proxy_mode = mode;
                    this.proxy_feedback = None;
                    cx.notify();
                }),
            ));
        }

        let mut mode_card = card(theme)
            .child(
                div()
                    .text_size(theme.font_px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(t!("Connection mode")),
            )
            .child(mode_row)
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(self.proxy_mode.description()),
            );

        if self.proxy_mode == ProxyMode::Manual {
            let host_field = self.proxy_host_field.clone();
            let port_field = self.proxy_port_field.clone();
            let username_field = self.proxy_username_field.clone();
            let password_field = self.proxy_password_field.clone();

            let mut protocol_row = div().flex().gap(px(8.));
            for protocol in [
                ProxyProtocol::Http,
                ProxyProtocol::Https,
                ProxyProtocol::Socks,
            ] {
                let selected = self.proxy_protocol == protocol;
                let label = if selected {
                    format!("✓ {}", protocol.label())
                } else {
                    protocol.label().to_string()
                };
                protocol_row = protocol_row.child(button(
                    SharedString::from(format!("proxy-protocol-{}", protocol.label())),
                    label,
                    theme,
                    selected,
                    cx.listener(move |this, _event, _window, cx| {
                        this.proxy_protocol = protocol;
                        this.proxy_feedback = None;
                        cx.notify();
                    }),
                ));
            }

            mode_card = mode_card
                .child(divider(theme))
                .child(
                    div()
                        .text_size(theme.font_px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child(t!("Proxy protocol")),
                )
                .child(protocol_row)
                .child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().flex_1().child(labeled_field(t!("Host"),
                            None,
                            theme,
                            host_field,
                        )))
                        .child(div().w(px(120.)).child(labeled_field(t!("Port"),
                            None,
                            theme,
                            port_field,
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().flex_1().child(labeled_field(t!("Username"),
                            Some(SharedString::from(t!("Optional"))),
                            theme,
                            username_field,
                        )))
                        .child(div().flex_1().child(labeled_field(t!("Password"),
                            Some(SharedString::from(t!("Optional, stored as plain text"))),
                            theme,
                            password_field,
                        ))),
                )
                .child(
                    div()
                        .text_size(theme.font_px(11.))
                        .text_color(theme.danger)
                        .child(t!("Proxy credentials are stored as plain text in settings.json. The file is private (0600) and credentials are never logged or written to exports, but this build does not use a system keyring. Prefer a proxy that does not require a password.")),
                );
        }

        let running = self.proxy_test_running;
        mode_card = mode_card
            .child(divider(theme))
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!("The proxy is applied to each account's WebKit data manager when the account opens. Existing connections keep their route, so reopen an account if the change does not take effect.")),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_faint)
                    .child(t!("Test connection checks web.whatsapp.com through the configured route without blocking the interface. HTTP and SOCKS5 proxies are tested end to end; an HTTPS proxy is checked for reachability before the TLS handshake.")),
            )
            .child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(theme.text_muted)
                    .child(tr!(
                        "Saved configuration: {summary}",
                        summary = settings.network.proxy.redacted_summary()
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button(
                        "proxy-test-connection",
                        if running {
                            t!("Testing…")
                        } else {
                            t!("Test connection")
                        },
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_proxy_test(cx);
                        }),
                    ))
                    .child(button(
                        "proxy-apply",t!("Apply proxy settings"),
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            this.apply_network_proxy(cx);
                        }),
                    )),
            );

        if let Some(notice) = self.proxy_feedback.clone() {
            mode_card = mode_card.child(
                div()
                    .text_size(theme.font_px(11.))
                    .text_color(if notice.error {
                        theme.danger
                    } else {
                        theme.text_muted
                    })
                    .child(notice.text),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(t!("Network"),t!("Choose how WhatsApp Web reaches the network. Changes apply per account and may require reopening an account."),
                theme,
            ))
            .child(mode_card)
            .into_any_element()
    }

    fn render_general(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shortcut_field = self.shortcut_field.clone();
        let boss_key_field = self.boss_key_field.clone();
        let palette_shortcut_field = self.palette_shortcut_field.clone();
        let error = self.shortcut_error.clone();
        let boss_error = self.boss_key_error.clone();
        let palette_error = self.palette_shortcut_error.clone();
        let downloads_dir_field = self.downloads_dir_field.clone();
        let downloads_error = self.downloads_error.clone();
        let backup_card = self.backup_card(theme, cx);
        let onboarding_status = if settings.onboarding_completed {
            tr!(
                "Completed (guide version {version}). Reopening it here never marks it uncompleted.",
                version = settings.onboarding_version
            )
        } else {
            t!("Not completed. It opens automatically only on a new installation.").to_string()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(t!("General"),t!("Window behavior, startup, writing, downloads, media actions, and the global show/hide shortcut."),
                theme,
            ))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Interface language")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!(
                                "Translates the native shell and its windows. WhatsApp Web content keeps its own language."
                            )),
                    )
                    .child({
                        let current = settings.language;
                        let mut row = div().flex().flex_wrap().gap(px(8.));
                        for language in [
                            crate::i18n::Language::System,
                            crate::i18n::Language::English,
                            crate::i18n::Language::Spanish,
                        ] {
                            let selected = current == language;
                            let label = if selected {
                                format!("✓ {}", language.label())
                            } else {
                                language.label().to_string()
                            };
                            row = row.child(button(
                                SharedString::from(format!("language-{}", language.slug())),
                                label,
                                theme,
                                selected,
                                cx.listener(move |this, _event, _window, cx| {
                                    this.model.update_settings(cx, |settings| {
                                        settings.language = language;
                                    });
                                }),
                            ));
                        }
                        row
                    }),
            )
            .child(
                card(theme)
                    .child(toggle_row(
                        "close-to-tray",t!("Close to tray"),t!("Closing an account window hides it instead of quitting OxideZap."),
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
                        "start-at-login",t!("Launch at login"),t!("Start OxideZap automatically when you sign in to the desktop."),
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
                        "remember-bounds",t!("Remember window size and position"),t!("Restore each account window where you left it."),
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
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Global shortcut")),
                    )
                    .child(labeled_field(t!("Show or hide OxideZap"),
                        Some(SharedString::from(t!("Use modifiers plus one key, for example Ctrl+Shift+W."))),
                        theme,
                        shortcut_field,
                    ))
                    .when_some(error, |style, error| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(div().flex().gap(px(8.)).child(button(
                        "reset-shortcut",t!("Reset"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            let default = "Ctrl+Shift+W".to_string();
                            this.shortcut_field.update(cx, |field, cx| {
                                field.set_content(default.clone(), cx);
                            });
                            if let Ok(shortcut) = Shortcut::parse("Ctrl+Shift+W") {
                                let result = this.model.update(cx, |model, cx| {
                                    model.set_shortcut(shortcut, cx)
                                });
                                match result {
                                    Ok(()) => this.shortcut_error = None,
                                    Err(error) => this.shortcut_error = Some(error),
                                }
                            }
                            cx.notify();
                        }),
                    ))
                    .child(button(
                        "apply-shortcut",t!("Apply shortcut"),
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let value = this.shortcut_field.read(cx).content().to_string();
                            match Shortcut::parse(&value) {
                                Ok(shortcut) => {
                                    let result = this.model.update(cx, |model, cx| {
                                        model.set_shortcut(shortcut, cx)
                                    });
                                    match result {
                                        Ok(()) => this.shortcut_error = None,
                                        Err(error) => this.shortcut_error = Some(error),
                                    }
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.shortcut_error = Some(crate::i18n::translate_owned(&error.to_string()));
                                    cx.notify();
                                }
                            }
                        }),
                    )))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("Global shortcuts require an X11 or XWayland session.")),
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Boss key")),
                    )
                    .child(labeled_field(t!("Hide or restore every window"),
                        Some(SharedString::from(t!("A second global shortcut for instant privacy. A second press restores the windows that were visible."))),
                        theme,
                        boss_key_field,
                    ))
                    .when_some(boss_error, |style, error| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(div().flex().gap(px(8.)).child(button(
                        "apply-boss-key",t!("Apply boss key"),
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let value = this.boss_key_field.read(cx).content().to_string();
                            match Shortcut::parse(&value) {
                                Ok(shortcut) => {
                                    let result = this.model.update(cx, |model, cx| {
                                        model.set_boss_key(shortcut, cx)
                                    });
                                    match result {
                                        Ok(()) => this.boss_key_error = None,
                                        Err(error) => this.boss_key_error = Some(error),
                                    }
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.boss_key_error = Some(crate::i18n::translate_owned(&error.to_string()));
                                    cx.notify();
                                }
                            }
                        }),
                    )))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("The boss key uses the same X11/XWayland grab and can conflict with desktop shortcuts.")),
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Command palette")),
                    )
                    .child(labeled_field(t!("Optional global shortcut"),
                        Some(SharedString::from(t!("Opens the command palette while any application is focused. Leave empty to use Ctrl+K only inside the native window."))),
                        theme,
                        palette_shortcut_field,
                    ))
                    .when_some(palette_error, |style, error| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(div().flex().gap(px(8.)).child(button(
                        "apply-palette-shortcut",t!("Apply palette shortcut"),
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let value = this
                                .palette_shortcut_field
                                .read(cx)
                                .content()
                                .to_string();
                            if value.trim().is_empty() {
                                let _ = this.model.update(cx, |model, cx| {
                                    model.set_command_palette_shortcut(None, cx)
                                });
                                this.palette_shortcut_error = None;
                                cx.notify();
                                return;
                            }
                            match Shortcut::parse(&value) {
                                Ok(shortcut) => {
                                    let result = this.model.update(cx, |model, cx| {
                                        model.set_command_palette_shortcut(Some(shortcut), cx)
                                    });
                                    match result {
                                        Ok(()) => this.palette_shortcut_error = None,
                                        Err(error) => {
                                            this.palette_shortcut_error = Some(error)
                                        }
                                    }
                                    cx.notify();
                                }
                                Err(error) => {
                                    this.palette_shortcut_error = Some(crate::i18n::translate_owned(&error.to_string()));
                                    cx.notify();
                                }
                            }
                        }),
                    )))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("A global palette shortcut takes precedence over WhatsApp Web's own Ctrl+K chat search.")),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Downloads")),
                    )
                    .child(labeled_field(t!("Download folder"),
                        Some(SharedString::from(t!("Leave the default to follow the desktop Downloads directory."))),
                        theme,
                        downloads_dir_field,
                    ))
                    .when_some(downloads_error, |style, error| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(button(
                                "choose-downloads-directory",t!("Choose…"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.start_choose_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "apply-downloads-directory",t!("Apply folder"),
                                theme,
                                true,
                                cx.listener(|this, _event, _window, cx| {
                                    this.apply_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "default-downloads-directory",t!("Use default"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.use_default_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "open-download-center",t!("Open Download Center"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.model.update(cx, |model, cx| {
                                        model.open_downloads_window(cx);
                                    });
                                }),
                            )),
                    )
                    .child(divider(theme))
                    .child(toggle_row(
                        "downloads-account-folders",t!("Organize by account"),t!("Save each account's files in its own subfolder."),
                        settings.downloads_account_folders,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.downloads_account_folders =
                                    !settings.downloads_account_folders;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "downloads-date-folders",t!("Organize by month"),t!("Add a YYYY-MM subfolder (UTC) for the month each file was downloaded."),
                        settings.downloads_date_folders,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.downloads_date_folders =
                                    !settings.downloads_date_folders;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "media-actions",t!("Media action menu"),t!("Right-click normal media to save it, open it, or copy an image. View-once and protected media are never offered."),
                        settings.media_actions,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.media_actions = !settings.media_actions;
                            });
                        }),
                    ))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(tr!(
                                "The download history keeps the last {count} entries. Existing files are never overwritten; collisions get a numbered suffix.",
                                count = DOWNLOAD_HISTORY_LIMIT
                            )),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("First-run guide")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text_muted)
                            .child(t!("Review the WhatsApp Web login, the QR steps, account isolation, tray and close behavior, notifications, the app lock, and where OxideZap stores data.")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(onboarding_status),
                    )
                    .child(div().flex().gap(px(8.)).child(button(
                        "replay-onboarding",t!("Show the first-run guide again"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update(cx, |model, cx| {
                                model.open_onboarding_window(cx);
                            });
                        }),
                    ))),
            )
            .child(backup_card)
            .into_any_element()
    }

    fn render_performance(
        &mut self,
        settings: &Settings,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (suspended, active_views, total_accounts, performance) = {
            let model = self.model.read(cx);
            (
                model.suspended_account_count(),
                model.active_webview_count(),
                model.settings.accounts.len(),
                model.performance_status(),
            )
        };
        let estimate = suspended as u64 * crate::app::ESTIMATED_SUSPENDED_MEMORY_BYTES;
        let feedback = self.memory_feedback.clone();
        let saver_active = performance.battery_saver;

        let mut mode_row = div().flex().flex_wrap().gap(px(8.));
        for mode in PerformanceMode::ALL {
            mode_row = mode_row.child(performance_mode_button(mode, settings, theme, cx));
        }

        let mut delay_row = div().flex().flex_wrap().gap(px(8.));
        for minutes in SUSPEND_DELAY_OPTIONS {
            delay_row = delay_row.child(suspend_delay_button(minutes, settings, theme, cx));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                t!("Performance"),
                t!("Reduce animation and polling work without turning off notifications, downloads, calls, the app lock, or security checks."),
                theme,
            ))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Battery saver mode"),
                    )
                    .child(mode_row)
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(settings.performance.mode.description()),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(
                                "Automatic reads the power source from /sys/class/power_supply and stays in normal mode when no battery can be read. Changes apply immediately; no restart is needed.",
                            ),
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(if saver_active {
                                theme.accent
                            } else {
                                theme.text_muted
                            })
                            .child(performance.status_text()),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "Power source: {}. {} mode.",
                                performance.power.label(),
                                if saver_active { "Battery saver" } else { "Normal" }
                            )),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("What battery saver changes"),
                    )
                    .child(performance_change_line(
                        "Pauses WhatsApp Web animation and CSS transitions.",
                        theme,
                    ))
                    .child(performance_change_line(
                        "Checks the connection and QR login state every 5 s instead of 2 s; online/offline events still report immediately.",
                        theme,
                    ))
                    .child(performance_change_line(
                        "Synchronizes the native WebKit overlay 4 times per second instead of 10.",
                        theme,
                    ))
                    .child(performance_change_line(
                        "When low-memory mode is already on, releases hidden accounts after at most 15 minutes and sweeps every 30 s. It never enables low-memory mode on its own.",
                        theme,
                    ))
                    .child(performance_change_line(
                        "Keeps downloads, calls and recordings, notifications, the app lock, and the WhatsApp login working.",
                        theme,
                    ))
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(
                                "These are qualitative cadence reductions, not benchmarked FPS or watt savings. WebKit does not expose per-view CPU or GPU use, and WhatsApp Web plus WebKit still dominate power consumption.",
                            ),
                    ),
            )
            .child(
                card(theme)
                    .child(toggle_row(
                        "low-memory-mode",
                        t!("Low-memory mode"),
                        t!("Destroy the WebKit view of an account that stayed hidden for the delay below. The account and its WhatsApp session stay on disk."),
                        settings.memory.suspend_hidden,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.memory.suspend_hidden = !settings.memory.suspend_hidden;
                            });
                        }),
                    ))
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Release hidden accounts after")),
                    )
                    .child(delay_row)
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("Reactivating reloads WhatsApp Web, which takes a few seconds. The current scroll position or an unsent draft may be lost.")),
                    )
                    .child(divider(theme))
                    .child(toggle_row(
                        "keep-active-loaded",t!("Keep the active account loaded"),t!("Never release the account you opened most recently, even when every window is hidden."),
                        settings.memory.keep_active_account,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.memory.keep_active_account =
                                    !settings.memory.keep_active_account;
                            });
                        }),
                    )),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Status")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(28.))
                            .child(storage_stat(
                                t!("Active views"),
                                tr!(
                                    "{active} of {total}",
                                    active = active_views,
                                    total = total_accounts
                                ),
                                theme,
                            ))
                            .child(storage_stat(
                                t!("Suspended accounts"),
                                suspended.to_string(),
                                theme,
                            ))
                            .child(storage_stat(
                                t!("Estimated memory released"),
                                storage::format_bytes(estimate),
                                theme,
                            )),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(tr!(
                                "The estimate assumes about {size} per released WebKit web process. WebKit does not expose per-view memory, so this is not a measurement.",
                                size = storage::format_bytes(crate::app::ESTIMATED_SUSPENDED_MEMORY_BYTES)
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(button(
                                "release-hidden-accounts",t!("Release hidden accounts now"),
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    let released = this.model.update(cx, |model, cx| {
                                        model.release_hidden_accounts_now(cx)
                                    });
                                    this.memory_feedback = Some(if released == 0 {
                                        Notice::success(t!(
                                            "No hidden account could be released right now. Downloads, calls or recordings, the active account, the app lock, and quitting all block suspension.",
                                        ))
                                    } else {
                                        Notice::success(tn!(
                                            released,
                                            "Released {count} hidden account. Open the account to restore it.",
                                            "Released {count} hidden accounts. Open the account to restore it.",
                                        ))
                                    });
                                    cx.notify();
                                }),
                            )),
                    )
                    .when_some(feedback, |style, notice| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(if notice.error {
                                    theme.danger
                                } else {
                                    theme.text_muted
                                })
                                .child(notice.text),
                        )
                    }),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("What suspension keeps")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text_muted)
                            .child(t!("Suspending never deletes the account, its WebKit profile, cookies, local storage, or the WhatsApp login. It only destroys the running view, which lets WebKit terminate that account's web process. Downloads, calls, and recordings always block suspension, and nothing is released while the app is locked or quitting.")),
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
            t!("Require a password to unlock"),
            t!("Hide every account window behind a lock screen."),
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
                    "lock-on-launch",t!("Lock on launch"),t!("Ask for the password when OxideZap starts."),
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
                    "lock-on-hide",t!("Lock when hidden"),t!("Lock automatically when the window is hidden to the tray."),
                    settings.lock.lock_on_hide,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.lock.lock_on_hide = !settings.lock.lock_on_hide;
                        });
                    }),
                ))
                .child(divider(theme))
                .child(toggle_row(
                    "lock-on-suspend",t!("Lock on system suspend"),t!("Lock before sleeping and after waking, using the logind D-Bus signal when available."),
                    settings.lock.lock_on_suspend,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.lock.lock_on_suspend = !settings.lock.lock_on_suspend;
                        });
                    }),
                ))
                .child(divider(theme))
                .child(toggle_row(
                    "lock-on-screen-lock",t!("Lock when the desktop screen locks"),t!("Best effort: depends on the desktop exposing a ScreenSaver D-Bus interface."),
                    settings.lock.lock_on_screen_lock,
                    theme,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update_settings(cx, |settings| {
                            settings.lock.lock_on_screen_lock = !settings.lock.lock_on_screen_lock;
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
                                .text_size(theme.font_px(14.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(t!("Lock after idle time")),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(6.))
                                .child(idle_button(0, settings, theme, cx))
                                .child(idle_button(1, settings, theme, cx))
                                .child(idle_button(5, settings, theme, cx))
                                .child(idle_button(15, settings, theme, cx))
                                .child(idle_button(30, settings, theme, cx)),
                        ),
                );
        }

        let mut password_card = card(theme).child(
            div()
                .text_size(theme.font_px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(if has_password {
                    t!("Change password")
                } else {
                    t!("Set a password")
                }),
        );
        password_card = password_card
            .child(labeled_field(
                t!("Password"),
                Some(SharedString::from(tr!(
                    "At least {count} characters.",
                    count = MIN_PASSWORD_LENGTH
                ))),
                theme,
                password_field,
            ))
            .child(labeled_field(
                t!("Confirm password"),
                None,
                theme,
                confirm_field,
            ))
            .when_some(error, |style, error| {
                style.child(
                    div()
                        .text_size(theme.font_px(11.))
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
                            t!("Update password")
                        } else {
                            t!("Set password")
                        },
                        theme,
                        true,
                        cx.listener(|this, _event, _window, cx| {
                            let password = this.password_field.read(cx).content().to_string();
                            let confirm = this.confirm_field.read(cx).content().to_string();
                            if password != confirm {
                                this.security_error =
                                    Some(t!("The passwords do not match.").to_string());
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
                            t!("Remove password"),
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
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text)
                            .child(t!("Remove password protection?")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                "remove-password-yes",
                                t!("Remove"),
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
                                t!("Cancel"),
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
            .child(section_header(t!("Security"),t!("The app lock uses Argon2id. System suspend and screen-lock detection are best effort through D-Bus."),
                theme,
            ))
            .child(security_card)
            .child(password_card)
            .when(settings.lock.enabled && has_password, |style| {
                style.child(div().flex().gap(px(8.)).child(button(
                    "lock-now",t!("Lock now"),
                    theme,
                    false,
                    cx.listener(|this, _event, _window, cx| {
                        this.model.update(cx, |model, cx| model.lock(cx));
                    }),
                )))
            })
            .into_any_element()
    }

    fn render_about(
        &mut self,
        settings: &Settings,
        update: &UpdateState,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = if update.development {
            Some(t!("Development builds do not check for updates.").to_string())
        } else if !settings.automatic_update_checks {
            Some(t!("Automatic update checks are off.").to_string())
        } else if update.available.is_none() {
            match update.skipped_version.as_ref() {
                Some(skipped) if update.latest_version.as_deref() == Some(skipped.as_str()) => {
                    Some(tr!("Version {version} is skipped.", version = skipped))
                }
                Some(_) => None,
                None if update.latest_version.is_some() => {
                    Some(t!("You are running the latest release.").to_string())
                }
                None => None,
            }
        } else {
            None
        };

        let diagnostics_running = self.diagnostics_running;
        let diagnostics_feedback = self.diagnostics_feedback.clone();
        let diagnostics_preview = self.diagnostics_preview.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(t!("About OxideZap"),t!("A native Linux shell around the official WhatsApp Web experience."),
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
                                            .text_size(theme.font_px(17.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme.text)
                                            .child("OxideZap"),
                                    )
                                    .child(
                                        div()
                                            .text_size(theme.font_px(12.))
                                            .text_color(theme.text_muted)
                                            .child(tr!(
                                                "Version {version}",
                                                version = update.current_version
                                            )),
                                    )
                                    .when(update.development, |style| {
                                        style.child(
                                            div()
                                                .text_size(theme.font_px(11.))
                                                .text_color(theme.text_faint)
                                                .child(t!("Development build")),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_size(theme.font_px(11.))
                                            .text_color(theme.text_faint)
                                            .child(t!("Rust · GPUI · WebKitGTK")),
                                    ),
                            ),
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text_muted)
                            .child(t!("OxideZap is an unofficial, independent project. It is not affiliated with, endorsed by, or sponsored by WhatsApp or Meta. WhatsApp is a trademark of Meta Platforms, Inc.")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                "open-repo",t!("GitHub repository"),
                                theme,
                                false,
                                |_event, _window, _cx| {
                                    crate::links::open_external(
                                        "https://github.com/artur282/oxidezap",
                                    );
                                },
                            ))
                            .child(button(
                                "open-issues",t!("Report an issue"),
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
                            .text_size(theme.font_px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Diagnostics"),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text_muted)
                            .child(
                                "Preview, copy, or save a privacy-scrubbed report with versions, session details, settings states, and recent errors. It never includes chats, account names or ids, cookies, proxy credentials, the app-lock hash, or full home paths.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                "diagnostics-preview",
                                if diagnostics_running {
                                    "Generating…"
                                } else {
                                    "Preview report"
                                },
                                theme,
                                true,
                                cx.listener(|this, _event, _window, cx| {
                                    this.start_diagnostics_action(DiagnosticsAction::Preview, cx);
                                }),
                            ))
                            .child(button(
                                "diagnostics-copy",
                                "Copy to clipboard",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.start_diagnostics_action(DiagnosticsAction::Copy, cx);
                                }),
                            ))
                            .child(button(
                                "diagnostics-save",
                                "Save to file",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.start_diagnostics_action(DiagnosticsAction::Save, cx);
                                }),
                            )),
                    )
                    .when_some(diagnostics_feedback, |style, notice| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(if notice.error {
                                    theme.danger
                                } else {
                                    theme.text_muted
                                })
                                .child(notice.text),
                        )
                    })
                    .when_some(diagnostics_preview, |style, preview| {
                        style.child(
                            div()
                                .id("diagnostics-preview-text")
                                .max_h(px(280.))
                                .overflow_y_scroll()
                                .p(px(10.))
                                .rounded_md()
                                .bg(theme.surface_alt)
                                .border_1()
                                .border_color(theme.border)
                                .font_family("monospace")
                                .text_size(theme.font_px(10.))
                                .text_color(theme.text_muted)
                                .child(preview),
                        )
                    }),
            )
            .when_some(update.available.clone(), |style, info| {
                style.child(
                    card(theme)
                        .border_color(theme.accent)
                        .child(
                            div()
                                .text_size(theme.font_px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(tr!(
                                    "Version {version} is available",
                                    version = info.version
                                )),
                        )
                        .child(
                            div()
                                .text_size(theme.font_px(12.))
                                .text_color(theme.text_muted)
                                .child(t!("A newer release is published on GitHub. OxideZap does not download or install it automatically.")),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button(
                                    "update-view-release",t!("View release"),
                                    theme,
                                    true,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.model.update(cx, |model, cx| {
                                            model.open_update_release(cx);
                                        });
                                    }),
                                ))
                                .child(button(
                                    "update-skip-version",t!("Skip this version"),
                                    theme,
                                    false,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.model.update(cx, |model, cx| {
                                            model.skip_update(cx);
                                        });
                                    }),
                                )),
                        ),
                )
            })
            .child(
                card(theme)
                    .child(toggle_row(
                        "automatic-update-checks",t!("Check for updates automatically"),t!("Ask GitHub for the latest release at most once every 24 hours. The request contains no personal data."),
                        settings.automatic_update_checks,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update(cx, |model, cx| {
                                let enabled = !model.settings.automatic_update_checks;
                                model.set_automatic_update_checks(enabled, cx);
                            });
                        }),
                    ))
                    .when_some(status, |style, status| {
                        style.child(
                            div()
                                .text_size(theme.font_px(11.))
                                .text_color(theme.text_faint)
                                .child(status),
                        )
                    }),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(t!("Built with")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(12.))
                            .text_color(theme.text)
                            .child(t!("GPUI for the native shell · WebKitGTK for WhatsApp Web · Rust for everything in between.")),
                    )
                    .child(
                        div()
                            .text_size(theme.font_px(11.))
                            .text_color(theme.text_faint)
                            .child(t!("MIT licensed. The source code is available on GitHub.")),
                    ),
            )
            .into_any_element()
    }
}

fn writing_card(
    settings: &Settings,
    theme: Theme,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let mut language_row = div().flex().flex_wrap().gap(px(8.));
    for language in [
        SpellCheckLanguage::Automatic,
        SpellCheckLanguage::Spanish,
        SpellCheckLanguage::English,
        SpellCheckLanguage::Disabled,
    ] {
        let selected = settings.writing.language == language;
        let label = if selected {
            format!("✓ {}", language.label())
        } else {
            language.label().to_string()
        };
        let writing = settings.writing.clone();
        language_row = language_row.child(button(
            SharedString::from(format!("spell-check-language-{language:?}").to_lowercase()),
            label,
            theme,
            selected,
            cx.listener(move |this, _event, _window, cx| {
                let mut writing = writing.clone();
                writing.set_language(language);
                this.model
                    .update(cx, |model, cx| model.set_writing(writing, cx));
            }),
        ));
    }

    let writing = settings.writing.clone();
    card(theme)
        .child(
            div()
                .text_size(theme.font_px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(t!("Writing")),
        )
        .child(
            div()
                .text_size(theme.font_px(13.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(t!("Spell check language")),
        )
        .child(language_row)
        .child(toggle_row(
            "spell-check-enabled",t!("Correct while I type"),t!("Use WebKitGTK's local spell checker in the WhatsApp composer."),
            settings.writing.spell_check,
            theme,
            cx.listener(move |this, _event, _window, cx| {
                let mut writing = writing.clone();
                writing.set_enabled(!writing.spell_check);
                this.model
                    .update(cx, |model, cx| model.set_writing(writing, cx));
            }),
        ))
        .child(
            div()
                .text_size(theme.font_px(11.))
                .text_color(theme.text_faint)
                .child(t!("Automatic follows the desktop locale. WebKitGTK only checks languages whose hunspell dictionaries are installed. Changes apply to open accounts; WhatsApp Web still controls its own composer and Enter-to-send behavior.")),
        )
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle);
        }
        window.set_window_title(t!("OxideZap Settings"));

        // The theme is resolved from the window appearance on every render, so a
        // repaint is enough when the system appearance changes. Register once per
        // view; a recreated window gets a new view and a new subscription.
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }

        let (theme, settings, update) = {
            let model = self.model.read(cx);
            (
                model.theme(window),
                model.settings.clone(),
                model.update_state.clone(),
            )
        };
        window.set_rem_size(theme.rem_size());

        let content = match self.section {
            SettingsSection::Accounts => self.render_accounts(&settings, theme, cx),
            SettingsSection::Storage => self.render_storage(&settings, theme, cx),
            SettingsSection::Appearance => self.render_appearance(&settings, theme, cx),
            SettingsSection::Accessibility => self.render_accessibility(&settings, theme, cx),
            SettingsSection::Notifications => self.render_notifications(&settings, theme, cx),
            SettingsSection::Network => self.render_network(&settings, theme, cx),
            SettingsSection::General => self.render_general(&settings, theme, cx),
            SettingsSection::Performance => self.render_performance(&settings, theme, cx),
            SettingsSection::Security => self.render_security(&settings, theme, cx),
            SettingsSection::About => self.render_about(&settings, &update, theme, cx),
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
                                    .text_size(theme.font_px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child("OxideZap"),
                            )
                            .child(
                                div()
                                    .text_size(theme.font_px(10.))
                                    .text_color(theme.text_faint)
                                    .child(t!("Settings")),
                            ),
                    ),
            )
            .child(div().h(px(10.)));

        for section in [
            SettingsSection::Accounts,
            SettingsSection::Storage,
            SettingsSection::Appearance,
            SettingsSection::Accessibility,
            SettingsSection::Notifications,
            SettingsSection::Network,
            SettingsSection::General,
            SettingsSection::Performance,
            SettingsSection::Security,
            SettingsSection::About,
        ] {
            let active = self.section == section;
            let id = SharedString::from(format!("settings-nav-{section:?}"));
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
                .text_size(theme.font_px(10.))
                .text_color(theme.text_faint)
                .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
        );

        div()
            .flex()
            .size_full()
            .bg(theme.bg)
            .track_focus(&self.focus_handle)
            .on_key_down(handle_focus_traversal)
            .on_key_down(cx.listener(Self::handle_escape))
            .child(sidebar)
            .child(
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
                        t!("Close"),
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

fn storage_stat(label: &'static str, value: String, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .text_size(theme.font_px(11.))
                .text_color(theme.text_faint)
                .child(label),
        )
        .child(
            div()
                .text_size(theme.font_px(16.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(value),
        )
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
        .text_size(theme.font_px(15.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(initial.to_uppercase())
}

fn performance_mode_button(
    mode: PerformanceMode,
    settings: &Settings,
    theme: Theme,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let selected = settings.performance.mode == mode;
    let text = if selected {
        format!("✓ {}", mode.label())
    } else {
        mode.label().to_string()
    };
    button(
        SharedString::from(format!("performance-mode-{}", mode.slug())),
        text,
        theme,
        selected,
        cx.listener(move |this, _event, _window, cx| {
            this.model.update_settings(cx, |settings| {
                settings.performance.mode = mode;
            });
        }),
    )
}

fn performance_change_line(text: &'static str, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.))
        .text_size(theme.font_px(12.))
        .text_color(theme.text_muted)
        .child("•")
        .child(text)
}

fn suspend_delay_button(
    minutes: u32,
    settings: &Settings,
    theme: Theme,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let selected = settings.memory.suspend_after_minutes == minutes;
    let text = if selected {
        format!("✓ {minutes} min")
    } else {
        format!("{minutes} min")
    };
    button(
        SharedString::from(format!("suspend-after-{minutes}")),
        text,
        theme,
        selected,
        cx.listener(move |this, _event, _window, cx| {
            this.model.update_settings(cx, |settings| {
                settings.memory.suspend_after_minutes = minutes;
            });
        }),
    )
}

fn idle_button(
    minutes: u32,
    settings: &Settings,
    theme: Theme,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let selected = settings.lock.idle_minutes == minutes;
    let label = if minutes == 0 {
        t!("Off").to_string()
    } else {
        format!("{minutes} min")
    };
    let text = if selected {
        format!("✓ {label}")
    } else {
        label
    };
    button(
        SharedString::from(format!("idle-{minutes}")),
        text,
        theme,
        selected,
        cx.listener(move |this, _event, _window, cx| {
            this.model.update_settings(cx, |settings| {
                settings.lock.idle_minutes = minutes;
            });
        }),
    )
}

fn css_size_label(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

const CSS_PREVIEW_CHARS: usize = 1200;

fn css_preview(css: &str) -> String {
    let css = css.trim();
    if css.is_empty() {
        return t!("Choose a local .css file to load a stylesheet.").to_string();
    }
    let mut chars = css.chars();
    let mut preview: String = chars.by_ref().take(CSS_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push_str("\n…");
    }
    preview
}

/// Reads and validates a stylesheet chosen by the user. The metadata check
/// rejects an oversized file before allocating its contents; validation runs
/// before the CSS reaches settings or WebKit.
fn read_custom_css_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        tr!(
            "Could not inspect {path}: {error}",
            path = path.display(),
            error = error
        )
    })?;
    if metadata.len() > MAX_CUSTOM_CSS_BYTES as u64 {
        return Err(tr!(
            "The CSS file is too large ({bytes} bytes; the limit is {limit} KiB).",
            bytes = metadata.len(),
            limit = MAX_CUSTOM_CSS_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        tr!(
            "Could not read {path}: {error}",
            path = path.display(),
            error = error
        )
    })?;
    let css = String::from_utf8(bytes)
        .map_err(|_| tr!("{path} is not valid UTF-8.", path = path.display()))?;
    validate_custom_css(&css)?;
    Ok(css)
}

/// Schedules a native GTK file chooser on the GTK/WebKit thread. This function
/// is called from the GPUI thread and never blocks it: the dialog runs on the
/// GTK main context, and the caller polls `result` from a GPUI task.
fn request_file_chooser(
    mode: FileChooserMode,
    suggested_name: String,
    result: Sender<Option<PathBuf>>,
) {
    glib::MainContext::default().invoke(move || {
        let save = matches!(mode, FileChooserMode::Save | FileChooserMode::Diagnostics);
        let folder = mode == FileChooserMode::Folder;
        let (title, action, accept) = if mode == FileChooserMode::Diagnostics {
            (
                "Save OxideZap diagnostics",
                gtk::FileChooserAction::Save,
                "Save",
            )
        } else if mode == FileChooserMode::Save {
            (
                t!("Export OxideZap settings"),
                gtk::FileChooserAction::Save,
                t!("Export"),
            )
        } else if folder {
            (
                t!("Choose the OxideZap download folder"),
                gtk::FileChooserAction::SelectFolder,
                t!("Select"),
            )
        } else if mode == FileChooserMode::Css {
            (
                t!("Load experimental WhatsApp CSS"),
                gtk::FileChooserAction::Open,
                t!("Load"),
            )
        } else {
            (
                t!("Import OxideZap settings"),
                gtk::FileChooserAction::Open,
                t!("Import"),
            )
        };
        let dialog = gtk::FileChooserNative::new(
            Some(title),
            None::<&gtk::Window>,
            action,
            Some(accept),
            Some(t!("Cancel")),
        );
        dialog.set_modal(true);
        if save {
            dialog.set_current_name(&suggested_name);
        }
        if !folder {
            let filter = gtk::FileFilter::new();
            if mode == FileChooserMode::Css {
                filter.set_name(Some(t!("CSS files (*.css)")));
                filter.add_pattern("*.css");
            } else if mode == FileChooserMode::Diagnostics {
                filter.set_name(Some(t!("Text files (*.txt)")));
                filter.add_pattern("*.txt");
            } else {
                filter.set_name(Some(t!("OxideZap settings (*.json)")));
                filter.add_pattern("*.json");
            }
            dialog.add_filter(filter);
        }

        let response = dialog.run();
        let chosen = (response == gtk::ResponseType::Accept)
            .then(|| dialog.filename())
            .flatten();
        dialog.destroy();
        let _ = result.send(chosen);
    });
}

/// Waits for the native chooser without blocking the GPUI thread.
async fn wait_for_file_choice(receiver: Receiver<Option<PathBuf>>) -> Option<PathBuf> {
    loop {
        match receiver.try_recv() {
            Ok(path) => return path,
            Err(TryRecvError::Empty) => {
                let _ = Timer::after(Duration::from_millis(50)).await;
            }
            Err(TryRecvError::Disconnected) => return None,
        }
    }
}
