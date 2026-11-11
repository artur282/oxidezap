use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, KeyDownEvent, Render, ScrollHandle,
    SharedString, Subscription, Timer, Window, div, prelude::*, px, transparent_black,
};

use crate::app::AppModel;
use crate::assets::{ICON_PALETTE, ICON_X};
use crate::settings::{ThemeMode, ZOOM_LEVELS};
use crate::theme::Theme;
use crate::ui::components::{icon, register_field_tab_stop};
use crate::ui::text_input::TextField;

/// Size and placement of the floating palette window.
pub const COMMAND_PALETTE_WIDTH: f32 = 620.0;
pub const COMMAND_PALETTE_HEIGHT: f32 = 460.0;
pub const COMMAND_PALETTE_TOP_OFFSET: f32 = 72.0;

const QUERY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ROW_HEIGHT: f32 = 34.0;

/// Stable identifiers for every command the palette can offer. The identifier
/// is used by tests and by the UI to tell commands apart; execution always goes
/// through [`CommandAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    ShowOrHide,
    ShowAll,
    HideAll,
    OpenSettings,
    AddAccount,
    SwitchAccount,
    OpenDownloadsWindow,
    OpenDownloadsFolder,
    OpenStorageFolder,
    Lock,
    SetDoNotDisturb,
    SetTheme,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    RetryConnection,
    Quit,
}

impl CommandKind {
    /// Short group label shown on the right side of a palette row.
    pub fn category(self) -> &'static str {
        match self {
            CommandKind::ShowOrHide | CommandKind::ShowAll | CommandKind::HideAll => "Window",
            CommandKind::OpenSettings => "App",
            CommandKind::AddAccount | CommandKind::SwitchAccount => "Accounts",
            CommandKind::OpenDownloadsWindow | CommandKind::OpenDownloadsFolder => "Downloads",
            CommandKind::OpenStorageFolder => "Storage",
            CommandKind::Lock => "Security",
            CommandKind::SetDoNotDisturb => "Notifications",
            CommandKind::SetTheme => "Appearance",
            CommandKind::ZoomIn | CommandKind::ZoomOut | CommandKind::ZoomReset => "Display",
            CommandKind::RetryConnection => "Connection",
            CommandKind::Quit => "App",
        }
    }
}

/// Everything a command can do. Each variant maps to an existing `AppModel`
/// action; the palette never invents behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandAction {
    ToggleVisibility,
    ShowAll,
    HideAll,
    OpenSettings,
    AddAccount,
    OpenAccount { index: usize, account_id: String },
    OpenDownloadsWindow,
    OpenDownloadsFolder,
    OpenStorageFolder,
    Lock,
    SetDoNotDisturb(bool),
    SetTheme(ThemeMode),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    RetryConnection { account_id: String },
    Quit,
}

