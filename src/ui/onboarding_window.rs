use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, KeyDownEvent, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use crate::app::AppModel;
use crate::theme::Theme;
use crate::ui::components::{app_icon, button, card, divider, handle_focus_traversal};

/// Number of pages in the first-run guide. Kept between three and five so the
/// tour stays short enough to read on a fresh installation.
pub const PAGE_COUNT: usize = OnboardingPage::ALL.len();

/// The pages of the first-run guide, in presentation order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnboardingPage {
    #[default]
    Welcome,
    LinkPhone,
    Accounts,
    Privacy,
    Ready,
}

impl OnboardingPage {
    pub const ALL: [OnboardingPage; 5] = [
        OnboardingPage::Welcome,
        OnboardingPage::LinkPhone,
        OnboardingPage::Accounts,
        OnboardingPage::Privacy,
        OnboardingPage::Ready,
    ];

    pub fn index(self) -> usize {
        match self {
            OnboardingPage::Welcome => 0,
            OnboardingPage::LinkPhone => 1,
            OnboardingPage::Accounts => 2,
            OnboardingPage::Privacy => 3,
            OnboardingPage::Ready => 4,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            OnboardingPage::Welcome => t!("Welcome to OxideZap"),
            OnboardingPage::LinkPhone => t!("Link your phone"),
            OnboardingPage::Accounts => t!("Accounts and data"),
            OnboardingPage::Privacy => t!("Windows and privacy"),
            OnboardingPage::Ready => t!("You're ready"),
        }
    }

    /// The next page, clamped at the last one. Navigation never runs past the
    /// ends of the guide, so a stray click cannot index outside `ALL`.
    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1).min(Self::ALL.len() - 1)]
    }

    /// The previous page, clamped at the first one.
    pub fn back(self) -> Self {
        Self::ALL[self.index().saturating_sub(1)]
    }

    pub fn is_last(self) -> bool {
        self == OnboardingPage::Ready
    }
}

/// Bounded page cursor shared with the tests. Rendering only reads `page`;
/// the window mutates it through `next` and `back`.
#[derive(Clone, Debug, Default)]
pub struct OnboardingState {
    page: OnboardingPage,
}

impl OnboardingState {
    pub fn page(&self) -> OnboardingPage {
        self.page
    }

    pub fn next(&mut self) {
        self.page = self.page.next();
    }

    pub fn back(&mut self) {
        self.page = self.page.back();
    }

    pub fn is_last(&self) -> bool {
        self.page.is_last()
    }
}

struct Locations {
    config_dir: String,
    accounts_dir: String,
    downloads_dir: String,
}

pub struct OnboardingWindow {
    model: Entity<AppModel>,
    state: OnboardingState,
    focus_handle: FocusHandle,
    appearance_subscription: Option<Subscription>,
}

impl OnboardingWindow {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        Self {
            model,
            state: OnboardingState::default(),
            focus_handle: cx.focus_handle(),
            appearance_subscription: None,
        }
    }

    fn advance(&mut self, cx: &mut Context<Self>) {
        if self.state.is_last() {
            self.open_whatsapp(cx);
            return;
        }
        self.state.next();
        cx.notify();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        self.state.back();
        cx.notify();
    }

    /// The final button opens WhatsApp Web and returns focus to the account
    /// window. Skip and Escape record completion without borrowing focus.
    fn open_whatsapp(&mut self, cx: &mut Context<Self>) {
        self.model
            .update(cx, |model, cx| model.finish_onboarding(cx));
    }

    fn skip(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.skip_onboarding(cx));
    }

    fn handle_escape(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.skip(cx);
        }
    }
}

impl Focusable for OnboardingWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OnboardingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            window.focus(&self.focus_handle);
        }
        window.set_window_title(t!("Welcome to OxideZap"));

        // The theme is resolved from the window appearance on every render, so
        // a repaint is enough when the system appearance changes. Register once
        // per view; a recreated window gets a new view and a new subscription.
        if self.appearance_subscription.is_none() {
            self.appearance_subscription =
                Some(cx.observe_window_appearance(window, |_this, _window, cx| cx.notify()));
        }

        let (theme, locations) = {
            let model = self.model.read(cx);
            (
                model.theme(window),
                Locations {
                    config_dir: model.paths.config_dir.display().to_string(),
                    accounts_dir: model.paths.accounts_dir.display().to_string(),
                    downloads_dir: model.paths.downloads_dir.display().to_string(),
                },
            )
        };

        let page = self.state.page();
        let is_last = self.state.is_last();
        let is_first = page == OnboardingPage::Welcome;

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.))
            .bg(theme.bg)
            .track_focus(&self.focus_handle)
            .on_key_down(handle_focus_traversal)
            .on_key_down(cx.listener(Self::handle_escape))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(660.))
                    .h_full()
                    .max_h(px(580.))
                    .rounded_lg()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .overflow_hidden()
                    .child(header(page, theme))
                    .child(progress(page, theme))
                    .child(
                        div()
                            .id("onboarding-content")
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_y_scroll()
                            .p(px(24.))
                            .child(page_content(page, theme, &locations)),
                    )
                    .child(footer(theme, is_last, is_first, cx)),
            )
    }
}

