use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, FocusHandle, Focusable, IntoElement,
    KeyDownEvent, Render, Rgba, SharedString, Subscription, Window, div, prelude::*, px, relative,
    uniform_list,
};

use crate::app::AppModel;
use crate::app::downloads::{DownloadRecord, DownloadStatus, format_size};
use crate::settings::format_timestamp;
use crate::theme::Theme;
use crate::ui::components::{TAB_INDEX, app_icon, button, divider, handle_focus_traversal};

const FILE_COLUMN_MIN_WIDTH: f32 = 180.0;
const ACCOUNT_COLUMN_WIDTH: f32 = 96.0;
const SIZE_COLUMN_WIDTH: f32 = 88.0;
const PROGRESS_COLUMN_WIDTH: f32 = 138.0;
const STATUS_COLUMN_WIDTH: f32 = 88.0;
const DATE_COLUMN_WIDTH: f32 = 108.0;
const ACTIONS_COLUMN_WIDTH: f32 = 258.0;

pub struct DownloadsWindow {
    model: Entity<AppModel>,
    focus_handle: FocusHandle,
    appearance_subscription: Option<Subscription>,
}

impl DownloadsWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            focus_handle: cx.focus_handle(),
            appearance_subscription: None,
        }
    }
}

impl DownloadsWindow {
    fn handle_escape(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            window.remove_window();
        }
    }
}

impl Focusable for DownloadsWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DownloadsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle, cx);
        }
        window.set_window_title(t!("OxideZap Downloads"));
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }

        let (theme, records, base_dir, active) = {
            let model = self.model.read(cx);
            (
                model.theme(window),
                model.downloads().to_vec(),
                model.downloads_base_dir(),
                model.active_downloads(),
            )
        };

        let subtitle = if records.is_empty() {
            t!("Downloads appear here as they start.").to_string()
        } else if active == 0 {
            tn!(records.len() as u32, "{count} item", "{count} items")
        } else {
            tr!(
                "{count} items · {active} in progress",
                count = records.len(),
                active = active
            )
        };

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(11.))
                    .child(app_icon(28.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(t!("Downloads")),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(subtitle)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(button(
                        "downloads-open-folder",
                        t!("Open folder"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.model
                                .update(cx, |model, cx| model.open_downloads_folder(cx));
                        }),
                    ))
                    .child(button(
                        "downloads-close",
                        t!("Close"),
                        theme,
                        false,
                        cx.listener(|_this, _event, window, _cx| {
                            window.remove_window();
                        }),
                    )),
            );

        let folder_line = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .text_color(theme.text_faint)
            .child(t!("Saving to"))
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(base_dir.display().to_string())),
            );

        let list = if records.is_empty() {
            empty_state(theme, cx, self.model.clone()).into_any_element()
        } else {
            let model = self.model.clone();
            uniform_list(
                "downloads-list",
                records.len(),
                move |range, _window, _cx| {
                    range
                        .map(|index| download_row(records[index].clone(), theme, model.clone()))
                        .collect()
                },
            )
            .flex_1()
            .min_h(px(0.))
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg)
            .track_focus(&self.focus_handle)
            .on_key_down(handle_focus_traversal)
            .on_key_down(cx.listener(Self::handle_escape))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(20.))
                    .pb(px(14.))
                    .bg(theme.surface)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(header)
                    .child(folder_line),
            )
            .child(div().px(px(20.)).py(px(10.)).child(columns_header(theme)))
            .child(div().flex_1().min_h(px(0.)).px(px(20.)).child(list))
            .child(divider(theme))
            .child(
                div()
                    .px(px(20.))
                    .py(px(8.))
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(t!("Downloads are written straight to disk. In-progress rows can be cancelled; finished rows can be retried when WebKit exposed a source URL.")),
            )
    }
}