impl CommandAction {
    /// Commands that leave the account webview as the place the user returns
    /// to. Actions that open another window or hide the app are excluded so the
    /// palette never steals focus from the window it just opened.
    pub fn focus_account_after(&self, active_account: Option<String>) -> Option<String> {
        match self {
            CommandAction::OpenAccount { account_id, .. } => Some(account_id.clone()),
            CommandAction::RetryConnection { account_id } => Some(account_id.clone()),
            CommandAction::AddAccount
            | CommandAction::ToggleVisibility
            | CommandAction::ShowAll
            | CommandAction::OpenDownloadsFolder
            | CommandAction::OpenStorageFolder
            | CommandAction::SetDoNotDisturb(_)
            | CommandAction::SetTheme(_)
            | CommandAction::ZoomIn
            | CommandAction::ZoomOut
            | CommandAction::ZoomReset => active_account,
            CommandAction::HideAll
            | CommandAction::OpenSettings
            | CommandAction::OpenDownloadsWindow
            | CommandAction::Lock
            | CommandAction::Quit => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub kind: CommandKind,
    pub title: String,
    pub keywords: Vec<String>,
    pub action: CommandAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandAccount {
    pub id: String,
    pub name: String,
}

/// Snapshot of the model state needed to build the command list. Keeping this
/// as data makes command construction and filtering testable without a GPUI
/// window or a running application.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandContext {
    pub accounts: Vec<CommandAccount>,
    pub active_account: Option<String>,
    pub lock_available: bool,
    pub do_not_disturb: bool,
    pub theme: ThemeMode,
    pub zoom: f64,
    pub retry_account: Option<String>,
    pub retry_available: bool,
}

/// The palette's pure selection model. Commands are stored once; `matches`
/// holds indexes into that list so selection and filtering never depend on UI
/// state.
pub struct PaletteState {
    commands: Vec<Command>,
    matches: Vec<usize>,
    selected: usize,
    query: String,
}

impl PaletteState {
    pub fn new(commands: Vec<Command>) -> Self {
        let matches = search_commands(&commands, "");
        Self {
            commands,
            matches,
            selected: 0,
            query: String::new(),
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn matches(&self) -> &[usize] {
        &self.matches
    }

    pub fn selected_match_index(&self) -> Option<usize> {
        if self.matches.is_empty() {
            None
        } else {
            Some(self.selected.min(self.matches.len() - 1))
        }
    }

    pub fn command_at(&self, match_index: usize) -> Option<&Command> {
        self.matches
            .get(match_index)
            .and_then(|command_index| self.commands.get(*command_index))
    }

    pub fn selected_command(&self) -> Option<&Command> {
        self.selected_match_index()
            .and_then(|match_index| self.command_at(match_index))
    }

    pub fn set_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query == self.query {
            return;
        }
        self.query = query;
        self.matches = search_commands(&self.commands, &self.query);
        self.selected = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.matches.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
    }
}

const TITLE_EXACT: i64 = 1000;
const TITLE_PREFIX: i64 = 900;
const TITLE_WORD_PREFIX: i64 = 800;
const TITLE_SUBSTRING: i64 = 700;
const TITLE_FUZZY: i64 = 600;
const KEYWORD_PREFIX: i64 = 400;
const KEYWORD_SUBSTRING: i64 = 300;
const KEYWORD_FUZZY: i64 = 200;
const FUZZY_MAX: i64 = 50;

/// Builds the command list in a stable, human-friendly order: window actions
/// first, then accounts and app-wide preferences, with Quit last so it is never
/// the accidental Enter target.
pub fn build_commands(context: &CommandContext) -> Vec<Command> {
    let mut commands = vec![
        command(
            CommandKind::ShowOrHide,
            "Show or hide OxideZap",
            &["window", "visibility", "toggle", "unhide"],
            CommandAction::ToggleVisibility,
        ),
        command(
            CommandKind::ShowAll,
            "Show all account windows",
            &["restore", "windows", "unhide"],
            CommandAction::ShowAll,
        ),
        command(
            CommandKind::HideAll,
            "Hide all account windows",
            &["minimize", "windows", "privacy"],
            CommandAction::HideAll,
        ),
        command(
            CommandKind::OpenSettings,
            "Open settings",
            &["preferences", "options", "configure"],
            CommandAction::OpenSettings,
        ),
        command(
            CommandKind::AddAccount,
            "Add account",
            &["new", "profile", "whatsapp"],
            CommandAction::AddAccount,
        ),
    ];

    for (index, account) in context.accounts.iter().enumerate() {
        let position = index + 1;
        let current = context.active_account.as_deref() == Some(account.id.as_str());
        let title = if current {
            format!("Switch to account {position}: {} (current)", account.name)
        } else {
            format!("Switch to account {position}: {}", account.name)
        };
        commands.push(command(
            CommandKind::SwitchAccount,
            title,
            &["account", "switch", "profile"],
            CommandAction::OpenAccount {
                index: position,
                account_id: account.id.clone(),
            },
        ));
    }

    commands.extend([
        command(
            CommandKind::OpenDownloadsWindow,
            "Open download center",
            &["downloads", "files", "history"],
            CommandAction::OpenDownloadsWindow,
        ),
        command(
            CommandKind::OpenDownloadsFolder,
            "Open downloads folder",
            &["downloads", "folder", "files"],
            CommandAction::OpenDownloadsFolder,
        ),
        command(
            CommandKind::OpenStorageFolder,
            "Open storage folder",
            &["data", "profile", "storage"],
            CommandAction::OpenStorageFolder,
        ),
    ]);

    if context.lock_available {
        commands.push(command(
            CommandKind::Lock,
            "Lock OxideZap",
            &["security", "password", "privacy"],
            CommandAction::Lock,
        ));
    }

    let (dnd_title, dnd_action) = if context.do_not_disturb {
        (
            "Turn off Do Not Disturb",
            CommandAction::SetDoNotDisturb(false),
        )
    } else {
        (
            "Turn on Do Not Disturb",
            CommandAction::SetDoNotDisturb(true),
        )
    };
    commands.push(command(
        CommandKind::SetDoNotDisturb,
        dnd_title,
        &["dnd", "mute", "notifications", "silent"],
        dnd_action,
    ));
    commands.extend([
        command(
            CommandKind::SetTheme,
            "Use light theme",
            &["theme", "appearance", "color"],
            CommandAction::SetTheme(ThemeMode::Light),
        ),
        command(
            CommandKind::SetTheme,
            "Use dark theme",
            &["theme", "appearance", "color"],
            CommandAction::SetTheme(ThemeMode::Dark),
        ),
        command(
            CommandKind::SetTheme,
            "Follow the system theme",
            &["theme", "appearance", "color"],
            CommandAction::SetTheme(ThemeMode::System),
        ),
        command(
            CommandKind::ZoomIn,
            "Zoom in",
            &["zoom", "scale", "text", "larger"],
            CommandAction::ZoomIn,
        ),
        command(
            CommandKind::ZoomOut,
            "Zoom out",
            &["zoom", "scale", "text", "smaller"],
            CommandAction::ZoomOut,
        ),
        command(
            CommandKind::ZoomReset,
            "Reset zoom",
            &["zoom", "scale", "normal", "100"],
            CommandAction::ZoomReset,
        ),
    ]);

    if context.retry_available
        && let Some(account_id) = context.retry_account.as_ref()
    {
        commands.push(command(
            CommandKind::RetryConnection,
            "Retry connection",
            &["reconnect", "reload", "offline", "network"],
            CommandAction::RetryConnection {
                account_id: account_id.clone(),
            },
        ));
    }

    commands.push(command(
        CommandKind::Quit,
        "Quit OxideZap",
        &["exit", "close", "shutdown"],
        CommandAction::Quit,
    ));
    commands
}

fn command(
    kind: CommandKind,
    title: impl Into<String>,
    keywords: &[&str],
    action: CommandAction,
) -> Command {
    Command {
        kind,
        title: title.into(),
        keywords: keywords
            .iter()
            .map(|keyword| (*keyword).to_string())
            .collect(),
        action,
    }
}

/// Returns indexes into `commands`, best match first. Equal scores keep the
/// original definition order so ranking is deterministic between frames.
pub fn search_commands(commands: &[Command], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return (0..commands.len()).collect();
    }

    let mut scored: Vec<(usize, i64)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| command_score(command, &query).map(|score| (index, score)))
        .collect();
    scored.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_index.cmp(right_index))
    });
    scored.into_iter().map(|(index, _)| index).collect()
}

