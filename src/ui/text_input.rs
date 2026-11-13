use std::ops::Range;
use std::rc::Rc;
use std::sync::Once;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill,
    point, prelude::*, px, relative, rgb, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

const KEY_CONTEXT: &str = "TextField";
const DEFAULT_MAX_LENGTH: usize = 512;
const UNDO_LIMIT: usize = 100;
const MASK: &str = "•";
const PLACEHOLDER_COLOR: u32 = 0x8696a0;
const TEXT_COLOR: u32 = 0x111b21;
const BORDER_COLOR: u32 = 0xd1d7db;
const FOCUS_COLOR: u32 = 0x00a884;
const CARET_COLOR: u32 = 0x00a884;
const SELECTION_COLOR: u32 = 0x00a88435;

actions!(
    text_field,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        SelectHome,
        SelectEnd,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Submit,
    ]
);

type EnterCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// A single-line text editor with caret, selection, clipboard, IME and undo.
///
/// Text is limited by grapheme clusters, can be masked for passwords, and can
/// run an `on_enter` callback. GPUI's X11 backend delivers dead keys and XIM
/// composition through [`EntityInputHandler`], so this component never
/// translates keys manually.
///
/// The editing behavior lives in [`TextState`] so it can be tested without a
/// GPUI window. [`TextField`] owns the focus handle and paints the state.
pub struct TextField {
    state: TextState,
    placeholder: SharedString,
    masked: bool,
    focus_handle: FocusHandle,
    on_enter: Option<EnterCallback>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
}

