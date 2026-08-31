use clap::error::ErrorKind;
use moh::{
    cli::{CliMode, parse},
    session::{SessionId, SessionSelector, SessionTitle},
};

#[test]
fn supported_forms_select_the_requested_mode() {
    let cases = vec![
        (vec!["moh"], CliMode::Default),
        (vec!["moh", "--new"], CliMode::New),
        (
            vec!["moh", "--resume", "session-7"],
            CliMode::Session {
                selector: SessionSelector::Id("session-7".parse::<SessionId>().unwrap()),
            },
        ),
        (
            vec!["moh", "--resume", "przegląd sesji"],
            CliMode::Session {
                selector: SessionSelector::Title(SessionTitle::parse("przegląd sesji").unwrap()),
            },
        ),
        (vec!["moh", "sessions"], CliMode::Sessions),
        (vec!["moh", "server"], CliMode::Server { detached: false }),
        (
            vec!["moh", "server", "--internal-detached"],
            CliMode::Server { detached: true },
        ),
    ];

    for (arguments, expected) in cases {
        assert_eq!(parse(arguments).unwrap(), expected);
    }
}

#[test]
fn new_does_not_accept_a_session_title() {
    for title in ["review", "przegląd", "session-7", "--unknown"] {
        assert!(parse(["moh", "--new", title]).is_err());
    }
}

#[test]
fn resume_requires_a_selector() {
    assert_eq!(
        parse(["moh", "--resume"]).unwrap_err().kind(),
        ErrorKind::InvalidValue
    );
}

#[test]
fn new_resume_and_subcommands_cannot_be_combined() {
    for arguments in [
        vec!["moh", "--new", "--resume", "review"],
        vec!["moh", "--new", "sessions"],
        vec!["moh", "--resume", "review", "sessions"],
    ] {
        assert_eq!(
            parse(arguments).unwrap_err().kind(),
            ErrorKind::ArgumentConflict
        );
    }
}

#[test]
fn session_id_namespace_is_not_accepted_as_a_title() {
    for selector in [
        "session-",
        "session-0",
        "session-01",
        "session-x",
        "session-18446744073709551616",
        " review",
        "review ",
        "bad\nselector",
    ] {
        assert_eq!(
            parse(["moh", "--resume", selector]).unwrap_err().kind(),
            ErrorKind::ValueValidation
        );
    }

    assert_eq!(
        parse(["moh", "--resume", &"界".repeat(65)])
            .unwrap_err()
            .kind(),
        ErrorKind::ValueValidation
    );
}

#[test]
fn root_help_documents_the_public_interface() {
    let error = parse(["moh", "--help"]).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    let help = error.to_string();
    assert!(help.contains("--new"));
    assert!(help.contains("--resume <SELECTOR>"));
    assert!(help.contains("sessions"));
    assert!(help.contains("server"));
    assert!(!help.contains("--session"));
}

#[test]
fn server_help_hides_the_private_detached_flag() {
    let error = parse(["moh", "server", "--help"]).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::DisplayHelp);
    assert!(!error.to_string().contains("--internal-detached"));
}

#[cfg(unix)]
#[test]
fn non_unicode_resume_values_are_rejected_without_echoing_them() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let non_unicode = OsString::from_vec(vec![b's', b'e', b'c', b'r', b'e', b't', 0xff]);
    let error = parse([
        OsString::from("moh"),
        OsString::from("--resume"),
        non_unicode,
    ])
    .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::InvalidUtf8);
    assert!(!error.to_string().contains("secret"));
}