fn command_score(command: &Command, query: &str) -> Option<i64> {
    let title = command.title.to_lowercase();
    if title == query {
        return Some(TITLE_EXACT);
    }
    if title.starts_with(query) {
        return Some(TITLE_PREFIX);
    }
    if title.split_whitespace().any(|word| word.starts_with(query)) {
        return Some(TITLE_WORD_PREFIX);
    }
    if title.contains(query) {
        return Some(TITLE_SUBSTRING);
    }
    if let Some(score) = fuzzy_score(&title, query) {
        return Some(TITLE_FUZZY + score);
    }

    let mut best = None;
    for keyword in &command.keywords {
        let keyword = keyword.to_lowercase();
        let score = if keyword.starts_with(query) {
            Some(KEYWORD_PREFIX)
        } else if keyword.contains(query) {
            Some(KEYWORD_SUBSTRING)
        } else {
            fuzzy_score(&keyword, query).map(|score| KEYWORD_FUZZY + score)
        };
        if let Some(score) = score {
            best = Some(best.map_or(score, |best: i64| best.max(score)));
        }
    }
    best
}

/// Greedy ordered subsequence match. The score rewards a tight span: a match
/// with no gaps scores the maximum and every skipped character costs a point.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
    let mut indices = haystack.char_indices();
    let mut first = None;
    let mut last = 0;
    for needle_char in needle.chars() {
        let (index, _) = indices.find(|(_, haystack_char)| *haystack_char == needle_char)?;
        first.get_or_insert(index);
        last = index;
    }
    let first = first?;
    let span = last - first + 1;
    let gaps = span.saturating_sub(needle.chars().count());
    Some((FUZZY_MAX - gaps as i64).max(1))
}

/// Maps the `1`..`9` keys to a zero-based account index. `Ctrl+1` is the first
/// account, mirroring the numbering used by the palette titles.
pub fn digit_account_index(key: &str) -> Option<usize> {
    key.parse::<usize>()
        .ok()
        .filter(|index| (1..=9).contains(index))
        .map(|index| index - 1)
}