impl TextField {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        register_key_bindings(cx);
        Self {
            state: TextState::new(DEFAULT_MAX_LENGTH),
            placeholder: placeholder.into(),
            masked: false,
            focus_handle: cx.focus_handle(),
            on_enter: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
        }
    }

    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.state.set_content(content);
        self
    }

    pub fn on_enter(mut self, callback: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_enter = Some(Rc::new(callback));
        self
    }

    pub fn content(&self) -> &str {
        &self.state.content
    }

    pub fn set_content(&mut self, content: impl Into<String>, cx: &mut Context<Self>) {
        self.state.set_content(content);
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_content("", cx);
    }

    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn resolved_range(&self, range: Option<Range<usize>>) -> Range<usize> {
        range
            .or_else(|| self.state.marked_range.clone())
            .unwrap_or_else(|| self.state.selected_range.clone())
    }

    fn replace_text(&mut self, range: Option<Range<usize>>, text: &str, cx: &mut Context<Self>) {
        let range = self.resolved_range(range);
        self.state.replace_range(range, text);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.state.delete_backward();
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.state.delete_forward();
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.state.move_left();
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.state.move_right();
        cx.notify();
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_previous_grapheme();
        cx.notify();
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_next_grapheme();
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.state.move_to(0);
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.state.move_to(self.state.content.len());
        cx.notify();
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_to(0);
        cx.notify();
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.state.select_to(self.state.content.len());
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text(None, &text, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.state.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.state.selected_text().to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.state.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.state.selected_text().to_string(),
            ));
            self.replace_text(None, "", cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.undo() {
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.redo() {
            cx.notify();
        }
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(callback) = self.on_enter.clone() {
            window.defer(cx, move |window, cx| callback(window, cx));
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;

        if event.modifiers.shift {
            self.state
                .select_to(self.index_for_mouse_position(event.position));
        } else {
            self.state
                .move_to(self.index_for_mouse_position(event.position));
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.state
                .select_to(self.index_for_mouse_position(event.position));
            cx.notify();
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.state.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.state.content.len();
        }
        let display_offset = line.closest_index_for_x(position.x - bounds.left());
        self.state
            .content_offset_from_display(self.masked, display_offset)
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.state.range_from_utf16(&range_utf16);
        actual_range.replace(self.state.range_to_utf16(&range));
        Some(self.state.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.state.range_to_utf16(&self.state.selected_range),
            reversed: self.state.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.state
            .marked_range
            .as_ref()
            .map(|range| self.state.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.marked_range = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.state.range_from_utf16(&range));
        self.replace_text(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.state.range_from_utf16(&range));
        let range = self.resolved_range(range);
        self.state
            .replace_and_mark(range, new_text, new_selected_range_utf16);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.state.range_from_utf16(&range_utf16);
        let start = self.state.display_offset(self.masked, range.start);
        let end = self.state.display_offset(self.masked, range.end);
        Some(Bounds::from_corners(
            point(bounds.left() + last_layout.x_for_index(start), bounds.top()),
            point(
                bounds.left() + last_layout.x_for_index(end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let display_index = last_layout.index_for_x(line_point.x)?;
        let content_index = self
            .state
            .content_offset_from_display(self.masked, display_index);
        Some(self.state.offset_to_utf16(content_index))
    }
}

struct TextFieldElement {
    input: Entity<TextField>,
}

struct PrepaintState {
    line: ShapedLine,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextFieldElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextFieldElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = &input.state.content;
        let selected_range = input.state.selected_range.clone();
        let cursor = input.state.cursor_offset();
        let marked_range = input.state.marked_range.clone();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), rgb(PLACEHOLDER_COLOR).into())
        } else {
            (
                input.state.display_text(input.masked),
                rgb(TEXT_COLOR).into(),
            )
        };
        let masked = input.masked;

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range {
            let start = input.state.display_offset(masked, marked_range.start);
            let end = input.state.display_offset(masked, marked_range.end);
            vec![
                TextRun {
                    len: start,
                    ..run.clone()
                },
                TextRun {
                    len: end.saturating_sub(start),
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len().saturating_sub(end),
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_offset = input.state.display_offset(masked, cursor);
        let cursor_position = line.x_for_index(cursor_offset);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_position, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    rgb(CARET_COLOR),
                )),
            )
        } else {
            let start = input.state.display_offset(masked, selected_range.start);
            let end = input.state.display_offset(masked, selected_range.end);
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(start), bounds.top()),
                        point(bounds.left() + line.x_for_index(end), bounds.bottom()),
                    ),
                    rgba(SELECTION_COLOR),
                )),
                None,
            )
        };

        PrepaintState {
            line,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let line = prepaint.line.clone();
        let _ = line.paint(
            bounds.origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);

        div()
            .flex()
            .items_center()
            .h(px(38.))
            .w_full()
            .px(px(12.))
            // Long values are clipped rather than horizontally scrolled.
            .overflow_hidden()
            .bg(rgb(0xffffff))
            .border_1()
            .border_color(if focused {
                rgb(FOCUS_COLOR)
            } else {
                rgb(BORDER_COLOR)
            })
            .rounded_md()
            .text_color(rgb(TEXT_COLOR))
            .text_size(px(14.))
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextFieldElement { input: cx.entity() })
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn register_key_bindings(cx: &mut App) {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        let context = Some(KEY_CONTEXT);
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, context),
            KeyBinding::new("delete", Delete, context),
            KeyBinding::new("left", Left, context),
            KeyBinding::new("right", Right, context),
            KeyBinding::new("shift-left", SelectLeft, context),
            KeyBinding::new("shift-right", SelectRight, context),
            KeyBinding::new("ctrl-a", SelectAll, context),
            KeyBinding::new("home", Home, context),
            KeyBinding::new("end", End, context),
            KeyBinding::new("shift-home", SelectHome, context),
            KeyBinding::new("shift-end", SelectEnd, context),
            KeyBinding::new("ctrl-c", Copy, context),
            KeyBinding::new("ctrl-x", Cut, context),
            KeyBinding::new("ctrl-v", Paste, context),
            KeyBinding::new("ctrl-shift-c", Copy, context),
            KeyBinding::new("ctrl-shift-x", Cut, context),
            KeyBinding::new("ctrl-shift-v", Paste, context),
            KeyBinding::new("ctrl-z", Undo, context),
            KeyBinding::new("ctrl-shift-z", Redo, context),
            KeyBinding::new("ctrl-y", Redo, context),
            KeyBinding::new("enter", Submit, context),
        ]);
    });
}

#[derive(Clone, Debug)]
struct TextSnapshot {
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
}

struct TextState {
    content: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    max_length: usize,
    undo_stack: Vec<TextSnapshot>,
    redo_stack: Vec<TextSnapshot>,
}