fn columns_header(theme: Theme) -> impl IntoElement {
    row_shell(theme)
        .h(px(24.))
        .border_b_1()
        .border_color(theme.border)
        .child(
            column_label(t!("File"), theme)
                .flex_1()
                .min_w(px(FILE_COLUMN_MIN_WIDTH)),
        )
        .child(column_label(t!("Account"), theme).w(px(ACCOUNT_COLUMN_WIDTH)))
        .child(column_label(t!("Size"), theme).w(px(SIZE_COLUMN_WIDTH)))
        .child(column_label(t!("Progress"), theme).w(px(PROGRESS_COLUMN_WIDTH)))
        .child(column_label(t!("Status"), theme).w(px(STATUS_COLUMN_WIDTH)))
        .child(column_label(t!("Started"), theme).w(px(DATE_COLUMN_WIDTH)))
        .child(column_label(t!("Actions"), theme).w(px(ACTIONS_COLUMN_WIDTH)))
}

fn download_row(record: DownloadRecord, theme: Theme, model: Entity<AppModel>) -> impl IntoElement {
    let status_color = status_color(record.status, theme);
    let progress = record.progress();
    let size = size_label(&record);
    let progress_label = progress_label(&record);
    let started = format_started_at(record.started_at);
    let filename = record.filename.clone();
    let account = record.account_name.clone();

    let file_cell = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .min_w(px(0.))
        .flex_1()
        .child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(SharedString::from(filename)),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(SharedString::from(
                    record
                        .path
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "—".to_string()),
                )),
        );

    let progress_cell = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .w(px(PROGRESS_COLUMN_WIDTH))
        .flex_shrink_0()
        .when(record.is_active(), |style| {
            style
                .child(
                    div()
                        .h(px(5.))
                        .w_full()
                        .rounded_full()
                        .overflow_hidden()
                        .bg(theme.border)
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(theme.accent)
                                .w(relative(progress.unwrap_or(0.0) as f32)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(progress_label.clone())),
                )
        })
        .when(!record.is_active(), |style| {
            style.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(progress_label)),
            )
        });

    let actions = row_actions(&record, theme, model);

    row_shell(theme)
        .h(px(54.))
        .child(file_cell)
        .child(
            div()
                .w(px(ACCOUNT_COLUMN_WIDTH))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(SharedString::from(account)),
        )
        .child(
            div()
                .w(px(SIZE_COLUMN_WIDTH))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .child(SharedString::from(size)),
        )
        .child(progress_cell)
        .child(
            div()
                .w(px(STATUS_COLUMN_WIDTH))
                .flex_shrink_0()
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(status_color)
                .child(crate::i18n::translate_dynamic(record.status.label())),
        )
        .child(
            div()
                .w(px(DATE_COLUMN_WIDTH))
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(SharedString::from(started)),
        )
        .child(
            div()
                .w(px(ACTIONS_COLUMN_WIDTH))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(actions),
        )
}

fn row_actions(record: &DownloadRecord, theme: Theme, model: Entity<AppModel>) -> impl IntoElement {
    let id = record.id.clone();
    let completed = record.status == DownloadStatus::Completed;
    let active = record.is_active();
    let can_retry = record.retry_url().is_some();
    let has_path = record.path.is_some();

    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .when(completed && has_path, |style| {
            let model = model.clone();
            let id = id.clone();
            style.child(row_action(
                format!("download-open-{id}"),
                t!("Open"),
                theme,
                true,
                move |_event, _window, cx| {
                    model.update(cx, |model, cx| model.open_download(&id, cx));
                },
            ))
        })
        .when(has_path, |style| {
            let model = model.clone();
            let id = id.clone();
            style.child(row_action(
                format!("download-folder-{id}"),
                t!("Folder"),
                theme,
                false,
                move |_event, _window, cx| {
                    model.update(cx, |model, cx| model.open_download_folder(&id, cx));
                },
            ))
        })
        .when(has_path, |style| {
            let path = record
                .path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            style.child(row_action(
                format!("download-copy-{id}"),
                t!("Copy"),
                theme,
                false,
                move |_event, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                },
            ))
        })
        .when(active, |style| {
            let model = model.clone();
            let id = id.clone();
            style.child(row_action(
                format!("download-cancel-{id}"),
                t!("Cancel"),
                theme,
                false,
                move |_event, _window, cx| {
                    model.update(cx, |model, cx| model.cancel_download(&id, cx));
                },
            ))
        })
        .when(!active && can_retry, |style| {
            let model = model.clone();
            let id = id.clone();
            style.child(row_action(
                format!("download-retry-{id}"),
                t!("Retry"),
                theme,
                false,
                move |_event, _window, cx| {
                    model.update(cx, |model, cx| model.retry_download(&id, cx));
                },
            ))
        })
        .when(!active, |style| {
            let model = model.clone();
            let id = id.clone();
            style.child(row_action(
                format!("download-remove-{id}"),
                t!("Remove"),
                theme,
                false,
                move |_event, _window, cx| {
                    model.update(cx, |model, cx| model.remove_download(&id, cx));
                },
            ))
        })
}