pub fn account_at(accounts: &[CommandAccount], index: usize) -> Option<&CommandAccount> {
    accounts.get(index)
}

/// Next zoom preset in the requested direction. Returns the current zoom when
/// the list is already at the corresponding limit.
pub fn zoom_after(current: f64, direction: i8) -> f64 {
    if direction > 0 {
        ZOOM_LEVELS
            .iter()
            .copied()
            .find(|zoom| *zoom > current + 0.001)
            .unwrap_or(current)
    } else {
        ZOOM_LEVELS
            .iter()
            .rev()
            .copied()
            .find(|zoom| *zoom < current - 0.001)
            .unwrap_or(current)
    }
}

// Native, keyboard-first command palette.
//
// GPUI 0.2.2 has no AccessKit/AT-SPI bridge, so the palette is not exposed as
// a semantic widget tree. Keyboard access uses the search field as the single
// tab stop; Tab is trapped in the field, Up/Down move a visible selection, and
// Enter runs the selected command. The selected row is drawn with a
// theme-colored marker and background.

/// The floating palette view. It owns the query/selection model, the search
/// field and the scroll handle; every command is dispatched through
/// [`AppModel::execute_command`].
pub struct CommandPalette {
    model: Entity<AppModel>,
    account_id: String,
    search: Entity<TextField>,
    focus_handle: gpui::FocusHandle,
    state: PaletteState,
    scroll_handle: ScrollHandle,
    appearance_subscription: Option<Subscription>,
    title_set: bool,
}

impl CommandPalette {
    pub fn new(model: Entity<AppModel>, account_id: String, cx: &mut Context<Self>) -> Self {
        let commands = build_commands(&model.read(cx).command_palette_context(&account_id));
        let state = PaletteState::new(commands);
        let search = cx.new(|cx| TextField::new("Search commands…", cx));
        register_field_tab_stop(&search, cx);

        let poll_search = search.clone();
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(QUERY_POLL_INTERVAL).await;
                let query = match poll_search.read_with(cx, |field, _| field.content().to_string())
                {
                    Ok(query) => query,
                    Err(_) => break,
                };
                let alive = this
                    .update(cx, |palette, cx| {
                        if palette.state.query() != query {
                            palette.state.set_query(query);
                            palette.scroll_selection();
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        Self {
            model,
            account_id,
            search,
            focus_handle: cx.focus_handle(),
            state,
            scroll_handle: ScrollHandle::new(),
            appearance_subscription: None,
            title_set: false,
        }
    }

    fn scroll_selection(&self) {
        if let Some(index) = self.state.selected_match_index() {
            self.scroll_handle.scroll_to_item(index);
        }
    }

    fn focus_search(&self, window: &mut Window, cx: &App) {
        let handle = self.search.read(cx).focus_handle(cx);
        window.focus(&handle);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.move_selection(delta);
        self.scroll_selection();
        cx.notify();
    }

    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let action = self
            .state
            .selected_command()
            .map(|command| command.action.clone());
        if let Some(action) = action {
            self.execute(action, window, cx);
        }
    }

    fn execute(&mut self, action: CommandAction, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        window.remove_window();
        model.update(cx, |model, cx| model.execute_command(action, cx));
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let account_id = self.account_id.clone();
        window.remove_window();
        model.update(cx, |model, _cx| {
            model.command_palette_dismissed(&account_id);
        });
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let command_modifier = keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.shift
            && !keystroke.modifiers.platform;
        match keystroke.key.as_str() {
            "escape" => {
                self.dismiss(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" => {
                self.run_selected(window, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_selection(1, cx);
                cx.stop_propagation();
            }
            "up" => {
                self.move_selection(-1, cx);
                cx.stop_propagation();
            }
            "tab" => {
                self.focus_search(window, cx);
                cx.stop_propagation();
            }
            "k" if command_modifier => {
                self.dismiss(window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.title_set {
            window.set_window_title("OxideZap command palette");
            self.title_set = true;
        }
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }
        if window.focused(cx).is_none() {
            self.focus_search(window, cx);
        }

        let theme = self.model.read(cx).theme(window);
        let count = self.state.matches().len();
        let selected = self.state.selected_match_index();

        let mut rows: Vec<AnyElement> = Vec::with_capacity(count);
        for (row, _) in self.state.matches().iter().enumerate() {
            let Some(command) = self.state.command_at(row).cloned() else {
                break;
            };
            let action = command.action.clone();
            let listener = cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.execute(action.clone(), window, cx);
            });
            rows.push(
                command_row(
                    row,
                    command.kind,
                    &command,
                    selected == Some(row),
                    theme,
                    listener,
                )
                .into_any_element(),
            );
        }

        let results: AnyElement = if count == 0 {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state(theme))
                .into_any_element()
        } else {
            div()
                .id("command-palette-results")
                .flex_1()
                .min_h(px(0.))
                .p(px(6.))
                .overflow_y_scroll()
                .track_scroll(&self.scroll_handle)
                .children(rows)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key))
            .child(search_row(self.search.clone(), theme, cx))
            .child(results)
            .child(footer(count, theme))
    }
}

fn search_row(
    search: Entity<TextField>,
    theme: Theme,
    cx: &mut Context<CommandPalette>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .pt(px(12.))
        .pb(px(10.))
        .border_b_1()
        .border_color(theme.border)
        .child(icon(ICON_PALETTE, 18.0, theme.accent))
        .child(div().flex_1().child(search))
        .child(
            div()
                .id("command-palette-close")
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded_md()
                .cursor_pointer()
                .hover(move |style| style.bg(theme.surface_hover))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.dismiss(window, cx);
                }))
                .child(icon(ICON_X, 14.0, theme.text_muted)),
        )
}

