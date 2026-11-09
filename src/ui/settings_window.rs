use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    SharedString, StatefulInteractiveElement, Subscription, Timer, Window, div, prelude::*, px,
};

use crate::app::downloads::{DOWNLOAD_HISTORY_LIMIT, valid_base_dir};
use crate::app::{AppModel, AppModelEntityExt};
use crate::assets::{ICON_BELL, ICON_GEAR, ICON_INFO, ICON_PALETTE, ICON_SHIELD, ICON_USER};
use crate::lock::MIN_PASSWORD_LENGTH;
use crate::settings::storage::{self, ConfirmationLevel, StorageOperation};
use crate::settings::{
    ImportPreview, MAX_ZOOM, MIN_ZOOM, ManualProxy, ProxyMode, ProxyProtocol, ProxySettings,
    Settings, SettingsExport, SpellCheckLanguage, ThemeMode, export_file_name, json_path,
    test_proxy_connection, write_settings_backup,
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
    Notifications,
    Network,
    General,
    Security,
    About,
}

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            SettingsSection::Accounts => "Accounts",
            SettingsSection::Storage => "Storage",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Notifications => "Notifications",
            SettingsSection::Network => "Network",
            SettingsSection::General => "General",
            SettingsSection::Security => "Security",
            SettingsSection::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            SettingsSection::Accounts => ICON_USER,
            SettingsSection::Storage => ICON_STORAGE,
            SettingsSection::Appearance => ICON_PALETTE,
            SettingsSection::Notifications => ICON_BELL,
            SettingsSection::Network => ICON_INFO,
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
    boss_key_field: Entity<TextField>,
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
    security_error: Option<String>,
    shortcut_error: Option<String>,
    boss_key_error: Option<String>,
    remove_password_confirm: bool,
    pending_import: Option<ImportPreview>,
    reset_confirm: bool,
    storage_confirm: Option<StorageConfirm>,
    storage_confirm_error: Option<String>,
    feedback: Option<Notice>,
    file_dialog_open: bool,
    operation_running: bool,
    appearance_subscription: Option<Subscription>,
}

