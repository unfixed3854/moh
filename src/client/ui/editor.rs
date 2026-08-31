use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn sanitize_editor_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                sanitized.push('\n');
            }
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push(' '),
            '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' => {}
            printable => sanitized.push(printable),
        }
    }
    sanitized
}

#[derive(Debug, Eq, PartialEq)]
pub enum EditorOutcome {
    Ignored,
    Consumed,
    Changed,
    Submitted(String),
}

pub struct EditorRows {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_column: u16,
}

pub struct EditorWindow {
    pub(crate) text: String,
    pub(crate) cursor_column: u16,
}

#[derive(Default)]
pub struct PromptEditor {
    value: String,
    cursor_grapheme: usize,
    viewport_top: usize,
    preferred_column: Option<usize>,
    last_width: Option<u16>,
}

struct GraphemeSpan {
    start: usize,
    end: usize,
    is_whitespace: bool,
}

struct VisualRow {
    start: usize,
    end: usize,
}

impl PromptEditor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(crate) fn set_value(&mut self, value: impl Into<String>) {
        self.value = sanitize_editor_text(&value.into());
        self.cursor_grapheme = self.grapheme_count();
        self.reset_vertical_state();
    }

    pub(crate) fn clear(&mut self) {
        self.value.clear();
        self.cursor_grapheme = 0;
        self.reset_vertical_state();
    }

    pub(crate) fn at_end(&self) -> bool {
        self.cursor_grapheme == self.grapheme_count()
    }

    pub(crate) fn at_final_line_end(&self) -> bool {
        self.at_end()
    }

    fn grapheme_count(&self) -> usize {
        UnicodeSegmentation::graphemes(self.value.as_str(), true).count()
    }

    fn is_whitespace(grapheme: &str) -> bool {
        grapheme.chars().all(char::is_whitespace)
    }

    fn grapheme_spans(&self) -> Vec<GraphemeSpan> {
        let mut spans = UnicodeSegmentation::grapheme_indices(self.value.as_str(), true)
            .map(|(start, grapheme)| GraphemeSpan {
                start,
                end: start + grapheme.len(),
                is_whitespace: Self::is_whitespace(grapheme),
            })
            .collect::<Vec<_>>();
        if let Some(last) = spans.last_mut() {
            last.end = self.value.len();
        }
        spans
    }

    fn cursor_byte_index(&self) -> usize {
        UnicodeSegmentation::grapheme_indices(self.value.as_str(), true)
            .nth(self.cursor_grapheme)
            .map_or(self.value.len(), |(index, _)| index)
    }

    fn rows(&self, width: u16) -> Vec<VisualRow> {
        let width = usize::from(width.max(1));
        let graphemes =
            UnicodeSegmentation::graphemes(self.value.as_str(), true).collect::<Vec<_>>();
        let mut rows = Vec::new();
        let mut start = 0;
        let mut column: usize = 0;
        for (index, grapheme) in graphemes.iter().enumerate() {
            if *grapheme == "\n" {
                rows.push(VisualRow { start, end: index });
                start = index + 1;
                column = 0;
                continue;
            }
            let grapheme_width = UnicodeWidthStr::width(*grapheme);
            if column > 0 && column.saturating_add(grapheme_width) > width {
                rows.push(VisualRow { start, end: index });
                start = index;
                column = 0;
            }
            column = column.saturating_add(grapheme_width);
        }
        rows.push(VisualRow {
            start,
            end: graphemes.len(),
        });
        rows
    }

    fn cursor_row_and_column(&self, rows: &[VisualRow]) -> (usize, usize) {
        let row_index = rows
            .iter()
            .position(|row| row.start <= self.cursor_grapheme && self.cursor_grapheme <= row.end)
            .unwrap_or_else(|| rows.len().saturating_sub(1));
        let row = &rows[row_index];
        let spans = self.grapheme_spans();
        let start = spans
            .get(row.start)
            .map_or(self.value.len(), |span| span.start);
        (
            row_index,
            UnicodeWidthStr::width(&self.value[start..self.cursor_byte_index()]),
        )
    }

    fn reset_vertical_state(&mut self) {
        self.viewport_top = 0;
        self.preferred_column = None;
    }

    fn insert(&mut self, text: &str) {
        let text = sanitize_editor_text(text);
        if text.is_empty() {
            return;
        }
        let index = self.cursor_byte_index();
        self.value.insert_str(index, &text);
        let insertion_end = index + text.len();
        self.cursor_grapheme = UnicodeSegmentation::grapheme_indices(self.value.as_str(), true)
            .take_while(|(start, _)| *start < insertion_end)
            .count();
        self.reset_vertical_state();
    }

    fn delete_at_cursor(&mut self) -> bool {
        let Some((start, grapheme)) =
            UnicodeSegmentation::grapheme_indices(self.value.as_str(), true)
                .nth(self.cursor_grapheme)
        else {
            return false;
        };
        self.value.replace_range(start..start + grapheme.len(), "");
        self.reset_vertical_state();
        true
    }

    fn delete_before_cursor(&mut self) -> bool {
        if self.cursor_grapheme == 0 {
            return false;
        }
        self.cursor_grapheme -= 1;
        self.delete_at_cursor()
    }

    fn move_word_left(&mut self) -> bool {
        let original = self.cursor_grapheme;
        let spans = self.grapheme_spans();
        while self.cursor_grapheme > 0 && spans[self.cursor_grapheme - 1].is_whitespace {
            self.cursor_grapheme -= 1;
        }
        while self.cursor_grapheme > 0 && !spans[self.cursor_grapheme - 1].is_whitespace {
            self.cursor_grapheme -= 1;
        }
        self.cursor_grapheme != original
    }

    fn move_word_right(&mut self) -> bool {
        let original = self.cursor_grapheme;
        let spans = self.grapheme_spans();
        let end = spans.len();
        if self.cursor_grapheme < end {
            if spans[self.cursor_grapheme].is_whitespace {
                while self.cursor_grapheme < end && spans[self.cursor_grapheme].is_whitespace {
                    self.cursor_grapheme += 1;
                }
            } else {
                while self.cursor_grapheme < end && !spans[self.cursor_grapheme].is_whitespace {
                    self.cursor_grapheme += 1;
                }
                while self.cursor_grapheme < end && spans[self.cursor_grapheme].is_whitespace {
                    self.cursor_grapheme += 1;
                }
            }
        }
        self.cursor_grapheme != original
    }

    fn delete_word_before_cursor(&mut self) -> bool {
        let spans = self.grapheme_spans();
        let end = self.cursor_grapheme;
        let mut start = end;
        while start > 0 && spans[start - 1].is_whitespace {
            start -= 1;
        }
        while start > 0 && !spans[start - 1].is_whitespace {
            start -= 1;
        }
        if start == end {
            return false;
        }
        let start_byte = spans[start].start;
        let end_byte = spans.get(end).map_or(self.value.len(), |span| span.start);
        self.value.replace_range(start_byte..end_byte, "");
        self.cursor_grapheme = start;
        self.reset_vertical_state();
        true
    }

    fn delete_word_at_cursor(&mut self) -> bool {
        let spans = self.grapheme_spans();
        let end = spans.len();
        let start = self.cursor_grapheme;
        if start == end {
            return false;
        }
        let mut range_end = start;
        while range_end < end && spans[range_end].is_whitespace {
            range_end += 1;
        }
        while range_end < end && !spans[range_end].is_whitespace {
            range_end += 1;
        }
        while range_end < end && spans[range_end].is_whitespace {
            range_end += 1;
        }
        if start == range_end {
            return false;
        }
        let start_byte = spans[start].start;
        let end_byte = spans
            .get(range_end)
            .map_or(self.value.len(), |span| span.start);
        self.value.replace_range(start_byte..end_byte, "");
        self.reset_vertical_state();
        true
    }

    fn line_start(&self) -> usize {
        UnicodeSegmentation::graphemes(self.value.as_str(), true)
            .take(self.cursor_grapheme)
            .enumerate()
            .filter_map(|(index, grapheme)| (grapheme == "\n").then_some(index + 1))
            .last()
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        UnicodeSegmentation::graphemes(self.value.as_str(), true)
            .enumerate()
            .skip(self.cursor_grapheme)
            .find_map(|(index, grapheme)| (grapheme == "\n").then_some(index))
            .unwrap_or_else(|| self.grapheme_count())
    }

    fn move_vertical(&mut self, direction: i8) -> bool {
        let rows = self.rows(self.last_width.unwrap_or(u16::MAX));
        let (current, column) = self.cursor_row_and_column(&rows);
        let target = if direction.is_negative() {
            current.checked_sub(1)
        } else {
            current.checked_add(1).filter(|target| *target < rows.len())
        };
        let Some(target) = target else {
            return false;
        };
        let desired = self.preferred_column.unwrap_or(column);
        let target_row = &rows[target];
        let graphemes =
            UnicodeSegmentation::graphemes(self.value.as_str(), true).collect::<Vec<_>>();
        let mut index = target_row.start;
        let mut target_column: usize = 0;
        while index < target_row.end {
            let width = UnicodeWidthStr::width(graphemes[index]);
            if target_column.saturating_add(width) > desired {
                break;
            }
            target_column = target_column.saturating_add(width);
            index += 1;
        }
        self.cursor_grapheme = index;
        self.preferred_column = Some(desired);
        true
    }

    fn has_disallowed_modifiers(modifiers: KeyModifiers) -> bool {
        modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::ALT)
    }

    fn exact_control(modifiers: KeyModifiers) -> bool {
        modifiers.contains(KeyModifiers::CONTROL)
            && !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
    }

    pub(crate) fn handle_event(&mut self, event: &Event) -> EditorOutcome {
        let Event::Key(key) = event else {
            if matches!(event, Event::Paste(_)) {
                let Event::Paste(paste) = event else {
                    unreachable!()
                };
                let paste = sanitize_editor_text(paste);
                if paste.is_empty() {
                    return EditorOutcome::Consumed;
                }
                self.insert(&paste);
                return EditorOutcome::Changed;
            }
            return EditorOutcome::Ignored;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return EditorOutcome::Ignored;
        }

        let modifiers = key.modifiers;
        if Self::exact_control(modifiers) {
            let word_result = match key.code {
                KeyCode::Left => Some(self.move_word_left()),
                KeyCode::Right => Some(self.move_word_right()),
                KeyCode::Backspace | KeyCode::Char('h') => Some(self.delete_word_before_cursor()),
                KeyCode::Delete => Some(self.delete_word_at_cursor()),
                _ => None,
            };
            if let Some(changed) = word_result {
                return if changed {
                    EditorOutcome::Changed
                } else {
                    EditorOutcome::Consumed
                };
            }
        }
        if Self::has_disallowed_modifiers(modifiers) {
            return EditorOutcome::Ignored;
        }

        match key.code {
            KeyCode::Char(character) => {
                let mut encoded = [0; 4];
                let character = sanitize_editor_text(character.encode_utf8(&mut encoded));
                if character.is_empty() {
                    EditorOutcome::Consumed
                } else {
                    self.insert(&character);
                    EditorOutcome::Changed
                }
            }
            KeyCode::Enter if modifiers == KeyModifiers::SHIFT => {
                self.insert("\n");
                EditorOutcome::Changed
            }
            KeyCode::Enter => {
                let submitted = std::mem::take(&mut self.value);
                self.cursor_grapheme = 0;
                self.reset_vertical_state();
                EditorOutcome::Submitted(submitted)
            }
            KeyCode::Esc => EditorOutcome::Consumed,
            KeyCode::Left => {
                if self.cursor_grapheme == 0 {
                    EditorOutcome::Consumed
                } else {
                    self.cursor_grapheme -= 1;
                    self.reset_vertical_state();
                    EditorOutcome::Changed
                }
            }
            KeyCode::Right => {
                if self.at_end() {
                    EditorOutcome::Consumed
                } else {
                    self.cursor_grapheme += 1;
                    self.reset_vertical_state();
                    EditorOutcome::Changed
                }
            }
            KeyCode::Home => {
                let start = self.line_start();
                if self.cursor_grapheme == start {
                    EditorOutcome::Consumed
                } else {
                    self.cursor_grapheme = start;
                    self.reset_vertical_state();
                    EditorOutcome::Changed
                }
            }
            KeyCode::End => {
                let end = self.line_end();
                if self.cursor_grapheme == end {
                    EditorOutcome::Consumed
                } else {
                    self.cursor_grapheme = end;
                    self.reset_vertical_state();
                    EditorOutcome::Changed
                }
            }
            KeyCode::Up => {
                if self.move_vertical(-1) {
                    EditorOutcome::Changed
                } else {
                    EditorOutcome::Consumed
                }
            }
            KeyCode::Down => {
                if self.move_vertical(1) {
                    EditorOutcome::Changed
                } else {
                    EditorOutcome::Consumed
                }
            }
            KeyCode::Backspace => {
                if self.delete_before_cursor() {
                    EditorOutcome::Changed
                } else {
                    EditorOutcome::Consumed
                }
            }
            KeyCode::Delete => {
                if self.delete_at_cursor() {
                    EditorOutcome::Changed
                } else {
                    EditorOutcome::Consumed
                }
            }
            _ => EditorOutcome::Ignored,
        }
    }

    pub(crate) fn visual_height(&self, width: u16) -> u16 {
        self.rows(width).len().try_into().unwrap_or(u16::MAX)
    }

    pub(crate) fn display_rows(&mut self, width: u16, visible_height: u16) -> EditorRows {
        if width == 0 || visible_height == 0 {
            return EditorRows {
                lines: Vec::new(),
                cursor_row: 0,
                cursor_column: 0,
            };
        }
        self.last_width = Some(width);
        let rows = self.rows(width);
        let (cursor, cursor_column) = self.cursor_row_and_column(&rows);
        let visible_height = usize::from(visible_height);
        if cursor < self.viewport_top {
            self.viewport_top = cursor;
        } else if cursor >= self.viewport_top.saturating_add(visible_height) {
            self.viewport_top = cursor + 1 - visible_height;
        }
        let graphemes =
            UnicodeSegmentation::graphemes(self.value.as_str(), true).collect::<Vec<_>>();
        let lines = rows
            .iter()
            .skip(self.viewport_top)
            .take(visible_height)
            .map(|row| graphemes[row.start..row.end].concat())
            .collect();
        EditorRows {
            lines,
            cursor_row: cursor.saturating_sub(self.viewport_top) as u16,
            cursor_column: cursor_column.min(usize::from(width.saturating_sub(1))) as u16,
        }
    }

    pub(crate) fn display_window(&mut self, width: u16) -> EditorWindow {
        if width == 0 {
            return EditorWindow {
                text: String::new(),
                cursor_column: 0,
            };
        }

        let graphemes =
            UnicodeSegmentation::graphemes(self.value.as_str(), true).collect::<Vec<_>>();
        let cursor = graphemes[..self.cursor_grapheme]
            .iter()
            .map(|grapheme| UnicodeWidthStr::width(*grapheme))
            .sum::<usize>();
        let width = usize::from(width);
        let start = cursor.saturating_sub(width.saturating_sub(1));
        let end = start.saturating_add(width);
        let mut text = String::new();
        let mut column = 0;
        for grapheme in graphemes {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if column >= start && column.saturating_add(grapheme_width) <= end {
                text.push_str(grapheme);
            }
            column = column.saturating_add(grapheme_width);
        }
        EditorWindow {
            text,
            cursor_column: cursor.saturating_sub(start).min(width - 1) as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn display_window_keeps_the_hardware_cursor_inside_the_available_width() {
        let mut editor = PromptEditor::new();
        editor.set_value("0123456789");
        let window = editor.display_window(4);
        assert!(UnicodeWidthStr::width(window.text.as_str()) <= 4);
        assert!(window.cursor_column < 4);
        assert!(window.text.ends_with("789"));
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn modified(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn editor_edits_and_submits_whole_graphemes() {
        let mut editor = PromptEditor::new();
        assert_eq!(
            editor.handle_event(&Event::Paste("a👩‍💻b".into())),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Left)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Backspace)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "ab");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Enter)),
            EditorOutcome::Submitted("ab".into())
        );
        assert_eq!(editor.value(), "");
    }

    #[test]
    fn shift_enter_inserts_a_newline_and_enter_submits_the_complete_prompt() {
        let mut editor = PromptEditor::new();
        editor.handle_event(&Event::Paste("first".into()));
        assert_eq!(
            editor.handle_event(&modified(KeyCode::Enter, KeyModifiers::SHIFT)),
            EditorOutcome::Changed
        );
        editor.handle_event(&Event::Paste("second".into()));
        assert_eq!(editor.value(), "first\nsecond");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Enter)),
            EditorOutcome::Submitted("first\nsecond".into())
        );
    }

    #[test]
    fn multiline_paste_normalizes_line_endings_and_controls() {
        let mut editor = PromptEditor::new();
        editor.handle_event(&Event::Paste("one\r\ntwo\rthree\t\x1b[2Jfour".into()));
        assert_eq!(editor.value(), "one\ntwo\nthree [2Jfour");
    }

    #[test]
    fn up_and_down_move_between_multiline_rows() {
        let mut editor = PromptEditor::new();
        editor.set_value("first\nsecond");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Up)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Delete)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "firstsecond");
    }

    #[test]
    fn display_rows_scrolls_and_tracks_the_cursor_across_soft_wraps() {
        let mut editor = PromptEditor::new();
        editor.set_value("a\nb\nc\nd\ne");
        let rows = editor.display_rows(2, 2);
        assert_eq!(rows.lines, ["d", "e"]);
        assert_eq!(rows.cursor_row, 1);
        assert_eq!(rows.cursor_column, 1);

        editor.set_value("abcd");
        let rows = editor.display_rows(2, 2);
        assert_eq!(rows.lines, ["ab", "cd"]);
        assert_eq!(rows.cursor_row, 1);
        assert_eq!(
            editor.handle_event(&key(KeyCode::Up)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.display_rows(2, 2).cursor_row, 0);
    }

    #[test]
    fn home_and_end_stay_within_the_current_logical_line() {
        let mut editor = PromptEditor::new();
        editor.set_value("first\nsecond");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Home)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Delete)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "first\necond");
        assert_eq!(
            editor.handle_event(&key(KeyCode::End)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Backspace)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "first\necon");
    }

    #[test]
    fn vertical_movement_preserves_the_preferred_column_across_short_rows() {
        let mut editor = PromptEditor::new();
        editor.set_value("abcd\nx\nwxyz");
        editor.display_rows(4, 3);
        assert_eq!(
            editor.handle_event(&key(KeyCode::Up)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.display_rows(4, 3).cursor_column, 1);
        assert_eq!(
            editor.handle_event(&key(KeyCode::Up)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.display_rows(4, 3).cursor_column, 3);
        assert_eq!(
            editor.handle_event(&key(KeyCode::Down)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Down)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.display_rows(4, 3).cursor_column, 3);
    }

    #[test]
    fn a_wide_grapheme_wraps_without_escaping_the_available_width() {
        let mut editor = PromptEditor::new();
        editor.set_value("ab界");
        let rows = editor.display_rows(3, 2);
        assert_eq!(rows.lines, ["ab", "界"]);
        assert_eq!(rows.cursor_row, 1);
        assert!(rows.cursor_column < 3);
    }

    #[test]
    fn editor_preserves_word_shortcuts_and_normalizes_paste() {
        let mut editor = PromptEditor::new();
        editor.handle_event(&Event::Paste("one  two\nthree\x1b[2J".into()));
        let ctrl_left = Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        let ctrl_backspace = Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(editor.handle_event(&ctrl_left), EditorOutcome::Changed);
        assert_eq!(editor.handle_event(&ctrl_backspace), EditorOutcome::Changed);
        assert_eq!(editor.value(), "one  three[2J");
    }

    #[test]
    fn display_rows_keeps_the_hardware_cursor_inside_the_available_width() {
        let mut editor = PromptEditor::new();
        editor.set_value("0123456789");
        let rows = editor.display_rows(4, 1);
        assert!(unicode_width::UnicodeWidthStr::width(rows.lines[0].as_str()) <= 4);
        assert!(rows.cursor_column < 4);
        assert_eq!(rows.lines, ["89"]);
    }

    #[test]
    fn editor_navigation_and_deletion_stop_at_grapheme_boundaries() {
        let mut editor = PromptEditor::new();
        editor.set_value("a👩‍💻界");

        assert_eq!(
            editor.handle_event(&key(KeyCode::Home)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Left)),
            EditorOutcome::Consumed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Delete)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "👩‍💻界");
        assert_eq!(
            editor.handle_event(&key(KeyCode::End)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Right)),
            EditorOutcome::Consumed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Backspace)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "👩‍💻");
    }

    #[test]
    fn editor_word_shortcuts_use_unicode_whitespace_and_ctrl_h() {
        let mut editor = PromptEditor::new();
        editor.set_value("\u{2003}界  two");
        assert_eq!(
            editor.handle_event(&key(KeyCode::Home)),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&modified(KeyCode::Delete, KeyModifiers::CONTROL)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "two");

        editor.set_value("one  two  ");
        assert_eq!(
            editor.handle_event(&modified(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "one  ");
    }

    #[test]
    fn shifted_control_word_shortcuts_are_ignored() {
        for shortcut_key in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Backspace,
            KeyCode::Delete,
        ] {
            let mut editor = PromptEditor::new();
            editor.set_value("one two");
            editor.handle_event(&key(KeyCode::Home));
            for _ in 0..3 {
                editor.handle_event(&key(KeyCode::Right));
            }

            assert_eq!(
                editor.handle_event(&modified(
                    shortcut_key,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )),
                EditorOutcome::Ignored,
                "shifted Ctrl+{shortcut_key:?}"
            );
            assert_eq!(editor.value(), "one two", "shifted Ctrl+{shortcut_key:?}");
            assert_eq!(
                editor.handle_event(&key(KeyCode::Delete)),
                EditorOutcome::Changed,
                "cursor moved by shifted Ctrl+{shortcut_key:?}"
            );
            assert_eq!(
                editor.value(),
                "onetwo",
                "cursor moved by shifted Ctrl+{shortcut_key:?}"
            );
        }
    }

    #[test]
    fn editor_constructs_and_deletes_whole_graphemes_from_scalar_input() {
        let cases: &[(&str, &[char])] = &[
            ("combining mark", &['e', '\u{301}']),
            ("emoji modifier", &['👍', '🏽']),
            ("regional indicators", &['🇺', '🇸']),
            ("zwj sequence", &['👩', '\u{200d}', '💻']),
        ];

        for &(name, scalars) in cases {
            let expected = scalars.iter().collect::<String>();
            let mut editor = PromptEditor::new();

            for &scalar in scalars {
                assert_eq!(
                    editor.handle_event(&key(KeyCode::Char(scalar))),
                    EditorOutcome::Changed,
                    "insert {name}"
                );
            }
            assert_eq!(editor.value(), expected, "value after {name}");
            assert_eq!(
                editor.handle_event(&key(KeyCode::Right)),
                EditorOutcome::Consumed,
                "cursor is at the end of {name}"
            );
            assert_eq!(
                editor.handle_event(&key(KeyCode::Backspace)),
                EditorOutcome::Changed,
                "backspace removes {name}"
            );
            assert_eq!(editor.value(), "", "empty after backspacing {name}");

            for &scalar in scalars {
                editor.handle_event(&key(KeyCode::Char(scalar)));
            }
            assert_eq!(
                editor.handle_event(&key(KeyCode::Left)),
                EditorOutcome::Changed,
                "left crosses all of {name}"
            );
            assert_eq!(
                editor.handle_event(&key(KeyCode::Delete)),
                EditorOutcome::Changed,
                "delete removes all of {name}"
            );
            assert_eq!(editor.value(), "", "empty after deleting {name}");
        }
    }

    #[test]
    fn editor_sanitizes_paste_and_typed_characters_before_submit() {
        let mut configured = PromptEditor::new();
        configured.set_value("A\x1b[2JB\x1b[?1049hC\tD\x08E\x7fF\u{0085}G\r\nH");
        assert_eq!(configured.value(), "A[2JB[?1049hC DEFG\nH");

        let mut editor = PromptEditor::new();
        assert_eq!(
            editor.handle_event(&Event::Paste(
                "P\x1b[2J\x1b[?1049h\tQ\0\x08\x1f\x7f\u{0080}\u{009f}\r\nR".to_owned(),
            )),
            EditorOutcome::Changed
        );
        assert_eq!(editor.value(), "P[2J[?1049h Q\nR");

        for control in ['\x1b', '\x08', '\u{007f}', '\u{0085}'] {
            assert_eq!(
                editor.handle_event(&key(KeyCode::Char(control))),
                EditorOutcome::Consumed,
                "typed control U+{:04X}",
                control as u32
            );
        }
        assert_eq!(
            editor.handle_event(&key(KeyCode::Char('\t'))),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Char('界'))),
            EditorOutcome::Changed
        );
        assert_eq!(
            editor.handle_event(&key(KeyCode::Enter)),
            EditorOutcome::Submitted("P[2J[?1049h Q\nR 界".into())
        );

        assert_eq!(
            editor.handle_event(&key(KeyCode::Enter)),
            EditorOutcome::Submitted(String::new())
        );
    }

    #[test]
    fn editor_ignores_releases_and_non_key_events() {
        let mut editor = PromptEditor::new();
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(editor.handle_event(&release), EditorOutcome::Ignored);
        assert_eq!(
            editor.handle_event(&Event::Resize(20, 4)),
            EditorOutcome::Ignored
        );
        assert_eq!(editor.value(), "");
    }
}