fn command_row(
    row: usize,
    kind: CommandKind,
    command: &Command,
    selected: bool,
    theme: Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let marker: gpui::Hsla = if selected {
        theme.accent.into()
    } else {
        transparent_black()
    };
    let text = if selected {
        theme.text
    } else {
        theme.text_muted
    };
    let background: gpui::Hsla = if selected {
        theme.surface_hover.into()
    } else {
        transparent_black()
    };
    div()
        .id(ElementId::named_usize("command-palette-row", row))
        .flex()
        .items_center()
        .gap(px(10.))
        .h(px(ROW_HEIGHT))
        .px(px(8.))
        .rounded_md()
        .bg(background)
        .cursor_pointer()
        .when(!selected, move |style| {
            style.hover(move |style| style.bg(theme.surface_alt))
        })
        .on_click(on_click)
        .child(div().w(px(2.)).h(px(16.)).rounded_full().bg(marker))
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .text_color(text)
                .child(SharedString::from(command.title.clone())),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme.text_faint)
                .child(kind.category()),
        )
}

fn empty_state(theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(14.))
                .text_color(theme.text)
                .child("No matching commands"),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child("Try a different search."),
        )
}

fn footer(count: usize, theme: Theme) -> impl IntoElement {
    let count_label = if count == 1 {
        "1 command".to_string()
    } else {
        format!("{count} commands")
    };
    div()
        .flex()
        .items_center()
        .justify_between()
        .px(px(14.))
        .py(px(8.))
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.text_faint)
                .child(SharedString::from(count_label)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.text_faint)
                .child("↑/↓ Move · Enter Run · Esc Close"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(kind: CommandKind, title: &str, keywords: &[&str]) -> Command {
        Command {
            kind,
            title: title.to_string(),
            keywords: keywords.iter().map(|keyword| keyword.to_string()).collect(),
            action: CommandAction::Quit,
        }
    }

    fn context() -> CommandContext {
        CommandContext {
            accounts: vec![
                CommandAccount {
                    id: "a".to_string(),
                    name: "Personal".to_string(),
                },
                CommandAccount {
                    id: "b".to_string(),
                    name: "Work".to_string(),
                },
            ],
            active_account: Some("a".to_string()),
            lock_available: true,
            do_not_disturb: false,
            theme: ThemeMode::Dark,
            zoom: 1.0,
            retry_account: Some("a".to_string()),
            retry_available: true,
        }
    }

    fn titles(commands: &[Command], matches: &[usize]) -> Vec<String> {
        matches
            .iter()
            .map(|index| commands[*index].title.clone())
            .collect()
    }

    fn find(commands: &[Command], kind: CommandKind) -> &Command {
        commands
            .iter()
            .find(|command| command.kind == kind)
            .unwrap_or_else(|| panic!("missing command kind {kind:?}"))
    }

    #[test]
    fn empty_query_keeps_the_natural_order() {
        let commands = vec![
            command(CommandKind::ShowOrHide, "Show or hide OxideZap", &[]),
            command(CommandKind::OpenSettings, "Open settings", &[]),
            command(CommandKind::Quit, "Quit OxideZap", &[]),
        ];

        assert_eq!(search_commands(&commands, ""), vec![0, 1, 2]);
        assert_eq!(search_commands(&commands, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn exact_title_outranks_prefix_word_prefix_substring_and_keyword() {
        let commands = vec![
            command(CommandKind::SetTheme, "Reset zoom", &[]),
            command(CommandKind::OpenSettings, "Preferences", &["settings"]),
            command(CommandKind::ShowOrHide, "Open settings", &[]),
            command(CommandKind::AddAccount, "Settings window", &[]),
            command(CommandKind::Quit, "Set", &[]),
        ];

        let matches = search_commands(&commands, "set");

        assert_eq!(
            titles(&commands, &matches),
            vec![
                "Set",
                "Settings window",
                "Open settings",
                "Reset zoom",
                "Preferences",
            ]
        );
    }

    #[test]
    fn title_matches_outrank_keyword_matches() {
        let commands = vec![
            command(CommandKind::OpenDownloadsWindow, "Files", &["downloads"]),
            command(CommandKind::OpenDownloadsFolder, "Open downloads", &[]),
        ];

        let matches = search_commands(&commands, "downloads");

        assert_eq!(titles(&commands, &matches), vec!["Open downloads", "Files"]);
    }

    #[test]
    fn keyword_only_matches_are_found() {
        let commands = vec![
            command(CommandKind::OpenSettings, "Preferences", &["configure"]),
            command(CommandKind::Quit, "Quit OxideZap", &[]),
        ];

        assert_eq!(search_commands(&commands, "configure"), vec![0]);
    }

    #[test]
    fn fuzzy_matches_scattered_characters() {
        let commands = vec![
            command(CommandKind::OpenSettings, "Open settings", &[]),
            command(CommandKind::Quit, "Quit OxideZap", &[]),
        ];

        assert_eq!(search_commands(&commands, "opst"), vec![0]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let commands = vec![command(CommandKind::OpenSettings, "Open Settings", &[])];

        assert_eq!(search_commands(&commands, "SETTINGS"), vec![0]);
    }

    #[test]
    fn equal_scores_keep_definition_order() {
        let commands = vec![
            command(CommandKind::ShowAll, "Open alpha", &[]),
            command(CommandKind::HideAll, "Open beta", &[]),
        ];

        assert_eq!(search_commands(&commands, "open"), vec![0, 1]);
    }

    #[test]
    fn no_match_returns_an_empty_result() {
        let commands = vec![command(CommandKind::Quit, "Quit OxideZap", &[])];

        assert!(search_commands(&commands, "zzzz").is_empty());
    }

    #[test]
    fn palette_starts_on_the_first_match() {
        let state = PaletteState::new(vec![
            command(CommandKind::ShowOrHide, "Show or hide OxideZap", &[]),
            command(CommandKind::OpenSettings, "Open settings", &[]),
        ]);

        assert_eq!(state.query(), "");
        assert_eq!(state.matches(), &[0, 1]);
        assert_eq!(state.selected_match_index(), Some(0));
        assert_eq!(
            state
                .selected_command()
                .map(|command| command.title.as_str()),
            Some("Show or hide OxideZap")
        );
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let mut state = PaletteState::new(vec![
            command(CommandKind::ShowOrHide, "Show or hide OxideZap", &[]),
            command(CommandKind::OpenSettings, "Open settings", &[]),
            command(CommandKind::Quit, "Quit OxideZap", &[]),
        ]);

        state.move_selection(1);
        assert_eq!(state.selected_match_index(), Some(1));
        state.move_selection(1);
        assert_eq!(state.selected_match_index(), Some(2));
        state.move_selection(1);
        assert_eq!(state.selected_match_index(), Some(0));
        state.move_selection(-1);
        assert_eq!(state.selected_match_index(), Some(2));
    }

    #[test]
    fn selection_resets_when_the_query_changes() {
        let mut state = PaletteState::new(vec![
            command(CommandKind::ShowOrHide, "Show or hide OxideZap", &[]),
            command(CommandKind::OpenSettings, "Open settings", &[]),
        ]);
        state.move_selection(1);
        assert_eq!(state.selected_match_index(), Some(1));

        state.set_query("settings");

        assert_eq!(state.query(), "settings");
        assert_eq!(state.matches(), &[1]);
        assert_eq!(state.selected_match_index(), Some(0));
        assert_eq!(
            state
                .selected_command()
                .map(|command| command.title.as_str()),
            Some("Open settings")
        );
    }

    #[test]
    fn empty_results_have_no_selection_and_ignore_navigation() {
        let mut state = PaletteState::new(vec![command(CommandKind::Quit, "Quit OxideZap", &[])]);
        state.set_query("zzzz");

        assert!(state.matches().is_empty());
        assert_eq!(state.selected_match_index(), None);
        assert_eq!(state.selected_command(), None);

        state.move_selection(1);
        state.move_selection(-1);

        assert_eq!(state.selected_match_index(), None);
        assert_eq!(state.selected_command(), None);
    }

    #[test]
    fn selection_stays_inside_a_narrowed_result_set() {
        let mut state = PaletteState::new(vec![
            command(CommandKind::ShowOrHide, "Show or hide OxideZap", &[]),
            command(CommandKind::OpenSettings, "Open settings", &[]),
            command(CommandKind::Quit, "Quit OxideZap", &[]),
        ]);
        state.move_selection(2);
        state.move_selection(2);

        state.set_query("settings");
        state.move_selection(1);
        state.move_selection(1);

        assert_eq!(state.matches(), &[1]);
        assert_eq!(state.selected_match_index(), Some(0));
        assert_eq!(
            state
                .selected_command()
                .map(|command| command.title.as_str()),
            Some("Open settings")
        );
    }

    #[test]
    fn builds_one_command_for_every_existing_action() {
        let commands = build_commands(&context());
        let kinds: Vec<CommandKind> = commands.iter().map(|command| command.kind).collect();

        for kind in [
            CommandKind::ShowOrHide,
            CommandKind::ShowAll,
            CommandKind::HideAll,
            CommandKind::OpenSettings,
            CommandKind::AddAccount,
            CommandKind::SwitchAccount,
            CommandKind::OpenDownloadsWindow,
            CommandKind::OpenDownloadsFolder,
            CommandKind::OpenStorageFolder,
            CommandKind::Lock,
            CommandKind::SetDoNotDisturb,
            CommandKind::SetTheme,
            CommandKind::ZoomIn,
            CommandKind::ZoomOut,
            CommandKind::ZoomReset,
            CommandKind::RetryConnection,
            CommandKind::Quit,
        ] {
            assert!(kinds.contains(&kind), "missing {kind:?}");
        }

        assert_eq!(
            find(&commands, CommandKind::ShowOrHide).action,
            CommandAction::ToggleVisibility
        );
        assert_eq!(
            find(&commands, CommandKind::OpenSettings).action,
            CommandAction::OpenSettings
        );
        assert_eq!(
            find(&commands, CommandKind::OpenDownloadsWindow).action,
            CommandAction::OpenDownloadsWindow
        );
        assert_eq!(
            find(&commands, CommandKind::OpenStorageFolder).action,
            CommandAction::OpenStorageFolder
        );
        assert_eq!(
            find(&commands, CommandKind::Quit).action,
            CommandAction::Quit
        );
        assert_eq!(
            commands.last().map(|command| command.kind),
            Some(CommandKind::Quit)
        );
    }

    #[test]
    fn account_switch_commands_carry_one_based_positions_and_ids() {
        let commands = build_commands(&context());
        let switches: Vec<&Command> = commands
            .iter()
            .filter(|command| command.kind == CommandKind::SwitchAccount)
            .collect();

        assert_eq!(switches.len(), 2);
        assert_eq!(switches[0].title, "Switch to account 1: Personal (current)");
        assert_eq!(
            switches[0].action,
            CommandAction::OpenAccount {
                index: 1,
                account_id: "a".to_string(),
            }
        );
        assert_eq!(switches[1].title, "Switch to account 2: Work");
        assert_eq!(
            switches[1].action,
            CommandAction::OpenAccount {
                index: 2,
                account_id: "b".to_string(),
            }
        );
    }

    #[test]
    fn do_not_disturb_command_offers_the_opposite_state() {
        let commands = build_commands(&context());
        let command = find(&commands, CommandKind::SetDoNotDisturb);
        assert_eq!(command.title, "Turn on Do Not Disturb");
        assert_eq!(command.action, CommandAction::SetDoNotDisturb(true));

        let mut enabled = context();
        enabled.do_not_disturb = true;
        let commands = build_commands(&enabled);
        let command = find(&commands, CommandKind::SetDoNotDisturb);
        assert_eq!(command.title, "Turn off Do Not Disturb");
        assert_eq!(command.action, CommandAction::SetDoNotDisturb(false));
    }

    #[test]
    fn lock_command_requires_a_configured_password() {
        let commands = build_commands(&context());
        assert!(
            commands
                .iter()
                .any(|command| command.kind == CommandKind::Lock)
        );

        let mut unlocked = context();
        unlocked.lock_available = false;
        let commands = build_commands(&unlocked);
        assert!(
            !commands
                .iter()
                .any(|command| command.kind == CommandKind::Lock)
        );
    }

    #[test]
    fn retry_command_only_appears_when_retry_is_available() {
        let commands = build_commands(&context());
        assert_eq!(
            find(&commands, CommandKind::RetryConnection).action,
            CommandAction::RetryConnection {
                account_id: "a".to_string(),
            }
        );

        let mut unavailable = context();
        unavailable.retry_available = false;
        let commands = build_commands(&unavailable);
        assert!(
            !commands
                .iter()
                .any(|command| command.kind == CommandKind::RetryConnection)
        );

        let mut without_account = context();
        without_account.retry_account = None;
        let commands = build_commands(&without_account);
        assert!(
            !commands
                .iter()
                .any(|command| command.kind == CommandKind::RetryConnection)
        );
    }

    #[test]
    fn theme_and_zoom_commands_map_to_actions() {
        let commands = build_commands(&context());
        let themes: Vec<&Command> = commands
            .iter()
            .filter(|command| command.kind == CommandKind::SetTheme)
            .collect();

        assert_eq!(themes.len(), 3);
        assert_eq!(themes[0].title, "Use light theme");
        assert_eq!(themes[0].action, CommandAction::SetTheme(ThemeMode::Light));
        assert_eq!(themes[1].title, "Use dark theme");
        assert_eq!(themes[1].action, CommandAction::SetTheme(ThemeMode::Dark));
        assert_eq!(themes[2].title, "Follow the system theme");
        assert_eq!(themes[2].action, CommandAction::SetTheme(ThemeMode::System));

        assert_eq!(
            find(&commands, CommandKind::ZoomIn).action,
            CommandAction::ZoomIn
        );
        assert_eq!(
            find(&commands, CommandKind::ZoomOut).action,
            CommandAction::ZoomOut
        );
        assert_eq!(
            find(&commands, CommandKind::ZoomReset).action,
            CommandAction::ZoomReset
        );
    }

    #[test]
    fn focus_account_after_maps_commands_that_keep_the_webview_relevant() {
        let active = Some("a".to_string());

        assert_eq!(
            CommandAction::OpenAccount {
                index: 2,
                account_id: "b".to_string(),
            }
            .focus_account_after(active.clone()),
            Some("b".to_string())
        );
        assert_eq!(
            CommandAction::RetryConnection {
                account_id: "c".to_string(),
            }
            .focus_account_after(active.clone()),
            Some("c".to_string())
        );
        assert_eq!(
            CommandAction::SetTheme(ThemeMode::Light).focus_account_after(active.clone()),
            active.clone()
        );
        assert_eq!(
            CommandAction::OpenSettings.focus_account_after(active.clone()),
            None
        );
        assert_eq!(CommandAction::HideAll.focus_account_after(active), None);
        assert_eq!(
            CommandAction::Quit.focus_account_after(Some("a".to_string())),
            None
        );
    }

    #[test]
    fn zoom_steps_follow_the_settings_presets() {
        assert_eq!(zoom_after(1.0, 1), 1.1);
        assert_eq!(zoom_after(1.0, -1), 0.9);
        assert_eq!(zoom_after(1.05, 1), 1.1);
        assert_eq!(zoom_after(0.8, -1), 0.8);
        assert_eq!(zoom_after(1.5, 1), 1.5);
    }

    #[test]
    fn digit_shortcuts_map_one_based_accounts() {
        assert_eq!(digit_account_index("1"), Some(0));
        assert_eq!(digit_account_index("9"), Some(8));
        assert_eq!(digit_account_index("0"), None);
        assert_eq!(digit_account_index("a"), None);
        assert_eq!(digit_account_index(""), None);

        let accounts = context().accounts;
        assert_eq!(
            account_at(&accounts, 0).map(|account| account.id.as_str()),
            Some("a")
        );
        assert_eq!(
            account_at(&accounts, 1).map(|account| account.id.as_str()),
            Some("b")
        );
        assert_eq!(account_at(&accounts, 2), None);
    }
}
