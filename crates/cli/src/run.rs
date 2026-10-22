use std::io::{Read, Write};
use std::process::ExitCode;

use crate::args;
use crate::commands::{Local, Repeat, Request};
use crate::wire::{self, Failure, Reply, Value};

/// The running Sonora refused the command, or the exchange broke.
const FAILED: u8 = 1;
/// No Sonora runs to send the command to. clap already takes 2 for usage errors.
const NOT_RUNNING: u8 = 3;

/// Carries out a command that needs no running Sonora.
pub fn local(local: Local) -> ExitCode {
    match local {
        Local::Completions(shell) => args::completions(shell),
    }
    ExitCode::SUCCESS
}

/// Sends `request` over `stream`, prints the reply and gives the code to exit with.
pub fn run<S: Read + Write>(stream: &mut S, request: &Request) -> ExitCode {
    // TODO: the messages here come from Fluent once render lands
    match wire::exchange(stream, request) {
        Ok(Reply::Done) => ExitCode::SUCCESS,
        Ok(Reply::Value(value)) => {
            println!("{}", show(&value));
            ExitCode::SUCCESS
        }
        Ok(Reply::Failed(failure)) => {
            eprintln!("sonora: {}", describe(&failure));
            ExitCode::from(FAILED)
        }
        Err(wire::Error::Closed) => {
            eprintln!("sonora: the running Sonora did not answer; it may be older, restart it");
            ExitCode::from(FAILED)
        }
        Err(error) => {
            eprintln!("sonora: {error}");
            ExitCode::from(FAILED)
        }
    }
}

/// Says that no Sonora runs, for a command that has nowhere to go.
pub fn not_running() -> ExitCode {
    eprintln!("sonora: Sonora is not running");
    ExitCode::from(NOT_RUNNING)
}

/// A queried value as a script reads it: the track as JSON, everything else as a bare word.
fn show(value: &Value) -> String {
    match value {
        Value::Current(current) => serde_json::to_string_pretty(current).unwrap_or_default(),
        Value::Volume(level) => level.to_string(),
        Value::Shuffle(on) => match on {
            true => "on".to_owned(),
            false => "off".to_owned(),
        },
        Value::Repeat(repeat) => match repeat {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
        .to_owned(),
    }
}

fn describe(failure: &Failure) -> String {
    match failure {
        Failure::Version(v) => format!("the running Sonora speaks protocol {v}; restart it"),
        Failure::Malformed => "the running Sonora could not read the command".to_owned(),
        Failure::Unsupported => "this command is not supported yet".to_owned(),
        Failure::SignedOut => "no provider is signed in".to_owned(),
        Failure::OtherProvider { link, current } => {
            format!("the link is for {link}, but {current} is the current provider")
        }
        Failure::Unqueueable(item) => format!("'{item}' cannot be queued"),
        Failure::Unreadable(path) => format!("cannot read '{path}'"),
        Failure::Unfetched(link) => format!("cannot fetch '{link}'"),
    }
}