impl TextState {
    fn new(max_length: usize) -> Self {
        Self {
            content: String::new(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            max_length,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn grapheme_count(&self) -> usize {
        self.content.graphemes(true).count()
    }

    fn grapheme_count_in(&self, range: &Range<usize>) -> usize {
        self.content
            .get(range.clone())
            .map_or(0, |content| content.graphemes(true).count())
    }

    fn display_text(&self, masked: bool) -> SharedString {
        if masked {
            SharedString::from(MASK.repeat(self.grapheme_count()))
        } else {
            SharedString::from(self.content.clone())
        }
    }

    fn display_offset(&self, masked: bool, content_offset: usize) -> usize {
        if !masked {
            return content_offset;
        }
        let graphemes = self
            .content
            .grapheme_indices(true)
            .take_while(|(index, _)| *index < content_offset)
            .count();
        graphemes * MASK.len()
    }

    fn content_offset_from_display(&self, masked: bool, display_offset: usize) -> usize {
        if !masked {
            return display_offset.min(self.content.len());
        }
        let graphemes = display_offset / MASK.len();
        self.content
            .grapheme_indices(true)
            .nth(graphemes)
            .map(|(index, _)| index)
            .unwrap_or(self.content.len())
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    fn select_all(&mut self) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
    }

    fn selected_text(&self) -> &str {
        &self.content[self.selected_range.clone()]
    }

    fn move_left(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    fn move_right(&mut self) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()));
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    fn select_previous_grapheme(&mut self) {
        self.select_to(self.previous_boundary(self.cursor_offset()));
    }

    fn select_next_grapheme(&mut self) {
        self.select_to(self.next_boundary(self.cursor_offset()));
    }

    fn delete_backward(&mut self) {
        if let Some(marked_range) = self.marked_range.clone() {
            self.replace_range(marked_range, "");
            return;
        }
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = self.previous_boundary(cursor)..cursor;
            self.selection_reversed = false;
        }
        let range = self.selected_range.clone();
        self.replace_range(range, "");
    }

    fn delete_forward(&mut self) {
        if let Some(marked_range) = self.marked_range.clone() {
            self.replace_range(marked_range, "");
            return;
        }
        if self.selected_range.is_empty() {
            let cursor = self.cursor_offset();
            self.selected_range = cursor..self.next_boundary(cursor);
            self.selection_reversed = false;
        }
        let range = self.selected_range.clone();
        self.replace_range(range, "");
    }

    fn set_content(&mut self, content: impl Into<String>) {
        self.content = sanitize_single_line(&content.into());
        self.truncate_to_max_length();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    fn truncate_to_max_length(&mut self) {
        if self.grapheme_count() > self.max_length {
            self.content = self.content.graphemes(true).take(self.max_length).collect();
            self.selected_range = self.content.len()..self.content.len();
            self.selection_reversed = false;
            self.marked_range = None;
        }
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str) -> String {
        let clipped = self.clipped_text(&range, text);
        if clipped.is_empty() && range.is_empty() {
            self.marked_range = None;
            self.selected_range = range.start..range.start;
            self.selection_reversed = false;
            return clipped;
        }

        self.record_undo();
        self.content.replace_range(range.clone(), &clipped);
        let cursor = range.start + clipped.len();
        self.selected_range = cursor..cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        clipped
    }

    fn replace_and_mark(
        &mut self,
        range: Range<usize>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
    ) {
        let inserted = self.replace_range(range.clone(), new_text);
        if !inserted.is_empty() {
            self.marked_range = Some(range.start..range.start + inserted.len());
        }
        if let Some(relative_range) = new_selected_range_utf16 {
            let start = range.start + utf16_to_byte_offset(&inserted, relative_range.start);
            let end = range.start + utf16_to_byte_offset(&inserted, relative_range.end);
            self.selected_range =
                start.min(range.start + inserted.len())..end.min(range.start + inserted.len());
            self.selection_reversed = false;
        }
    }

    fn clipped_text(&self, range: &Range<usize>, text: &str) -> String {
        let sanitized = sanitize_single_line(text);
        let removed = self.grapheme_count_in(range);
        let available = self
            .max_length
            .saturating_sub(self.grapheme_count().saturating_sub(removed));
        sanitized.graphemes(true).take(available).collect()
    }

    fn record_undo(&mut self) {
        self.undo_stack.push(TextSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        });
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    fn snapshot(&self) -> TextSnapshot {
        TextSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn restore(&mut self, snapshot: TextSnapshot) {
        self.content = snapshot.content;
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.marked_range = None;
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

fn sanitize_single_line(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    result.push(' ');
                }
            }
            '\n' => result.push(' '),
            ch if ch.is_control() => {}
            ch => result.push(ch),
        }
    }
    result
}

fn utf16_to_byte_offset(text: &str, utf16_offset: usize) -> usize {
    let mut utf16_count = 0;
    for (byte_offset, ch) in text.char_indices() {
        if utf16_count >= utf16_offset {
            return byte_offset;
        }
        utf16_count += ch.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(content: &str) -> TextState {
        let mut state = TextState::new(DEFAULT_MAX_LENGTH);
        state.set_content(content);
        state
    }

    fn type_text(state: &mut TextState, text: &str) {
        let range = state.selected_range.clone();
        state.replace_range(range, text);
    }

    #[test]
    fn inserts_text_at_the_cursor() {
        let mut state = TextState::new(DEFAULT_MAX_LENGTH);
        type_text(&mut state, "hola");
        assert_eq!(state.content, "hola");
        assert_eq!(state.selected_range, 4..4);

        state.move_to(2);
        type_text(&mut state, "X");
        assert_eq!(state.content, "hoXla");
        assert_eq!(state.selected_range, 3..3);
    }

    #[test]
    fn backspace_removes_one_grapheme_cluster() {
        let mut state = state_with("a👨‍👩‍👧‍👦");
        state.delete_backward();
        assert_eq!(state.content, "a");
        assert_eq!(state.selected_range, 1..1);

        state.delete_backward();
        assert_eq!(state.content, "");
        assert_eq!(state.selected_range, 0..0);
    }

    #[test]
    fn forward_delete_removes_the_next_grapheme_cluster() {
        let mut state = state_with("año");
        state.move_to(1);
        state.delete_forward();
        assert_eq!(state.content, "ao");
        assert_eq!(state.selected_range, 1..1);
    }

    #[test]
    fn backspace_on_a_selection_removes_only_the_selection() {
        let mut state = state_with("hello world");
        state.selected_range = 5..11;
        state.delete_backward();
        assert_eq!(state.content, "hello");
        assert_eq!(state.selected_range, 5..5);
    }

    #[test]
    fn movement_collapses_selection_to_its_edges() {
        let mut state = state_with("abcdef");
        state.selected_range = 2..5;
        state.move_left();
        assert_eq!(state.selected_range, 2..2);
        state.select_next_grapheme();
        assert_eq!(state.selected_range, 2..3);
        state.select_to(5);
        state.move_right();
        assert_eq!(state.selected_range, 5..5);
    }

    #[test]
    fn arrow_keys_move_by_grapheme_cluster() {
        let mut state = state_with("añb");
        assert_eq!(state.selected_range, 4..4);
        state.move_left();
        assert_eq!(state.selected_range, 3..3);
        state.move_left();
        assert_eq!(state.selected_range, 1..1);
        state.move_right();
        assert_eq!(state.selected_range, 3..3);
    }

    #[test]
    fn select_all_and_shift_arrows_build_a_selection() {
        let mut state = state_with("abc");
        state.select_all();
        assert_eq!(state.selected_range, 0..3);
        assert_eq!(state.selected_text(), "abc");

        state.move_to(1);
        state.select_next_grapheme();
        state.select_next_grapheme();
        assert_eq!(state.selected_range, 1..3);
        assert_eq!(state.selected_text(), "bc");
    }

    #[test]
    fn home_and_end_move_to_the_content_edges() {
        let mut state = state_with("hello");
        state.move_to(0);
        assert_eq!(state.selected_range, 0..0);
        state.move_to(state.content.len());
        assert_eq!(state.selected_range, 5..5);
    }

    #[test]
    fn max_length_counts_grapheme_clusters() {
        let mut state = TextState::new(3);
        type_text(&mut state, "a🔟"); // "🔟" is one grapheme cluster
        assert_eq!(state.content, "a🔟");
        type_text(&mut state, "b");
        assert_eq!(state.content, "a🔟b");
        type_text(&mut state, "c");
        assert_eq!(state.content, "a🔟b");
    }

    #[test]
    fn pasted_text_respects_the_remaining_grapheme_budget() {
        let mut state = TextState::new(5);
        state.set_content("ab");
        type_text(&mut state, "cd\nef");
        assert_eq!(state.content, "abcd ");
        assert_eq!(state.selected_range, 5..5);
    }

    #[test]
    fn replacement_can_free_space_for_new_graphemes() {
        let mut state = TextState::new(3);
        state.set_content("abc");
        state.selected_range = 1..3;
        type_text(&mut state, "xyz");
        assert_eq!(state.content, "axy");
    }

    #[test]
    fn set_content_truncates_by_grapheme_and_places_the_cursor_at_the_end() {
        let mut state = TextState::new(2);
        state.set_content("👨‍👩‍👧‍👦añ");
        assert_eq!(state.content, "👨‍👩‍👧‍👦a");
        assert_eq!(
            state.selected_range,
            state.content.len()..state.content.len()
        );
    }

    #[test]
    fn control_characters_are_removed_from_single_line_input() {
        let mut state = TextState::new(16);
        type_text(&mut state, "a\tb\nc\rd");
        assert_eq!(state.content, "ab c d");
    }

    #[test]
    fn crlf_is_normalized_to_a_single_space() {
        let mut state = TextState::new(16);
        type_text(&mut state, "a\r\nb");
        assert_eq!(state.content, "a b");
    }

    #[test]
    fn undo_and_redo_restore_content_and_selection() {
        let mut state = TextState::new(DEFAULT_MAX_LENGTH);
        type_text(&mut state, "ab");
        type_text(&mut state, "c");
        assert_eq!(state.content, "abc");

        assert!(state.undo());
        assert_eq!(state.content, "ab");
        assert_eq!(state.selected_range, 2..2);

        assert!(state.undo());
        assert_eq!(state.content, "");

        assert!(state.redo());
        assert_eq!(state.content, "ab");

        assert!(state.redo());
        assert_eq!(state.content, "abc");
        assert!(!state.redo());
    }

    #[test]
    fn undo_history_is_cleared_when_content_is_replaced_programmatically() {
        let mut state = TextState::new(DEFAULT_MAX_LENGTH);
        type_text(&mut state, "secret");
        state.set_content("");
        assert!(!state.undo());
        assert_eq!(state.content, "");
    }

    #[test]
    fn utf16_conversions_handle_astral_characters() {
        let state = state_with("a😀b");
        assert_eq!(state.offset_to_utf16(0), 0);
        assert_eq!(state.offset_to_utf16(1), 1);
        assert_eq!(state.offset_to_utf16(5), 3);
        assert_eq!(state.offset_to_utf16(6), 4);
        assert_eq!(state.offset_from_utf16(1), 1);
        assert_eq!(state.offset_from_utf16(2), 5);
        assert_eq!(state.offset_from_utf16(3), 5);
        assert_eq!(state.range_to_utf16(&(1..5)), 1..3);
        assert_eq!(state.range_from_utf16(&(1..3)), 1..5);
    }

    #[test]
    fn masked_display_uses_one_bullet_per_grapheme() {
        let state = state_with("año");

        assert_eq!(state.display_text(true), "•••");
        assert_eq!(state.display_text(false), "año");
        assert_eq!(state.display_offset(true, 0), 0);
        assert_eq!(state.display_offset(true, 1), 3);
        assert_eq!(state.display_offset(true, 3), 6);
        assert_eq!(state.display_offset(true, 4), 9);
        assert_eq!(state.content_offset_from_display(true, 0), 0);
        assert_eq!(state.content_offset_from_display(true, 3), 1);
        assert_eq!(state.content_offset_from_display(true, 6), 3);
        assert_eq!(state.content_offset_from_display(true, 9), 4);
    }

    #[test]
    fn unmasked_display_matches_the_content() {
        let state = state_with("año");

        assert_eq!(state.display_offset(false, 3), 3);
        assert_eq!(state.content_offset_from_display(false, 3), 3);
        assert_eq!(state.content_offset_from_display(false, 99), 4);
    }
}
