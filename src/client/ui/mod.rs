mod editor;
mod markdown;
mod session_browser;
mod view;

pub(super) use editor::{EditorOutcome, PromptEditor};
pub(super) use session_browser::{BrowserAction, BrowserMode, SessionBrowserState};
#[cfg(test)]
pub(super) use session_browser::{BrowserLayer, BrowserRow};
pub(super) use view::render;

const MOUSE_SCROLL_ROWS: usize = 3;

pub(super) fn fuzzy_subsequence_score(query: &str, candidate: &str) -> Option<usize> {
    if query == candidate {
        return Some(usize::MAX);
    }
    let mut candidate = candidate.char_indices();
    let mut previous = None;
    let mut score = 0_usize;
    for wanted in query.chars() {
        let (index, _) = candidate.find(|(_, character)| *character == wanted)?;
        score += 10;
        if previous.is_some_and(|previous| index == previous + 1) {
            score += 5;
        }
        previous = Some(index);
    }
    Some(score)
}

#[derive(Debug)]
pub(super) struct TranscriptScroll {
    top: usize,
    content_height: usize,
    viewport_height: u16,
    auto_follow: bool,
}

impl Default for TranscriptScroll {
    fn default() -> Self {
        Self {
            top: 0,
            content_height: 0,
            viewport_height: 0,
            auto_follow: true,
        }
    }
}

impl TranscriptScroll {
    fn max_top(&self) -> usize {
        self.content_height
            .saturating_sub(usize::from(self.viewport_height))
    }

    pub(super) fn update_metrics(&mut self, content_height: usize, viewport_height: u16) {
        self.content_height = content_height;
        self.viewport_height = viewport_height;
        self.top = if self.auto_follow {
            self.max_top()
        } else {
            self.top.min(self.max_top())
        };
    }

    pub(super) fn page_up(&mut self) {
        self.auto_follow = false;
        let step = usize::from(self.viewport_height.saturating_sub(1).max(1));
        self.top = self.top.saturating_sub(step);
    }

    pub(super) fn page_down(&mut self) {
        self.auto_follow = false;
        let step = usize::from(self.viewport_height.saturating_sub(1).max(1));
        self.top = self.top.saturating_add(step).min(self.max_top());
    }

    pub(super) fn wheel_up(&mut self) {
        self.auto_follow = false;
        self.top = self.top.saturating_sub(MOUSE_SCROLL_ROWS);
    }

    pub(super) fn wheel_down(&mut self) {
        self.auto_follow = false;
        self.top = self
            .top
            .saturating_add(MOUSE_SCROLL_ROWS)
            .min(self.max_top());
    }

    pub(super) fn follow_latest(&mut self) {
        self.auto_follow = true;
        self.top = self.max_top();
    }

    pub(super) const fn top(&self) -> usize {
        self.top
    }

