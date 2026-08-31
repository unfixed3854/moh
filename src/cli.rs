//! Command-line mode parsing and domain validation.

use std::ffi::OsString;

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};

use crate::session::{SessionId, SessionSelector, SessionTitle};

/// A validated command-line mode ready for later binary dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliMode {
    /// Apply running-session startup selection for the current working directory.
    Default,
    /// Open a fresh ephemeral chat.
    New,
    /// Attach to an existing session by ID or current-directory title.
    Session {
        /// Validated session ID or title.
        selector: SessionSelector,
    },
    /// List sessions for the current working directory.
    Sessions,
    /// Run the backend server.
    Server {
        /// Whether to use the private detached-process mode.
        detached: bool,
    },
}

#[derive(Debug, Parser)]
#[command(name = "moh", args_conflicts_with_subcommands = true)]
struct CliArguments {
    /// Open a fresh ephemeral chat.
    #[arg(long, conflicts_with = "resume")]
    new: bool,
    /// Attach to an existing session by ID or current-directory title.
    #[arg(long, value_name = "SELECTOR", conflicts_with = "new")]
    resume: Option<String>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// List sessions for the current working directory.
    Sessions,
    /// Run the backend server.
    Server(ServerArguments),
}

#[derive(Args, Debug)]
struct ServerArguments {
    /// Run in the private detached-process mode.
    #[arg(long, hide = true)]
    internal_detached: bool,
}

/// Parses a process argument iterator into one validated command-line mode.
pub fn parse<I, S>(arguments: I) -> Result<CliMode, clap::Error>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString> + Clone,
{
    let arguments = CliArguments::try_parse_from(arguments)?;
    match arguments {
        CliArguments {
            new: true,
            resume: None,
            command: None,
        } => Ok(CliMode::New),
        CliArguments {
            new: false,
            resume: Some(selector),
            command: None,
        } => parse_selector(&selector).map(|selector| CliMode::Session { selector }),
        CliArguments {
            new: false,
            resume: None,
            command: None,
        } => Ok(CliMode::Default),
        CliArguments {
            new: false,
            resume: None,
            command: Some(CliCommand::Sessions),
        } => Ok(CliMode::Sessions),
        CliArguments {
            new: false,
            resume: None,
            command: Some(CliCommand::Server(arguments)),
        } => Ok(CliMode::Server {
            detached: arguments.internal_detached,
        }),
        _ => unreachable!("clap rejects conflicting arguments before CLI mode conversion"),
    }
}

fn parse_selector(selector: &str) -> Result<SessionSelector, clap::Error> {
    if selector.starts_with("session-") {
        return selector
            .parse::<SessionId>()
            .map(SessionSelector::Id)
            .map_err(|error| cli_error(error.to_string()));
    }
    SessionTitle::parse(selector)
        .map(SessionSelector::Title)
        .map_err(|error| cli_error(error.to_string()))
}

fn cli_error(message: String) -> clap::Error {
    CliArguments::command().error(ErrorKind::ValueValidation, message)
}
