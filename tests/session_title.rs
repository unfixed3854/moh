use moh::session::{SessionTitle, fallback_title, sanitize_generated_title};

#[test]
fn titles_reject_empty_untrimmed_controlled_and_overlong_values() {
    assert!(SessionTitle::parse("").is_err());
    assert!(SessionTitle::parse(" title").is_err());
    assert!(SessionTitle::parse("title\n").is_err());
    assert!(SessionTitle::parse("界".repeat(65)).is_err());
    assert_eq!(
        SessionTitle::parse("界".repeat(64)).unwrap().as_str(),
        "界".repeat(64)
    );
}

#[test]
fn fallback_collapses_whitespace_and_truncates_on_a_scalar_boundary() {
    let title = fallback_title("  investigate\n\tthis   session persistence failure in detail  ");
    assert_eq!(
        title.as_str(),
        "investigate this session persistence failure in detail"
    );
}

#[test]
fn fallback_prefers_a_complete_word_before_adding_an_ellipsis() {
    assert_eq!(
        fallback_title(
            "zero one two three four five six seven eight nine ten eleven twelve thirteen"
        )
        .as_str(),
        "zero one two three four five six seven eight nine ten eleven…"
    );
}

#[test]
fn generated_title_uses_first_plain_nonempty_line() {
    assert_eq!(
        sanitize_generated_title("\n**\"Fix session switching\"**\nignored")
            .unwrap()
            .as_str(),
        "Fix session switching"
    );
    assert!(sanitize_generated_title("\u{1b}[2J\n").is_none());
}
