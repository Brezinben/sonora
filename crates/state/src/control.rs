use std::time::Duration;

use cli::commands::{self, Level, PlaybackCommand, Request, Switch};
use cli::wire::{self, Current, Failure, Reply, State, Value};
use gpui::{App, Context};
use music::Track;

use crate::{Playback, PlaybackState, Repeat, Sonora};

/// Carries out a command the command line sent and says what to answer it. Playback goes
/// through `Playback`, so a device steered over Connect is the one that obeys.
pub fn answer(request: Request, cx: &mut App) -> Reply {
    let Request::Playback(command) = request else {
        return Reply::Failed(Failure::Unsupported);
    };
    let playback = Sonora::global(cx).playback.clone();
    playback.update(cx, |playback, cx| play(playback, command, cx))
}

fn play(playback: &mut Playback, command: PlaybackCommand, cx: &mut Context<Playback>) -> Reply {
    match command {
        PlaybackCommand::Play => playback.resume(cx),
        PlaybackCommand::Pause => playback.pause(cx),
        PlaybackCommand::Next => playback.next(cx),
        PlaybackCommand::Previous => playback.previous(cx),
        PlaybackCommand::Seek(percent) => playback.seek_fraction(f32::from(percent) / 100., cx),
        PlaybackCommand::CurrentTrack => {
            return Reply::Value(Value::Current(current(playback, cx)));
        }
        PlaybackCommand::Volume(None) => {
            return Reply::Value(Value::Volume(percent(playback.shown_volume(cx))));
        }
        PlaybackCommand::Volume(Some(level)) => {
            let target = match level {
                Level::To(level) => i16::from(level),
                Level::By(step) => i16::from(percent(playback.shown_volume(cx))) + i16::from(step),
            };
            playback.set_volume(f32::from(target.clamp(0, 100)) / 100., cx);
        }
        PlaybackCommand::Shuffle(None) => {
            return Reply::Value(Value::Shuffle(playback.shown_shuffle(cx)));
        }
        PlaybackCommand::Shuffle(Some(switch)) => match switch {
            Switch::On => playback.set_shuffle(true, cx),
            Switch::Off => playback.set_shuffle(false, cx),
            Switch::Toggle => playback.toggle_shuffle(cx),
        },
        PlaybackCommand::Repeat(None) => {
            return Reply::Value(Value::Repeat(wire_repeat(playback.shown_repeat(cx))));
        }
        PlaybackCommand::Repeat(Some(repeat)) => {
            let repeat = match repeat {
                commands::Repeat::Off => Repeat::Off,
                commands::Repeat::All => Repeat::All,
                commands::Repeat::One => Repeat::One,
            };
            playback.set_repeat(repeat, cx);
        }
    }
    Reply::Done
}

/// What plays now, on the steered device when there is one, or `None` when nothing is loaded.
fn current(playback: &Playback, cx: &App) -> Option<Current> {
    let track = playback.shown_track(cx)?;
    let (state, position) = match playback.steered(cx) {
        Some(steered) => {
            let state = match steered.playing {
                true => State::Playing,
                false => State::Paused,
            };
            (state, steered.position)
        }
        None => {
            let state = match playback.state() {
                PlaybackState::Playing => State::Playing,
                PlaybackState::Paused => State::Paused,
                PlaybackState::Loading => State::Loading,
                PlaybackState::Idle | PlaybackState::Failed(_) => State::Idle,
            };
            (state, playback.live_position())
        }
    };
    Some(Current {
        state,
        track: wire_track(&track),
        position_ms: millis(position),
        volume: percent(playback.shown_volume(cx)),
        shuffle: playback.shown_shuffle(cx),
        repeat: wire_repeat(playback.shown_repeat(cx)),
    })
}

fn wire_track(track: &Track) -> wire::Track {
    let artists = match track.artist_refs.is_empty() {
        true => vec![track.artists.clone()],
        false => track
            .artist_refs
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
    };
    wire::Track {
        id: track.id.clone(),
        title: track.name.clone(),
        artists,
        album: track.album.clone(),
        cover: track.cover.clone(),
        duration_ms: millis(track.duration),
    }
}

fn wire_repeat(repeat: Repeat) -> commands::Repeat {
    match repeat {
        Repeat::Off => commands::Repeat::Off,
        Repeat::All => commands::Repeat::All,
        Repeat::One => commands::Repeat::One,
    }
}

/// A volume level of 0 to 1 as the whole percent the command line speaks.
fn percent(level: f32) -> u8 {
    (level.clamp(0., 1.) * 100.).round() as u8
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