    #[cfg(test)]
    pub(super) const fn auto_follow(&self) -> bool {
        self.auto_follow
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MenuKind {
    Commands,
    Models,
    Efforts,
    Processes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PopupKind {
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarPreference {
    Auto,
    Hide,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MenuItem {
    value: String,
    description: String,
}

impl MenuItem {
    pub(super) fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: sanitize_line(&value.into()),
            description: sanitize_line(&description.into()),
        }
    }
}

#[derive(Default)]
pub(super) struct MenuState {
    kind: Option<MenuKind>,
    items: Vec<MenuItem>,
    selected: usize,
}

impl MenuState {
    pub(super) fn set<I>(&mut self, kind: MenuKind, items: I)
    where
        I: IntoIterator<Item = MenuItem>,
    {
        self.kind = Some(kind);
        self.items = items.into_iter().collect();
        self.selected = 0;
    }

    pub(super) fn clear(&mut self) {
        self.kind = None;
        self.items.clear();
        self.selected = 0;
    }

    pub(super) fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub(super) fn select_previous(&mut self) {
        if !self.items.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    pub(super) fn selected_value(&self) -> Option<&str> {
        self.items
            .get(self.selected)
            .map(|item| item.value.as_str())
    }

    pub(super) const fn kind(&self) -> Option<MenuKind> {
        self.kind
    }

    pub(super) fn is_open(&self) -> bool {
        self.kind.is_some() && !self.items.is_empty()
    }
}

pub(super) fn sanitize_line(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut separator = false;
    for character in text.chars() {
        if matches!(character, '\n' | '\r' | '\t') {
            if !separator {
                sanitized.push(' ');
            }
            separator = true;
        } else if matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}') {
            continue;
        } else {
            sanitized.push(character);
            separator = false;
        }
    }
    sanitized
}

pub(super) struct UiState {
    editor: PromptEditor,
    scroll: TranscriptScroll,
    menu: MenuState,
    session_browser: SessionBrowserState,
    popup: Option<PopupKind>,
    sidebar_preference: SidebarPreference,
    sidebar_open: bool,
    frame_width: u16,
    notices: Vec<String>,
    local_error: bool,
    welcome_dismissed: bool,
    needs_redraw: bool,
}

impl UiState {
    pub(super) fn new() -> Self {
        Self {
            editor: PromptEditor::new(),
            scroll: TranscriptScroll::default(),
            menu: MenuState::default(),
            session_browser: SessionBrowserState::default(),
            popup: None,
            sidebar_preference: SidebarPreference::Auto,
            sidebar_open: false,
            frame_width: 0,
            notices: Vec::new(),
            local_error: false,
            welcome_dismissed: false,
            needs_redraw: true,
        }
    }

    pub(super) fn editor(&self) -> &PromptEditor {
        &self.editor
    }

    pub(super) fn editor_mut(&mut self) -> &mut PromptEditor {
        &mut self.editor
    }

    pub(super) fn scroll(&self) -> &TranscriptScroll {
        &self.scroll
    }

    pub(super) fn scroll_mut(&mut self) -> &mut TranscriptScroll {
        &mut self.scroll
    }

    pub(super) fn menu(&self) -> &MenuState {
        &self.menu
    }

    pub(super) fn menu_mut(&mut self) -> &mut MenuState {
        &mut self.menu
    }

    pub(super) fn session_browser(&self) -> &SessionBrowserState {
        &self.session_browser
    }

    pub(super) fn session_browser_mut(&mut self) -> &mut SessionBrowserState {
        &mut self.session_browser
    }

    pub(super) fn help_open(&self) -> bool {
        self.popup == Some(PopupKind::Help)
    }

    #[cfg(test)]
    pub(super) fn set_help_open(&mut self, open: bool) {
        self.set_popup(open.then_some(PopupKind::Help));
    }

    pub(super) const fn popup(&self) -> Option<PopupKind> {
        self.popup
    }

    pub(super) fn set_popup(&mut self, popup: Option<PopupKind>) {
        if popup.is_some() {
            self.menu.clear();
        }
        self.popup = popup;
        self.request_redraw();
    }

    pub(super) fn record_frame_width(&mut self, width: u16) {
        self.frame_width = width;
    }

    pub(super) fn sidebar_visible(&self, width: u16) -> bool {
        self.sidebar_open || (self.sidebar_preference == SidebarPreference::Auto && width > 120)
    }

    pub(super) fn toggle_sidebar(&mut self) {
        let visible = self.sidebar_visible(self.frame_width);
        self.sidebar_preference = if visible {
            SidebarPreference::Hide
        } else {
            SidebarPreference::Auto
        };
        self.sidebar_open = !visible;
        self.request_redraw();
    }

    pub(super) fn notices(&self) -> &[String] {
        &self.notices
    }

    pub(super) fn local_error(&self) -> bool {
        self.local_error
    }

    pub(super) fn welcome_dismissed(&self) -> bool {
        self.welcome_dismissed
    }

    #[cfg(test)]
    pub(super) fn dismiss_welcome(&mut self) {
        self.welcome_dismissed = true;
        self.request_redraw();
    }

    pub(super) fn push_notice(&mut self, notice: impl AsRef<str>) {
        self.notices.push(sanitize_line(notice.as_ref()));
        self.request_redraw();
    }

    pub(super) fn push_error(&mut self, notice: impl AsRef<str>) {
        self.local_error = true;
        self.push_notice(notice);
    }

    pub(super) fn clear_local_error(&mut self) {
        self.local_error = false;
        self.request_redraw();
    }

    pub(super) fn authoritative_reset(&mut self) {
        self.notices.clear();
        self.local_error = false;
        self.menu.clear();
        self.popup = None;
        self.welcome_dismissed = false;
        self.request_redraw();
    }

    pub(super) fn chat_transition_reset(&mut self) {
        self.editor.clear();
        self.scroll = TranscriptScroll::default();
        self.session_browser.close();
        self.authoritative_reset();
    }

    pub(super) fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    pub(super) fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.needs_redraw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_follow_tracks_bottom_and_manual_scroll_stays_stable() {
        let mut scroll = TranscriptScroll::default();
        scroll.update_metrics(30, 8);
        assert_eq!(scroll.top(), 22);
        assert!(scroll.auto_follow());

        scroll.page_up();
        assert_eq!(scroll.top(), 15);
        assert!(!scroll.auto_follow());

        scroll.update_metrics(40, 8);
        assert_eq!(scroll.top(), 15);

        scroll.follow_latest();
        assert_eq!(scroll.top(), 32);
        assert!(scroll.auto_follow());
    }

    #[test]
    fn page_and_wheel_steps_are_exact_and_clamped() {
        let mut scroll = TranscriptScroll::default();
        scroll.update_metrics(20, 5);
        scroll.page_up();
        assert_eq!(scroll.top(), 11);
        scroll.wheel_up();
        assert_eq!(scroll.top(), 8);
        scroll.wheel_down();
        assert_eq!(scroll.top(), 11);
        scroll.page_down();
        assert_eq!(scroll.top(), 15);
        scroll.page_down();
        assert_eq!(scroll.top(), 15);
    }

    #[test]
    fn menu_selection_wraps_and_replacement_selects_first_item() {
        let mut menu = MenuState::default();
        menu.set(
            MenuKind::Commands,
            [
                MenuItem::new("/quit", "Exit moh"),
                MenuItem::new("/model", "Change model"),
            ],
        );
        assert_eq!(menu.selected_value(), Some("/quit"));
        menu.select_previous();
        assert_eq!(menu.selected_value(), Some("/model"));
        menu.select_next();
        assert_eq!(menu.selected_value(), Some("/quit"));

        menu.set(
            MenuKind::Models,
            [MenuItem::new("gpt-5.6-terra", "Balanced model")],
        );
        assert_eq!(menu.selected_value(), Some("gpt-5.6-terra"));
    }

    #[test]
    fn authoritative_reset_clears_only_local_projection_state() {
        let mut ui = UiState::new();
        ui.editor_mut().set_value("keep me");
        ui.push_error("safe\x1b[2J error");
        ui.set_popup(Some(PopupKind::Help));
        ui.dismiss_welcome();
        ui.authoritative_reset();

        assert_eq!(ui.editor().value(), "keep me");
        assert!(ui.notices().is_empty());
        assert!(!ui.local_error());
        assert_eq!(ui.popup(), None);
        assert!(!ui.welcome_dismissed());
        assert!(ui.take_redraw());
    }

    #[test]
    fn chat_transition_reset_clears_session_local_state() {
        let mut ui = UiState::new();
        ui.editor_mut().set_value("discard me");
        ui.scroll_mut().update_metrics(30, 8);
        ui.scroll_mut().page_up();
        ui.menu_mut()
            .set(MenuKind::Commands, [MenuItem::new("/quit", "Exit moh")]);
        ui.session_browser_mut().open();
        ui.set_help_open(true);
        ui.push_error("discard notice");

        ui.chat_transition_reset();

        assert!(ui.editor().value().is_empty());
        assert_eq!(ui.scroll().top(), 0);
        assert!(ui.menu().kind().is_none());
        assert!(!ui.session_browser().is_open());
        assert!(!ui.help_open());
        assert!(ui.notices().is_empty());
        assert!(!ui.local_error());
    }
}
