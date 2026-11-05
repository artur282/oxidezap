use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, IntoElement, SharedString, Window, div,
    prelude::*, px, rgb, svg,
};

use crate::theme::Theme;

pub fn app_icon(size: f32) -> impl IntoElement {
    svg()
        .path("oxidezap-mono.svg")
        .size(px(size))
        .text_color(rgb(0x25d366))
}

pub fn icon(icon_path: &'static str, size: f32, color: impl Into<gpui::Rgba>) -> impl IntoElement {
    svg()
        .path(icon_path)
        .size(px(size))
        .text_color(color.into())
}

pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: Theme,
    primary: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = label.into();
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
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(34.))
        .px(px(14.))
        .rounded_md()
        .bg(background)
        .border_1()
        .border_color(border)
        .text_color(text)
        .text_size(px(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_click(on_click)
        .child(label)
}

pub fn titlebar_button(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    theme: Theme,
    danger: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let color = if danger {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(42.))
        .h_full()
        .cursor_pointer()
        .hover(move |style| {
            style.bg(if danger {
                theme.danger
            } else {
                theme.surface_hover
            })
        })
        .on_click(on_click)
        .child(icon(icon_path, 14.0, color))
}

pub fn section_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    theme: Theme,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(22.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(title),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.text_muted)
                .child(description),
        )
}

pub fn card(theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(16.))
        .rounded_lg()
        .bg(theme.surface)
        .border_1()
        .border_color(theme.border)
}

pub fn divider(theme: Theme) -> impl IntoElement {
    div().h(px(1.)).w_full().bg(theme.border)
}

pub fn toggle_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    value: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .py(px(10.))
        .cursor_pointer()
        .on_click(on_click)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(description),
                ),
        )
        .child(toggle(value, theme))
}

pub fn toggle(value: bool, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w(px(40.))
        .h(px(22.))
        .p(px(3.))
        .rounded_full()
        .bg(if value { theme.accent } else { theme.border })
        .when(value, |style| style.justify_end())
        .when(!value, |style| style.justify_start())
        .child(div().size(px(16.)).rounded_full().bg(rgb(0xffffff)))
}

pub fn sidebar_item(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = label.into();
    let color = if active {
        theme.accent
    } else {
        theme.text_muted
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(10.))
        .h(px(38.))
        .px(px(12.))
        .rounded_md()
        .cursor_pointer()
        .bg(if active {
            theme.surface_hover
        } else {
            theme.surface
        })
        .text_color(if active { theme.text } else { theme.text_muted })
        .text_size(px(13.))
        .hover(move |style| style.bg(theme.surface_hover))
        .on_click(on_click)
        .child(icon(icon_path, 16.0, color))
        .child(label)
}

pub fn unread_badge(count: u32, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(18.))
        .h(px(18.))
        .px(px(5.))
        .rounded_full()
        .bg(theme.accent)
        .text_color(theme.accent_text)
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(if count > 99 {
            "99+".to_string()
        } else {
            count.to_string()
        })
}

pub fn labeled_field(
    label: impl Into<SharedString>,
    description: Option<SharedString>,
    theme: Theme,
    field: impl IntoElement,
) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(label),
        )
        .when_some(description, |style, description| {
            style.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(description),
            )
        })
        .child(field)
}