fn header(page: OnboardingPage, theme: Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .px(px(24.))
        .pt(px(22.))
        .pb(px(14.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(app_icon(34.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.text_faint)
                                .child(t!("FIRST-RUN GUIDE")),
                        )
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(page.title()),
                        ),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.text_faint)
                .child(tr!(
                    "Step {step} of {total}",
                    step = page.index() + 1,
                    total = PAGE_COUNT
                )),
        )
        .into_any_element()
}

fn progress(page: OnboardingPage, theme: Theme) -> AnyElement {
    let mut bar = div().flex().gap(px(6.)).px(px(24.)).pb(px(16.));
    for candidate in OnboardingPage::ALL {
        let reached = candidate.index() <= page.index();
        bar = bar.child(div().h(px(4.)).flex_1().rounded_full().bg(if reached {
            theme.accent
        } else {
            theme.border
        }));
    }
    bar.into_any_element()
}

fn page_content(page: OnboardingPage, theme: Theme, locations: &Locations) -> AnyElement {
    let body = div().flex().flex_col().gap(px(14.));
    match page {
        OnboardingPage::Welcome => body
            .child(paragraph(
                t!(
                    "OxideZap wraps the official WhatsApp Web in a native Linux window. \
                 It is an independent, unofficial project, not affiliated with or \
                 endorsed by WhatsApp or Meta."
                ),
                theme,
            ))
            .child(bullet(
                t!("The official login"),
                t!(
                    "WhatsApp keeps handling the account, the QR login, and your messages. \
                 OxideZap only provides the desktop window around it."
                ),
                theme,
            ))
            .child(bullet(
                t!("No telemetry"),
                t!(
                    "This guide makes no network requests. The app collects no analytics and \
                 never asks for your phone number or WhatsApp credentials."
                ),
                theme,
            ))
            .child(path_card(
                t!("Your session stays on this computer"),
                &[(t!("Settings"), locations.config_dir.as_str())],
                theme,
            )),
        OnboardingPage::LinkPhone => body
            .child(paragraph(
                t!(
                    "The account window behind this guide shows WhatsApp Web's QR code. \
                 Keep the guide open while you pick up your phone, or skip it and come back \
                 from Settings > General later."
                ),
                theme,
            ))
            .child(step(1, t!("Open WhatsApp on your phone."), theme))
            .child(step(
                2,
                t!("Tap Linked devices, then Link a device."),
                theme,
            ))
            .child(step(
                3,
                t!("Point the phone at the QR code in the OxideZap window."),
                theme,
            ))
            .child(warning_card(
                t!("QR code does not load?"),
                t!(
                    "It can take a few seconds. If the page stays blank, check that \
                 WebKitGTK 2.46.1 or newer is installed; older versions can hang the QR \
                 login. OxideZap never waits for the QR before showing this guide."
                ),
                theme,
            )),
        OnboardingPage::Accounts => body
            .child(paragraph(
                t!(
                    "Every account gets its own window and its own WebKit profile, so two \
                 WhatsApp accounts never share cookies, local storage, or caches."
                ),
                theme,
            ))
            .child(bullet(
                t!("Account isolation"),
                t!(
                    "Log in to a second number from Settings > Accounts > Add account. \
                 Removing an account also removes its local profile."
                ),
                theme,
            ))
            .child(path_card(
                t!("Where your data lives"),
                &[
                    (t!("Config"), locations.config_dir.as_str()),
                    (t!("Account data"), locations.accounts_dir.as_str()),
                    (t!("Downloads"), locations.downloads_dir.as_str()),
                ],
                theme,
            ))
            .child(paragraph(
                t!(
                    "OxideZap creates its files with private 0600/0700 permissions. \
                 The download folder stays shared with your desktop."
                ),
                theme,
            )),
        OnboardingPage::Privacy => body
            .child(bullet(
                t!("Close to tray"),
                t!(
                    "Closing an account window hides it instead of quitting. Reopen it from \
                 the tray icon or the global show/hide shortcut; turn this off in \
                 Settings > General."
                ),
                theme,
            ))
            .child(bullet(
                t!("Native notifications"),
                t!(
                    "Message previews use your desktop's notification service. Toggle them \
                 globally, mute one account, or turn on Do Not Disturb in \
                 Settings > Notifications."
                ),
                theme,
            ))
            .child(bullet(
                t!("App lock"),
                t!(
                    "Set a password in Settings > Security to hide every window behind a \
                 privacy screen: lock now, on launch, on hide, on suspend, when the \
                 desktop locks, or after an idle timeout."
                ),
                theme,
            ))
            .child(bullet(
                t!("Global shortcuts"),
                t!(
                    "The default show/hide shortcut is Ctrl+Shift+W. The boss key, \
                 Ctrl+Shift+H, hides every window immediately for privacy and restores \
                 them on the next press."
                ),
                theme,
            )),
        OnboardingPage::Ready => body
            .child(paragraph(
                t!(
                    "That is everything. Your settings are saved as you change them, and \
                 you can reopen this guide any time from Settings > General."
                ),
                theme,
            ))
            .child(bullet(
                t!("View-once messages"),
                t!(
                    "WhatsApp only lets you open view-once media on your phone. OxideZap does \
                 not bypass that restriction; the desktop window follows WhatsApp Web's \
                 own rules."
                ),
                theme,
            ))
            .child(bullet(
                t!("Single-phone account"),
                t!(
                    "A WhatsApp account stays linked to your phone. OxideZap does not run a \
                 second copy of the account and cannot receive messages while the phone \
                 is offline, just like WhatsApp Web."
                ),
                theme,
            ))
            .child(path_card(
                t!("Where your data lives"),
                &[
                    (t!("Settings"), locations.config_dir.as_str()),
                    (t!("Accounts"), locations.accounts_dir.as_str()),
                ],
                theme,
            ))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_faint)
                    .child(t!(
                        "Prefer the full walkthrough? The README documents the same steps."
                    )),
            )
            .child(div().flex().gap(px(8.)).child(button(
                "onboarding-read-guide",
                t!("Read the guide"),
                theme,
                false,
                |_event, _window, _cx| {
                    crate::links::open_external(
                        "https://github.com/artur282/oxidezap#first-run-guide",
                    );
                },
            ))),
    }
    .into_any_element()
}

