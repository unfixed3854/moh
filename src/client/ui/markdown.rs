#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn rendered(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn markdown_maps_inline_and_block_styles_to_spans() {
        let source = format!(
            "# Heading\n\n**bold** and {}code{}",
            char::from(96),
            char::from(96),
        );
        let text = markdown_text(&source);
        assert_eq!(text.lines.len(), 3);
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            text.lines[2]
                .spans
                .iter()
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            text.lines[2]
                .spans
                .iter()
                .any(|span| span.content == "code" && span.style.fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn links_keep_a_visible_destination_without_terminal_escapes() {
        let text = markdown_text("[Ratatui](https://ratatui.rs)");
        let rendered = text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered, "Ratatui (https://ratatui.rs)");
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn markdown_sanitization_preserves_lines_and_removes_terminal_controls() {
        assert_eq!(
            sanitize_markdown_source("# title\r\n\nbody\t\x1b[31mred\x1b[0m"),
            "# title\n\nbody    red"
        );
    }

    #[test]
    fn markdown_preserves_inline_semantics_without_ansi_output() {
        let text = markdown_text(
            "This is **bold**, *italic*, ~~removed~~, `code`, and [docs](https://example.test).",
        );
        let line = &text.lines[0];

        assert!(line.spans.iter().any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(line.spans.iter().any(|span| {
            span.content == "italic" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(line.spans.iter().any(|span| {
            span.content == "removed" && span.style.add_modifier.contains(Modifier::CROSSED_OUT)
        }));
        assert!(
            line.spans
                .iter()
                .any(|span| { span.content == "code" && span.style.fg == Some(Color::Yellow) })
        );
        assert!(line.spans.iter().any(|span| {
            span.content == "docs" && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
        assert_eq!(
            rendered(&text),
            "This is bold, italic, removed, code, and docs (https://example.test)."
        );
        assert!(!rendered(&text).contains('\u{1b}'));
    }

    #[test]
    fn markdown_preserves_block_list_quote_and_code_structure() {
        let text = markdown_text(
            "# Heading\n\n> quoted\n\n- first\n- second\n\n1. one\n2. two\n\n```rust\nlet x = 1;\n```\n\n---",
        );
        let rendered = rendered(&text);

        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(rendered.contains("│ quoted"));
        assert!(rendered.contains("• first"));
        assert!(rendered.contains("• second"));
        assert!(rendered.contains("1. one"));
        assert!(rendered.contains("2. two"));
        assert!(text.lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "rust"
                && span.style.fg == Some(Color::Yellow)
                && span.style.add_modifier.contains(Modifier::DIM)
        }));
        assert!(
            text.lines.iter().flat_map(|line| &line.spans).any(|span| {
                span.content == "let x = 1;" && span.style.fg == Some(Color::Yellow)
            })
        );
        assert!(text.lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "───" && span.style.add_modifier.contains(Modifier::DIM)
        }));
    }

    #[test]
    fn markdown_keeps_task_markers_and_nested_list_indentation() {
        let text = markdown_text("- [x] done\n  - [ ] queued");
        let rendered = rendered(&text);

        assert!(rendered.contains("• [x] done"));
        assert!(rendered.contains("  • [ ] queued"));
    }

    #[test]
    fn incomplete_streamed_markdown_remains_readable() {
        let text = markdown_text("Starting **bold and `code\n\n```rust\nfn main() {");
        let rendered = rendered(&text);

        assert!(rendered.contains("Starting"));
        assert!(rendered.contains("fn main() {"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn unicode_content_is_retained_without_width_or_ansi_processing() {
        let text = markdown_text("界 and e\u{301} and 👩‍💻");

        assert_eq!(rendered(&text), "界 and e\u{301} and 👩‍💻");
    }

    #[test]
    fn sanitization_removes_csi_osc_and_apc_sequences() {
        let source = "a\x1b[2Jb\x1b]8;;https://example.test\x07c\x1b_payload\x1b\\de";

        assert_eq!(sanitize_markdown_source(source), "abcde");
        assert_eq!(sanitize_markdown_source("a\x1b[31"), "a[31");
    }

    #[test]
    fn sanitization_removes_c1_csi_osc_and_apc_sequences() {
        assert_eq!(sanitize_markdown_source("a\u{009b}31mb"), "ab");
        assert_eq!(
            sanitize_markdown_source("a\u{009d}8;;https://example.test\x07b"),
            "ab"
        );
        assert_eq!(
            sanitize_markdown_source("a\u{009d}8;;https://example.test\x1b\\b"),
            "ab"
        );
        assert_eq!(sanitize_markdown_source("a\u{009f}payload\u{009c}b"), "ab");
        assert_eq!(sanitize_markdown_source("a\u{009f}payload\x07b"), "ab");
    }

    #[test]
    fn links_avoid_repeating_an_identical_destination() {
        let text = markdown_text("<https://ratatui.rs>");

        assert_eq!(rendered(&text), "https://ratatui.rs");
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn soft_and_hard_breaks_become_spaces_and_lines() {
        let text = markdown_text("first\nsecond  \nthird");

        assert_eq!(rendered(&text), "first secondthird");
        assert_eq!(text.lines.len(), 2);
        assert_eq!(
            text.lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "first second"
        );
        assert_eq!(
            text.lines[1]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "third"
        );
    }
}
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

pub(super) fn markdown_text(source: &str) -> Text<'static> {
    let source = sanitize_markdown_source(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    MarkdownBuilder::new().collect(Parser::new_ext(&source, options))
}

pub(super) fn sanitize_markdown_source(source: &str) -> String {
    let mut stripped = String::with_capacity(source.len());
    let mut sequence = String::new();
    let mut state = EscapeState::Text;

    for character in source.chars() {
        match state {
            EscapeState::Text => match character {
                '\x1b' => state = EscapeState::Escape,
                '\u{009b}' => state = EscapeState::Csi,
                '\u{009d}' => state = EscapeState::Osc,
                '\u{009f}' => state = EscapeState::Apc,
                _ => {
                    stripped.push(character);
                }
            },
            EscapeState::Escape => match character {
                '[' => {
                    sequence.push(character);
                    state = EscapeState::Csi;
                }
                ']' => {
                    sequence.push(character);
                    state = EscapeState::Osc;
                }
                '_' => {
                    sequence.push(character);
                    state = EscapeState::Apc;
                }
                '\x1b' => state = EscapeState::Escape,
                _ => {
                    stripped.push(character);
                    state = EscapeState::Text;
                }
            },
            EscapeState::Csi => {
                if is_csi_final(character) {
                    sequence.clear();
                    state = EscapeState::Text;
                } else if character == '\x1b' {
                    stripped.push_str(&sequence);
                    sequence.clear();
                    state = EscapeState::Escape;
                } else {
                    sequence.push(character);
                }
            }
            EscapeState::Osc | EscapeState::Apc => {
                if matches!(character, '\x07' | '\u{009c}') {
                    sequence.clear();
                    state = EscapeState::Text;
                } else if character == '\x1b' {
                    state = EscapeState::StringTerminator;
                } else {
                    sequence.push(character);
                }
            }
            EscapeState::StringTerminator => {
                if matches!(character, '\\' | '\u{009c}') {
                    sequence.clear();
                    state = EscapeState::Text;
                } else if character == '\x1b' {
                    sequence.push('\x1b');
                } else {
                    sequence.push(character);
                    state = EscapeState::Osc;
                }
            }
        }
    }

    if !matches!(state, EscapeState::Text) {
        stripped.push_str(&sequence);
    }

    let normalized = stripped.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter_map(|character| match character {
            '\n' => Some("\n".to_owned()),
            '\t' => Some("    ".to_owned()),
            character if character.is_control() => None,
            character => Some(character.to_string()),
        })
        .collect()
}

#[derive(Clone, Copy)]
enum EscapeState {
    Text,
    Escape,
    Csi,
    Osc,
    Apc,
    StringTerminator,
}

fn is_csi_final(character: char) -> bool {
    matches!(character as u32, 0x40..=0x7e)
}

struct MarkdownBuilder {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style: Style,
    style_stack: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    in_code_block: bool,
    links: Vec<LinkContext>,
}

struct LinkContext {
    destination: String,
    label: String,
}

impl MarkdownBuilder {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style: Style::default(),
            style_stack: Vec::new(),
            lists: Vec::new(),
            quote_depth: 0,
            in_code_block: false,
            links: Vec::new(),
        }
    }

    fn collect<'a>(mut self, events: impl IntoIterator<Item = Event<'a>>) -> Text<'static> {
        for event in events {
            self.event(event);
        }
        self.finish()
    }

    fn push_text(&mut self, content: &str) {
        let content = sanitize_markdown_source(content);
        if content.is_empty() {
            return;
        }
        if let Some(link) = self.links.last_mut() {
            link.label.push_str(&content);
        }
        self.current.push(Span::styled(content, self.style));
    }

    fn push_quote_prefix(&mut self) {
        for _ in 0..self.quote_depth {
            self.current.push(Span::styled(
                "│ ",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
    }

    fn finish_line(&mut self) {
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    fn start_block(&mut self, nested: bool) {
        if !self.current.is_empty() {
            self.finish_line();
        }
        if !nested && self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::default());
        }
        self.push_quote_prefix();
    }

    fn end_block(&mut self) {
        if !self.current.is_empty() {
            self.finish_line();
        }
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(self.style);
        self.style = style;
    }

    fn pop_style(&mut self) {
        self.style = self.style_stack.pop().unwrap_or_default();
    }

    fn append_code_text(&mut self, text: &str) {
        let mut lines = text.split('\n').peekable();
        while let Some(line) = lines.next() {
            self.push_text(line);
            if lines.peek().is_some() {
                self.finish_line();
                self.push_quote_prefix();
            }
        }
    }

    fn finish(mut self) -> Text<'static> {
        if !self.current.is_empty() {
            self.finish_line();
        }
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if self.current.is_empty() {
                        self.start_block(!self.lists.is_empty());
                    }
                }
                Tag::Heading { .. } => {
                    self.start_block(false);
                    self.push_style(self.style.add_modifier(Modifier::BOLD));
                }
                Tag::BlockQuote(_) => self.quote_depth += 1,
                Tag::CodeBlock(kind) => {
                    self.start_block(false);
                    self.push_style(self.style.fg(Color::Yellow));
                    if let CodeBlockKind::Fenced(language) = kind
                        && !language.is_empty()
                    {
                        self.push_style(self.style.add_modifier(Modifier::DIM));
                        self.push_text(language.as_ref());
                        self.pop_style();
                        self.finish_line();
                        self.push_quote_prefix();
                    }
                    self.in_code_block = true;
                }
                Tag::List(start) => {
                    self.start_block(!self.lists.is_empty());
                    self.lists.push(start);
                }
                Tag::Item => {
                    if !self.current.is_empty() {
                        self.finish_line();
                    }
                    self.push_quote_prefix();
                    self.push_text(&"  ".repeat(self.lists.len().saturating_sub(1)));
                    let marker = if let Some(Some(number)) = self.lists.last_mut() {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    } else {
                        "• ".to_owned()
                    };
                    self.push_text(&marker);
                }
                Tag::Emphasis => self.push_style(self.style.add_modifier(Modifier::ITALIC)),
                Tag::Strong => self.push_style(self.style.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => {
                    self.push_style(self.style.add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { dest_url, .. } => {
                    self.push_style(self.style.add_modifier(Modifier::UNDERLINED));
                    self.links.push(LinkContext {
                        destination: sanitize_markdown_source(dest_url.as_ref()),
                        label: String::new(),
                    });
                }
                Tag::Image { .. } => self.push_text("[image: "),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => self.end_block(),
                TagEnd::Heading(_) => {
                    self.pop_style();
                    self.end_block();
                }
                TagEnd::BlockQuote(_) => {
                    self.end_block();
                    self.quote_depth = self.quote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    self.in_code_block = false;
                    self.pop_style();
                    self.end_block();
                }
                TagEnd::List(_) => {
                    self.end_block();
                    self.lists.pop();
                }
                TagEnd::Item => {}
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
                TagEnd::Link => {
                    self.pop_style();
                    if let Some(link) = self.links.pop()
                        && !link.destination.is_empty()
                        && link.label != link.destination
                    {
                        self.current.push(Span::styled(
                            format!(" ({})", link.destination),
                            Style::default().add_modifier(Modifier::DIM),
                        ));
                    }
                }
                TagEnd::Image => self.push_text("]"),
                _ => {}
            },
            Event::Text(text) => {
                if self.in_code_block {
                    self.append_code_text(text.as_ref());
                } else {
                    self.push_text(text.as_ref());
                }
            }
            Event::Code(code) => {
                self.push_style(self.style.fg(Color::Yellow));
                self.push_text(code.as_ref());
                self.pop_style();
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => {
                self.finish_line();
                self.push_quote_prefix();
            }
            Event::Rule => {
                self.start_block(false);
                self.current.push(Span::styled(
                    "───",
                    Style::default().add_modifier(Modifier::DIM),
                ));
                self.end_block();
            }
            Event::TaskListMarker(checked) => self.push_text(if checked { "[x] " } else { "[ ] " }),
            Event::Html(html) | Event::InlineHtml(html) => self.push_text(html.as_ref()),
            Event::FootnoteReference(label) => self.push_text(&format!("[{label}]")),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.push_text(math.as_ref()),
        }
    }
}