fn row_action(
    id: String,
    label: &'static str,
    theme: Theme,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let (background, hover, text, border) = if primary {
        (
            theme.accent,
            theme.accent_hover,
            theme.accent_text,
            theme.accent,
        )
    } else {
        (
            theme.surface_alt,
            theme.surface_hover,
            theme.text,
            theme.border,
        )
    };
    div()
        .id(gpui::ElementId::Name(SharedString::from(id)))
        .tab_index(TAB_INDEX)
        .flex()
        .items_center()
        .justify_center()
        .h(px(24.))
        .px(px(7.))
        .rounded_md()
        .bg(background)
        .border_1()
        .border_color(border)
        .text_color(text)
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .focus(move |style| style.border_2().border_color(theme.accent))
        .hover(move |style| style.bg(hover))
        .on_click(on_click)
        .child(label)
}

fn empty_state(
    theme: Theme,
    cx: &mut Context<DownloadsWindow>,
    model: Entity<AppModel>,
) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .child(app_icon(48.0))
        .child(
            div()
                .text_size(px(16.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(t!("No downloads yet")),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(t!("Files saved from WhatsApp Web show their progress and history here. Blob and view-once media never reach this list.")),
        )
        .child(button(
            "downloads-empty-open-folder",t!("Open download folder"),
            theme,
            false,
            cx.listener(move |_this, _event, _window, cx| {
                model.update(cx, |model, cx| model.open_downloads_folder(cx));
            }),
        ))
}

fn row_shell(theme: Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(4.))
        .border_b_1()
        .border_color(theme.border)
        .text_color(theme.text)
}

fn column_label(label: &'static str, theme: Theme) -> gpui::Div {
    div()
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_faint)
        .child(label)
}

fn status_color(status: DownloadStatus, theme: Theme) -> Rgba {
    match status {
        DownloadStatus::InProgress => theme.accent,
        DownloadStatus::Completed => theme.text,
        DownloadStatus::Failed => theme.danger,
        DownloadStatus::Cancelled => theme.text_muted,
    }
}

fn size_label(record: &DownloadRecord) -> String {
    match record.total_bytes {
        Some(total) => format_size(total),
        None if record.is_active() => format_size(record.received_bytes),
        None => "—".to_string(),
    }
}

fn progress_label(record: &DownloadRecord) -> String {
    if record.is_active() {
        match (record.progress(), record.total_bytes) {
            (Some(fraction), Some(total)) => tr!(
                "{percent}% · {received} of {total}",
                percent = (fraction * 100.0).round() as u32,
                received = format_size(record.received_bytes),
                total = format_size(total)
            ),
            _ => format_size(record.received_bytes),
        }
    } else if record.status == DownloadStatus::Completed {
        t!("Complete").to_string()
    } else {
        "—".to_string()
    }
}

fn format_started_at(millis: u64) -> String {
    let seconds = millis / 1000;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(seconds);
    if age < 60 {
        t!("Just now").to_string()
    } else if age < 3600 {
        tr!("{minutes} min ago", minutes = age / 60)
    } else if age < 86_400 {
        tr!("{hours} h ago", hours = age / 3600)
    } else {
        let stamp = format_timestamp(UNIX_EPOCH + Duration::from_secs(seconds));
        stamp.chars().take(16).collect::<String>().replace('T', " ")
    }
}
