use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{self, Path};
use std::process::ExitCode;

use clap::error::ErrorKind;
use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::Shell;

use crate::commands::{
    CommandType, Item, Level, Local, PlaybackCommand, QueueCommand, Repeat, Request, Switch,
};
use crate::item;

/// The command line as the user types it. It is flat where `Request` is grouped, so the two
/// shapes are kept apart and joined in `into_command`.
#[derive(Parser)]
#[command(
    name = "sonora",
    version,
    about = "A native music streaming client",
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Sub>,
    /// Files or links to open.
    #[arg(value_hint = ValueHint::AnyPath)]
    items: Vec<String>,
}

#[derive(Subcommand)]
enum Sub {
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Skip to the next track.
    Next,
    /// Go back to the previous track.
    #[command(alias = "previous")]
    Prev,
    /// Jump to a point of the current track, in percent.
    Seek {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
    /// Print the current track as JSON.
    Current,
    /// Print the volume, or set it: `50`, `+5`, `-5`.
    Volume {
        /// A level from 0 to 100, or a step from the current one.
        #[arg(allow_negative_numbers = true, value_parser = level)]
        level: Option<Level>,
    },
    /// Print shuffle, or switch it.
    Shuffle { switch: Option<SwitchArg> },
    /// Print repeat, or set it.
    Repeat { repeat: Option<RepeatArg> },
    /// Queue files or links after the tracks queued by hand.
    Add {
        #[arg(required = true, value_hint = ValueHint::AnyPath, value_parser = queued)]
        items: Vec<Item>,
    },
    /// Queue files or links right after the current track.
    PlayNext {
        #[arg(required = true, value_hint = ValueHint::AnyPath, value_parser = queued)]
        items: Vec<Item>,
    },
    /// Print the completion script for a shell.
    Completions { shell: Shell },
}

#[derive(Clone, Copy, ValueEnum)]
enum SwitchArg {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Copy, ValueEnum)]
enum RepeatArg {
    Off,
    All,
    One,
}

/// Reads the command line, `args` including the program name. Help, version and usage errors
/// are printed here and come back as the code to exit with.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<CommandType, ExitCode> {
    let args = Args::try_parse_from(args).map_err(fail)?;
    into_command(args).map_err(fail)
}

/// Writes the completion script for `shell` to stdout. It is built whole first, since clap
/// panics when stdout closes under it, as it does when piped into `head`.
pub fn completions(shell: Shell) {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Args::command(), "sonora", &mut script);
    io::stdout().write_all(&script).ok();
}

fn into_command(args: Args) -> Result<CommandType, clap::Error> {
    let Some(command) = args.command else {
        if let Some(typo) = args.items.iter().find(|item| is_typo(item)) {
            return Err(Args::command().error(
                ErrorKind::InvalidSubcommand,
                format!("unrecognized subcommand '{typo}'"),
            ));
        }
        let items = args.items.into_iter().map(item::anchored).collect();
        return Ok(CommandType::Launch(items));
    };
    let playback = match command {
        Sub::Play => PlaybackCommand::Play,
        Sub::Pause => PlaybackCommand::Pause,
        Sub::Next => PlaybackCommand::Next,
        Sub::Prev => PlaybackCommand::Previous,
        Sub::Seek { percent } => PlaybackCommand::Seek(percent),
        Sub::Current => PlaybackCommand::CurrentTrack,
        Sub::Volume { level } => PlaybackCommand::Volume(level),
        Sub::Shuffle { switch } => PlaybackCommand::Shuffle(switch.map(Switch::from)),
        Sub::Repeat { repeat } => PlaybackCommand::Repeat(repeat.map(Repeat::from)),
        Sub::Add { items } => return Ok(queue(QueueCommand::Add(items))),
        Sub::PlayNext { items } => return Ok(queue(QueueCommand::PlayNext(items))),
        Sub::Completions { shell } => return Ok(CommandType::Local(Local::Completions(shell))),
    };
    Ok(CommandType::Remote(Request::Playback(playback)))
}

impl From<SwitchArg> for Switch {
    fn from(switch: SwitchArg) -> Self {
        match switch {
            SwitchArg::On => Self::On,
            SwitchArg::Off => Self::Off,
            SwitchArg::Toggle => Self::Toggle,
        }
    }
}

impl From<RepeatArg> for Repeat {
    fn from(repeat: RepeatArg) -> Self {
        match repeat {
            RepeatArg::Off => Self::Off,
            RepeatArg::All => Self::All,
            RepeatArg::One => Self::One,
        }
    }
}

fn queue(command: QueueCommand) -> CommandType {
    CommandType::Remote(Request::Queue(command))
}

/// Prints a clap error, or the help and version it carries, and gives its exit code.
fn fail(error: clap::Error) -> ExitCode {
    error.print().ok();
    ExitCode::from(error.exit_code() as u8)
}

/// A volume as typed: a signed step from the current level, or a level to land on.
fn level(text: &str) -> Result<Level, String> {
    let signed = text.starts_with(['+', '-']);
    let value: i16 = text
        .parse()
        .map_err(|_| format!("'{text}' is not a number"))?;
    match signed {
        true => i8::try_from(value)
            .map(Level::By)
            .map_err(|_| format!("a step of {value} is too large")),
        false => u8::try_from(value)
            .ok()
            .filter(|value| *value <= 100)
            .map(Level::To)
            .ok_or_else(|| format!("{value} is not between 0 and 100")),
    }
}

/// An item to queue as typed: an address goes as it is, a path has to name a file here.
fn queued(text: &str) -> Result<Item, String> {
    if item::is_address(text) {
        return Ok(Item::Link(text.to_owned()));
    }
    let path = path::absolute(item::local_path(text)).map_err(|error| error.to_string())?;
    match path.exists() {
        true => Ok(Item::Path(path)),
        false => Err(format!("no file at '{}'", path.display())),
    }
}

/// Whether an item meant as a file or link reads as a mistyped command instead: a bare word
/// that names no file here and is no address.
fn is_typo(item: &str) -> bool {
    !item.contains(['/', '\\', ':', '.']) && !Path::new(item).exists()
}
