use gpui::{
    AnyView, App, ClickEvent, Div, ElementId, Entity, FontWeight, IntoElement, KeyDownEvent,
    Keystroke, Render, SharedString, StyleRefinement, Window, div, prelude::*, px, rgb, svg,
    transparent_black,
};

use crate::theme::Theme;
use crate::ui::text_input::TextField;

// GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so screen readers cannot inspect
// the native shell, and it provides no `focus_ring` API. Keyboard access is
// implemented with focus handles, tab stops and a theme-colored focus border.
// GPUI activates a focused element's click listener with Enter/Space itself and
// does not bind Tab, so every native window handles Tab and Shift+Tab at its
// root.
//
// All interactive elements use index 0 so Tab order follows paint (visual)
// order through the tab-stop insertion history.
pub const TAB_INDEX: isize = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    Next,
    Previous,
}

pub fn focus_direction(keystroke: &Keystroke) -> Option<FocusDirection> {
    let modifiers = keystroke.modifiers;
    if keystroke.key != "tab" || modifiers.control || modifiers.platform || modifiers.alt {
        return None;
    }
    Some(if modifiers.shift {
        FocusDirection::Previous
    } else {
        FocusDirection::Next
    })
}

pub fn handle_focus_traversal(event: &KeyDownEvent, window: &mut Window, _cx: &mut App) {
    match focus_direction(&event.keystroke) {
        Some(FocusDirection::Next) => window.focus_next(),
        Some(FocusDirection::Previous) => window.focus_prev(),
        None => {}
    }
}

fn focus_ring(color: gpui::Rgba) -> impl FnOnce(StyleRefinement) -> StyleRefinement {
    move |style| style.border_2().border_color(color)
}

// GPUI 0.2.2 only renders tooltips while the pointer hovers the element and
// exposes no accessible-name API, so icon-only controls get a hover tooltip as
// the only label the framework allows.
struct TextTooltip {
    label: SharedString,
    theme: Theme,
}

impl Render for TextTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded_md()
            .bg(self.theme.surface_alt)
            .border_1()
            .border_color(self.theme.border)
            .text_color(self.theme.text)
            .text_size(px(11.))
            .child(self.label.clone())
    }
}

fn text_tooltip(
    label: SharedString,
    theme: Theme,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| {
        cx.new(|_| TextTooltip {
            label: label.clone(),
            theme,
        })
        .into()
    }
}

/// GPUI 0.2.2's `TextField` keeps its focus handle private but exposes it
/// through `focus_handle`; marking that handle as a tab stop adds the field to
/// the window tab order without changing the widget.
pub fn register_field_tab_stop(field: &Entity<TextField>, cx: &App) {
    field
        .read(cx)
        .focus_handle(cx)
        .tab_stop(true)
        .tab_index(TAB_INDEX);
}

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
    let (background, hover, text, border, focus_color) = if primary {
        (
            theme.accent,
            theme.accent_hover,
            theme.accent_text,
            theme.accent,
            theme.accent_text,
        )
    } else {
        (
            theme.surface_alt,
            theme.surface_hover,
            theme.text,
            theme.border,
            theme.accent,
        )
    };
    div()
        .id(id)
        .tab_index(TAB_INDEX)
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
        .focus(focus_ring(focus_color))
        .hover(move |style| style.bg(hover))
        .on_click(on_click)
        .child(label)
}

pub fn titlebar_button(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    label: impl Into<SharedString>,
    theme: Theme,
    danger: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = label.into();
    let color = if danger {
        theme.danger
    } else {
        theme.text_muted
    };
    div()
        .id(id)
        .tab_index(TAB_INDEX)
        .flex()
        .items_center()
        .justify_center()
        .w(px(42.))
        .h_full()
        .border_1()
        .border_color(transparent_black())
        .cursor_pointer()
        .focus(move |style| {
            style
                .border_2()
                .bg(theme.surface_hover)
                .border_color(theme.accent)
        })
        .hover(move |style| {
            style.bg(if danger {
                theme.danger
            } else {
                theme.surface_hover
            })
        })
        .tooltip(text_tooltip(label, theme))
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
        .tab_index(TAB_INDEX)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .py(px(10.))
        .rounded_md()
        .border_1()
        .border_color(transparent_black())
        .cursor_pointer()
        .focus(move |style| {
            style
                .border_2()
                .bg(theme.surface_hover)
                .border_color(theme.accent)
        })
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
    let marker_color: gpui::Hsla = if active {
        theme.accent.into()
    } else {
        transparent_black()
    };
    div()
        .id(id)
        .tab_index(TAB_INDEX)
        .flex()
        .items_center()
        .gap(px(10.))
        .h(px(38.))
        .px(px(12.))
        .rounded_md()
        .border_1()
        .border_color(transparent_black())
        .cursor_pointer()
        .bg(if active {
            theme.surface_hover
        } else {
            theme.surface
        })
        .text_color(if active { theme.text } else { theme.text_muted })
        .text_size(px(13.))
        .focus(move |style| {
            style
                .border_2()
                .bg(theme.surface_hover)
                .border_color(theme.accent)
        })
        .hover(move |style| style.bg(theme.surface_hover))
        .on_click(on_click)
        .child(div().w(px(2.)).h(px(16.)).rounded_full().bg(marker_color))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn direction(keystroke: &str) -> Option<FocusDirection> {
        focus_direction(&Keystroke::parse(keystroke).unwrap())
    }

    #[test]
    fn tab_moves_focus_forward() {
        assert_eq!(direction("tab"), Some(FocusDirection::Next));
    }

    #[test]
    fn shift_tab_moves_focus_backward() {
        assert_eq!(direction("shift-tab"), Some(FocusDirection::Previous));
    }

    #[test]
    fn modified_tab_does_not_move_focus() {
        assert_eq!(direction("ctrl-tab"), None);
        assert_eq!(direction("alt-tab"), None);
        assert_eq!(direction("cmd-tab"), None);
    }

    #[test]
    fn other_keys_do_not_move_focus() {
        assert_eq!(direction("enter"), None);
        assert_eq!(direction("space"), None);
        assert_eq!(direction("escape"), None);
    }
}