impl SettingsWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        tracing::info!("SettingsWindow::new");
        let password_field = cx.new(|cx| TextField::new("New password", cx).masked(true));
        let confirm_field = cx.new(|cx| TextField::new("Confirm password", cx).masked(true));
        let shortcut = model.read(cx).settings.shortcut.clone();
        let boss_key = model.read(cx).settings.boss_key.clone();
        let shortcut_field = cx.new(|cx| TextField::new("Ctrl+Shift+W", cx).with_content(shortcut));
        let boss_key_field = cx.new(|cx| TextField::new("Ctrl+Shift+H", cx).with_content(boss_key));
        let downloads_dir = model
            .read(cx)
            .settings
            .downloads_directory
            .clone()
            .unwrap_or_else(|| model.read(cx).paths.downloads_dir.display().to_string());
        let downloads_dir_field =
            cx.new(|cx| TextField::new("Download folder", cx).with_content(downloads_dir));
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
            TextField::new("Optional username", cx).with_content(proxy.manual.username.clone())
        });
        let proxy_password_field = cx.new(|cx| {
            TextField::new("Optional password", cx)
                .masked(true)
                .with_content(proxy.manual.password.clone())
        });
        register_field_tab_stop(&password_field, cx);
        register_field_tab_stop(&confirm_field, cx);
        register_field_tab_stop(&shortcut_field, cx);
        register_field_tab_stop(&boss_key_field, cx);
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
            remove_confirm: None,
            password_field,
            confirm_field,
            shortcut_field,
            boss_key_field,
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
            security_error: None,
            shortcut_error: None,
            boss_key_error: None,
            remove_password_confirm: false,
            pending_import: None,
            reset_confirm: false,
            storage_confirm: None,
            storage_confirm_error: None,
            feedback: None,
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
            || self.remove_password_confirm
            || self.pending_import.is_some()
            || self.reset_confirm
            || self.storage_confirm.is_some();
        if !pending {
            return false;
        }
        self.renaming = None;
        self.remove_confirm = None;
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
                                        register_field_tab_stop(&field, cx);
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
                let field = cx.new(|cx| TextField::new("Account name", cx));
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
                    "Scanning…".to_string()
                } else {
                    "Not scanned yet".to_string()
                }
            });
        let free = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.free_bytes)
            .map(storage::format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let last_cleanup = settings
            .last_storage_cleanup
            .clone()
            .unwrap_or_else(|| "Never".to_string());

        let mut summary = card(theme)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(28.))
                    .child(storage_stat("Total account data", total, theme))
                    .child(storage_stat("Free disk space", free, theme))
                    .child(storage_stat("Last cleanup", last_cleanup, theme)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button(
                        "storage-refresh",
                        if scanning { "Scanning…" } else { "Refresh" },
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update(cx, |model, cx| model.refresh_storage(cx));
                            cx.notify();
                        }),
                    ))
                    .child(button(
                        "storage-open-folder",
                        "Open data folder",
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
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(format!("Data folder: {}", data_dir.display())),
            );
        if let Some(notice) = notice {
            summary = summary.child(
                div()
                    .text_size(px(11.))
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
                    let partial = if info.complete() { "" } else { " · partial" };
                    format!(
                        "Data {} · Cache {} · Total {}{partial}",
                        storage::format_bytes(info.data.bytes),
                        storage::format_bytes(info.cache.bytes),
                        storage::format_bytes(info.total_bytes())
                    )
                })
                .unwrap_or_else(|| {
                    if scanning {
                        "Scanning…".to_string()
                    } else {
                        "Not scanned yet".to_string()
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
                                    .text_size(px(14.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.text_muted)
                                    .child(info_line),
                            ),
                    )
                    .when(open, |style| {
                        style.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.text_faint)
                                .child("Window open"),
                        )
                    }),
            );

            if working {
                entry = entry.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child("Running storage operation…"),
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
                            "Clear cache",
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
                            "Sign out",
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
                            "Delete account data",
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
            .child(section_header(
                "Storage",
                "Inspect and clean the WebKit profile behind each account.",
                theme,
            ))
            .child(summary)
            .child(accounts)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(
                        "Clearing data closes and recreates the account's webview. Clearing the cache keeps the WhatsApp session; signing out requires the QR code again. Sizes are approximate while WhatsApp Web is running and stop at a bounded scan limit.",
                    )
                    .child(
                        "Deleting account data requires typing the account name and removes the account from OxideZap. The last account cannot be deleted.",
                    ),
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
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child(format!("Sign out of \"{name}\"?")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(
                                "The account and its settings are kept, but its WebKit data is deleted. WhatsApp Web will ask for the QR code again.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button(
                                SharedString::from(format!("storage-sign-out-yes-{confirm_id}")),
                                "Sign out",
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
                                SharedString::from(format!("storage-sign-out-no-{cancel_id}")),
                                "Cancel",
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
                            .text_size(px(12.))
                            .text_color(theme.text)
                            .child(format!("Delete all data for \"{name}\"?")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(
                                "This closes the account, deletes its WebKit profile, and removes it from OxideZap. This cannot be undone.",
                            ),
                    )
                    .child(labeled_field(
                        "Type the account name to confirm",
                        None,
                        theme,
                        field.clone(),
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
                                SharedString::from(format!("storage-delete-yes-{delete_id}")),
                                "Delete account data",
                                theme,
                                true,
                                cx.listener(move |this, _event, _window, cx| {
                                    let typed = field.read(cx).content().to_string();
                                    if !storage::typed_confirmation_matches(&typed, &expected) {
                                        this.storage_confirm_error = Some(
                                            "Type the account name exactly to confirm."
                                                .to_string(),
                                        );
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
                                SharedString::from(format!("storage-delete-no-{cancel_id}")),
                                "Cancel",
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
            let id = SharedString::from(format!("theme-{}", mode.label().to_lowercase()));
            let description = match mode {
                ThemeMode::System => "Follow the desktop",
                ThemeMode::Light => "Bright surfaces",
                ThemeMode::Dark => "Low-light surfaces",
            };
            let description = if selected {
                format!("Selected · {description}")
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
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(mode.label()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .child(description),
                    ),
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
            let text = if selected {
                format!("✓ {label}")
            } else {
                label.to_string()
            };
            zoom_row = zoom_row.child(button(
                SharedString::from(format!("zoom-{label}")),
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
                    .child(theme_cards)
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(
                                "System follows the desktop color scheme live. On Linux this depends on the XDG desktop portal and the desktop settings service, so some window managers may not report changes.",
                            ),
                    ),
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
                "Muted"
            } else if sound {
                "Notifications and sound"
            } else {
                "Notifications, silent"
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
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.text_faint)
                                .child(state),
                        ),
                )
                .child(toggle_row(
                    SharedString::from(format!("mute-{account_id}")),
                    "Mute notifications",
                    "Suppress native message notifications from this account.",
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
                    "Notification sound",
                    "Ask the desktop notification service to play a message sound.",
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
                card(theme)
                    .child(toggle_row(
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
                    ))
                    .child(divider(theme))
                    .child(toggle_row(
                        "connection-notifications",
                        "Connection notifications",
                        "Show a native notification when an account goes offline or reconnects.",
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
                        "do-not-disturb",
                        "Do not disturb",
                        "Pause message notifications and sounds from every account. Unread counts keep updating.",
                        settings.do_not_disturb,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.set_do_not_disturb(!settings.do_not_disturb);
                            });
                        }),
                    )),
            )
            .child(writing_card(settings, theme, cx))
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Per-account"),
                    )
                    .child(per_account),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child("Muted accounts and Do not disturb have priority over per-account sounds.")
                    .child("Connection notifications also follow Do not disturb and per-account mute.")
                    .child("Notifications are also suppressed while the app is locked.")
                    .child("Sound playback depends on the notification service and its sound theme; some servers ignore the requested sound or always use their own."),
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
                        anyhow::anyhow!("could not write {}: {error}", path.display())
                    })?;
                    Ok::<PathBuf, anyhow::Error>(path)
                })
                .await;
            this.update(cx, |this, cx| {
                this.file_dialog_open = false;
                this.feedback = Some(match result {
                    Ok(path) => {
                        Notice::success(format!("Settings exported to {}.", path.display()))
                    }
                    Err(error) => Notice::error(format!("Could not export settings: {error}")),
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
                        this.feedback =
                            Some(Notice::error(format!("Could not import settings: {error}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                Some("Enter an absolute folder path, or use the default.".to_string());
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
                        this.feedback = Some(Notice::success(format!(
                            "Settings imported. The previous settings were backed up to {}.",
                            path.display()
                        )));
                    }
                    Err(error) => {
                        this.pending_import = Some(preview);
                        this.feedback = Some(Notice::error(format!(
                            "Could not back up the current settings, so nothing was imported: {error}"
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
                        this.feedback = Some(Notice::success(format!(
                            "Preferences reset to their defaults. Your accounts were kept and a backup was written to {}.",
                            path.display()
                        )));
                    }
                    Err(error) => {
                        this.feedback = Some(Notice::error(format!(
                            "Could not back up the current settings, so nothing was reset: {error}"
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
                        "Export settings\u{2026}",
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_export(cx);
                        }),
                    ))
                    .child(button(
                        "import-settings",
                        "Import settings\u{2026}",
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_import(cx);
                        }),
                    ))
                    .child(button(
                        "reset-settings",
                        "Reset settings",
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
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child("Waiting for the file dialog or the current operation\u{2026}"),
                )
            });

        let mut card = card(theme)
            .child(
                div()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child("Backup and restore"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .child(
                        "Export the configuration to a readable JSON file, import it on another installation, or restore the defaults.",
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(
                        "Exports never include account sessions, cookies, chats, or the app-lock password. Every import and reset writes a dated backup first.",
                    ),
            )
            .child(actions);

        if let Some(notice) = feedback {
            card = card.child(
                div()
                    .text_size(px(11.))
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
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child(change.clone()),
                );
            }
            card = card
                .child(divider(theme))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Import summary"),
                )
                .child(summary)
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_faint)
                        .child(
                            "Accounts from the file replace the current account list; local session data is not deleted. The app-lock password is not imported, so set it again afterwards.",
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button(
                            "import-apply",
                            "Apply import",
                            theme,
                            true,
                            cx.listener(|this, _event, _window, cx| {
                                this.apply_pending_import(cx);
                            }),
                        ))
                        .child(button(
                            "import-cancel",
                            "Cancel",
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
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child("Reset preferences to their defaults?"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_faint)
                        .child(
                            "Theme, zoom, notifications, startup, the global shortcut, and the app-lock password are reset. Accounts and their WhatsApp sessions are kept. A dated backup is written first.",
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button(
                            "reset-apply",
                            "Reset settings",
                            theme,
                            true,
                            cx.listener(|this, _event, _window, cx| {
                                this.reset_settings(cx);
                            }),
                        ))
                        .child(button(
                            "reset-cancel",
                            "Cancel",
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
                return Err("Enter the proxy port.".to_string());
            }
            port_text
                .parse::<u16>()
                .map_err(|_| "The proxy port must be a number between 1 and 65535.".to_string())?
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
                self.proxy_feedback = Some(Notice::success(
                    "Proxy settings saved. Reopen an account if the new route does not take effect.",
                ));
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
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child("Connection mode"),
            )
            .child(mode_row)
            .child(
                div()
                    .text_size(px(11.))
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
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted)
                        .child("Proxy protocol"),
                )
                .child(protocol_row)
                .child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().flex_1().child(labeled_field(
                            "Host",
                            None,
                            theme,
                            host_field,
                        )))
                        .child(div().w(px(120.)).child(labeled_field(
                            "Port",
                            None,
                            theme,
                            port_field,
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.))
                        .child(div().flex_1().child(labeled_field(
                            "Username",
                            Some(SharedString::from("Optional")),
                            theme,
                            username_field,
                        )))
                        .child(div().flex_1().child(labeled_field(
                            "Password",
                            Some(SharedString::from("Optional, stored as plain text")),
                            theme,
                            password_field,
                        ))),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.danger)
                        .child(
                            "Proxy credentials are stored as plain text in settings.json. The file is private (0600) and credentials are never logged or written to exports, but this build does not use a system keyring. Prefer a proxy that does not require a password.",
                        ),
                );
        }

        let running = self.proxy_test_running;
        mode_card = mode_card
            .child(divider(theme))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(
                        "The proxy is applied to each account's WebKit data manager when the account opens. Existing connections keep their route, so reopen an account if the change does not take effect.",
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child("Test connection checks web.whatsapp.com through the configured route without blocking the interface. HTTP and SOCKS5 proxies are tested end to end; an HTTPS proxy is checked for reachability before the TLS handshake."),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(format!(
                        "Saved configuration: {}",
                        settings.network.proxy.redacted_summary()
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(button(
                        "proxy-test-connection",
                        if running {
                            "Testing\u{2026}"
                        } else {
                            "Test connection"
                        },
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.start_proxy_test(cx);
                        }),
                    ))
                    .child(button(
                        "proxy-apply",
                        "Apply proxy settings",
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
                    .text_size(px(11.))
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
            .child(section_header(
                "Network",
                "Choose how WhatsApp Web reaches the network. Changes apply per account and may require reopening an account.",
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
        let error = self.shortcut_error.clone();
        let boss_error = self.boss_key_error.clone();
        let downloads_dir_field = self.downloads_dir_field.clone();
        let downloads_error = self.downloads_error.clone();
        let backup_card = self.backup_card(theme, cx);

        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(section_header(
                "General",
                "Window behavior, startup, writing, and the global show/hide shortcut.",
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
                    )
                    .child(divider(theme))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Boss key"),
                    )
                    .child(labeled_field(
                        "Hide or restore every window",
                        Some(SharedString::from(
                            "A second global shortcut for instant privacy. A second press restores the windows that were visible.",
                        )),
                        theme,
                        boss_key_field,
                    ))
                    .when_some(boss_error, |style, error| {
                        style.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.danger)
                                .child(error),
                        )
                    })
                    .child(div().flex().gap(px(8.)).child(button(
                        "apply-boss-key",
                        "Apply boss key",
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
                                    this.boss_key_error = Some(error.to_string());
                                    cx.notify();
                                }
                            }
                        }),
                    )))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child("The boss key uses the same X11/XWayland grab and can conflict with desktop shortcuts."),
                    ),
            )
            .child(
                card(theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child("Downloads"),
                    )
                    .child(labeled_field(
                        "Download folder",
                        Some(SharedString::from(
                            "Leave the default to follow the desktop Downloads directory.",
                        )),
                        theme,
                        downloads_dir_field,
                    ))
                    .when_some(downloads_error, |style, error| {
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
                            .flex_wrap()
                            .gap(px(8.))
                            .child(button(
                                "choose-downloads-directory",
                                "Choose…",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.start_choose_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "apply-downloads-directory",
                                "Apply folder",
                                theme,
                                true,
                                cx.listener(|this, _event, _window, cx| {
                                    this.apply_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "default-downloads-directory",
                                "Use default",
                                theme,
                                false,
                                cx.listener(|this, _event, _window, cx| {
                                    this.use_default_downloads_directory(cx);
                                }),
                            ))
                            .child(button(
                                "open-download-center",
                                "Open Download Center",
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
                        "downloads-account-folders",
                        "Organize by account",
                        "Save each account's files in its own subfolder.",
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
                        "downloads-date-folders",
                        "Organize by month",
                        "Add a YYYY-MM subfolder (UTC) for the month each file was downloaded.",
                        settings.downloads_date_folders,
                        theme,
                        cx.listener(|this, _event, _window, cx| {
                            this.model.update_settings(cx, |settings| {
                                settings.downloads_date_folders =
                                    !settings.downloads_date_folders;
                            });
                        }),
                    ))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_faint)
                            .child(format!(
                                "The download history keeps the last {DOWNLOAD_HISTORY_LIMIT} entries. Existing files are never overwritten; collisions get a numbered suffix."
                            )),
                    ),
            )
            .child(backup_card)
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
                .child(toggle_row(
                    "lock-on-suspend",
                    "Lock on system suspend",
                    "Lock before sleeping and after waking, using the logind D-Bus signal when available.",
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
                    "lock-on-screen-lock",
                    "Lock when the desktop screen locks",
                    "Best effort: depends on the desktop exposing a ScreenSaver D-Bus interface.",
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
                                .child(idle_button(1, settings, theme, cx))
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
                "The app lock uses Argon2id. System suspend and screen-lock detection are best effort through D-Bus.",
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

    fn render_about(
        &mut self,
        settings: &Settings,
        update: &UpdateState,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let status = if update.development {
            Some("Development builds do not check for updates.".to_string())
        } else if !settings.automatic_update_checks {
            Some("Automatic update checks are off.".to_string())
        } else if update.available.is_none() {
            match update.skipped_version.as_ref() {
                Some(skipped) if update.latest_version.as_deref() == Some(skipped.as_str()) => {
                    Some(format!("Version {skipped} is skipped."))
                }
                Some(_) => None,
                None if update.latest_version.is_some() => {
                    Some("You are running the latest release.".to_string())
                }
                None => None,
            }
        } else {
            None
        };

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
                                            .child(format!("Version {}", update.current_version)),
                                    )
                                    .when(update.development, |style| {
                                        style.child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(theme.text_faint)
                                                .child("Development build"),
                                        )
                                    })
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
            .when_some(update.available.clone(), |style, info| {
                style.child(
                    card(theme)
                        .border_color(theme.accent)
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(format!("Version {} is available", info.version)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text_muted)
                                .child(
                                    "A newer release is published on GitHub. OxideZap does not download or install it automatically.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button(
                                    "update-view-release",
                                    "View release",
                                    theme,
                                    true,
                                    cx.listener(|this, _event, _window, cx| {
                                        this.model.update(cx, |model, cx| {
                                            model.open_update_release(cx);
                                        });
                                    }),
                                ))
                                .child(button(
                                    "update-skip-version",
                                    "Skip this version",
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
                        "automatic-update-checks",
                        "Check for updates automatically",
                        "Ask GitHub for the latest release at most once every 24 hours. The request contains no personal data.",
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
                                .text_size(px(11.))
                                .text_color(theme.text_faint)
                                .child(status),
                        )
                    }),
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
            SharedString::from(format!(
                "spell-check-language-{}",
                language.label().to_lowercase()
            )),
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
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child("Writing"),
        )
        .child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child("Spell check language"),
        )
        .child(language_row)
        .child(toggle_row(
            "spell-check-enabled",
            "Correct while I type",
            "Use WebKitGTK's local spell checker in the WhatsApp composer.",
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
                .text_size(px(11.))
                .text_color(theme.text_faint)
                .child("Automatic follows the desktop locale. WebKitGTK only checks languages whose hunspell dictionaries are installed. Changes apply to open accounts; WhatsApp Web still controls its own composer and Enter-to-send behavior."),
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

        let content = match self.section {
            SettingsSection::Accounts => self.render_accounts(&settings, theme, cx),
            SettingsSection::Storage => self.render_storage(&settings, theme, cx),
            SettingsSection::Appearance => self.render_appearance(&settings, theme, cx),
            SettingsSection::Notifications => self.render_notifications(&settings, theme, cx),
            SettingsSection::Network => self.render_network(&settings, theme, cx),
            SettingsSection::General => self.render_general(&settings, theme, cx),
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
            SettingsSection::Storage,
            SettingsSection::Appearance,
            SettingsSection::Notifications,
            SettingsSection::Network,
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

fn storage_stat(label: &'static str, value: String, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.text_faint)
                .child(label),
        )
        .child(
            div()
                .text_size(px(16.))
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

/// Schedules a native GTK file chooser on the GTK/WebKit thread. This function
/// is called from the GPUI thread and never blocks it: the dialog runs on the
/// GTK main context, and the caller polls `result` from a GPUI task.
fn request_file_chooser(
    mode: FileChooserMode,
    suggested_name: String,
    result: Sender<Option<PathBuf>>,
) {
    glib::MainContext::default().invoke(move || {
        let save = mode == FileChooserMode::Save;
        let folder = mode == FileChooserMode::Folder;
        let (title, action, accept) = if save {
            (
                "Export OxideZap settings",
                gtk::FileChooserAction::Save,
                "Export",
            )
        } else if folder {
            (
                "Choose the OxideZap download folder",
                gtk::FileChooserAction::SelectFolder,
                "Select",
            )
        } else {
            (
                "Import OxideZap settings",
                gtk::FileChooserAction::Open,
                "Import",
            )
        };
        let dialog = gtk::FileChooserNative::new(
            Some(title),
            None::<&gtk::Window>,
            action,
            Some(accept),
            Some("Cancel"),
        );
        dialog.set_modal(true);
        if save {
            dialog.set_current_name(&suggested_name);
        }
        if !folder {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("OxideZap settings (*.json)"));
            filter.add_pattern("*.json");
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