fn footer(
    theme: Theme,
    is_last: bool,
    is_first: bool,
    cx: &mut Context<OnboardingWindow>,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .px(px(24.))
        .py(px(16.))
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(button(
                    "onboarding-skip",
                    t!("Skip"),
                    theme,
                    false,
                    cx.listener(|this, _event, _window, cx| {
                        this.skip(cx);
                    }),
                ))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.text_faint)
                        .child(t!("Skipping hides this guide permanently.")),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .when(!is_first, |style| {
                    style.child(button(
                        "onboarding-back",
                        t!("Back"),
                        theme,
                        false,
                        cx.listener(|this, _event, _window, cx| {
                            this.go_back(cx);
                        }),
                    ))
                })
                .child(button(
                    "onboarding-next",
                    if is_last {
                        t!("Open WhatsApp Web")
                    } else {
                        t!("Next")
                    },
                    theme,
                    true,
                    cx.listener(|this, _event, _window, cx| {
                        this.advance(cx);
                    }),
                )),
        )
        .into_any_element()
}

fn paragraph(text: impl Into<SharedString>, theme: Theme) -> AnyElement {
    div()
        .text_size(px(13.))
        .text_color(theme.text_muted)
        .child(text.into())
        .into_any_element()
}

fn bullet(title: &'static str, description: impl Into<SharedString>, theme: Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(div().size(px(6.)).rounded_full().bg(theme.accent))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .child(description.into()),
                ),
        )
        .into_any_element()
}

fn step(number: u32, description: &'static str, theme: Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.))
                .rounded_full()
                .bg(theme.surface_alt)
                .text_size(px(11.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.accent)
                .child(number.to_string()),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.text)
                .child(description),
        )
        .into_any_element()
}

fn path_card(title: &'static str, rows: &[(&'static str, &str)], theme: Theme) -> AnyElement {
    let mut card = card(theme).child(
        div()
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted)
            .child(title),
    );
    for (label, value) in rows {
        card = card.child(divider(theme)).child(
            div()
                .flex()
                .justify_between()
                .gap(px(16.))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.text_faint)
                        .child(*label),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text)
                        .child((*value).to_string()),
                ),
        );
    }
    card.into_any_element()
}

fn warning_card(title: &'static str, description: &'static str, theme: Theme) -> AnyElement {
    card(theme)
        .border_color(theme.accent)
        .child(
            div()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.text_muted)
                .child(description),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_is_bounded_at_both_ends() {
        let mut state = OnboardingState::default();
        assert_eq!(state.page(), OnboardingPage::Welcome);

        state.back();
        assert_eq!(state.page(), OnboardingPage::Welcome);

        for expected in OnboardingPage::ALL.iter().skip(1) {
            state.next();
            assert_eq!(state.page(), *expected);
        }
        assert!(state.is_last());

        state.next();
        assert_eq!(state.page(), OnboardingPage::Ready);
        assert!(state.is_last());
    }

    #[test]
    fn next_and_back_move_one_page_at_a_time() {
        let mut state = OnboardingState::default();
        state.next();
        state.next();
        assert_eq!(state.page(), OnboardingPage::Accounts);

        state.back();
        assert_eq!(state.page(), OnboardingPage::LinkPhone);
    }

    #[test]
    fn the_guide_has_between_three_and_five_pages() {
        assert!((3..=5).contains(&PAGE_COUNT));
        assert_eq!(PAGE_COUNT, OnboardingPage::ALL.len());
    }
}
