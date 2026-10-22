use cli::commands::{self, Item, Level, PlaybackCommand, QueueCommand, Request, Switch};
use cli::wire::{self, Current, Failure, Reply, State, Value};
use gpui::{App, Context};
use music::Track;
use music::link::Linked;

use crate::{Origin, Playback, PlaybackState, Repeat, Sonora};

/// Carries out a command the command line sent and says what to answer it. Playback goes
/// through `Playback`, so a device steered over Connect is the one that obeys.
pub fn answer(request: Request, cx: &mut App) -> Reply {
    let playback = Sonora::global(cx).playback.clone();
    match request {
        Request::Playback(command) => {
            playback.update(cx, |playback, cx| play(playback, command, cx))
        }
        Request::Queue(command) => queue(command, cx),
        Request::Provider | Request::Search => Reply::Failed(Failure::Unsupported),
    }
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

/// Queues files and a link. The link is read before anything is queued, so a bad one refuses
/// the whole command; what fails later, a file that does not read or a fetch, is told on a
/// toast in the app. Files and the link are fetched apart, so they may land in either order.
fn queue(command: QueueCommand, cx: &mut App) -> Reply {
    let (items, next) = match command {
        QueueCommand::Add(items) => (items, false),
        QueueCommand::PlayNext(items) => (items, true),
    };
    let mut paths = Vec::new();
    let mut links = Vec::new();
    for item in items {
        match item {
            Item::Path(path) => paths.push(path),
            Item::Link(link) => links.push(link),
        }
    }
    let linked = match links.as_slice() {
        [] => None,
        [link] => match read(link, cx) {
            Ok(linked) => Some(linked),
            Err(failure) => return Reply::Failed(failure),
        },
        _ => return Reply::Failed(Failure::ManyLinks),
    };

    let playback = Sonora::global(cx).playback.clone();
    playback.update(cx, |playback, cx| {
        match (paths.is_empty(), next) {
            (true, _) => {}
            (false, true) => playback.play_paths_next(paths, cx),
            (false, false) => playback.enqueue_paths(paths, cx),
        }
        match (linked, next) {
            (None, _) => {}
            (Some(Linked::Track(id)), true) => playback.play_track_next(id, cx),
            (Some(Linked::Track(id)), false) => playback.enqueue_track(id, cx),
            (Some(Linked::Album(id)), true) => playback.play_album_next(Origin::album(id), cx),
            (Some(Linked::Album(id)), false) => playback.enqueue_album(Origin::album(id), cx),
            (Some(Linked::Playlist(id)), true) => {
                playback.play_playlist_next(Origin::playlist(id), cx)
            }
            (Some(Linked::Playlist(id)), false) => {
                playback.enqueue_playlist(Origin::playlist(id), cx)
            }
            (Some(Linked::Artist(id)), true) => playback.play_artist_next(Origin::artist(id), cx),
            (Some(Linked::Artist(id)), false) => playback.enqueue_artist(Origin::artist(id), cx),
            (Some(Linked::User(_) | Linked::Jam(_) | Linked::Short), _) => {}
        }
    });
    Reply::Done
}

/// What a link to queue points at, refused unless it is a track, album, playlist or artist of
/// the provider signed in now.
fn read(link: &str, cx: &App) -> Result<Linked, Failure> {
    let session = Sonora::global(cx).session.read(cx);
    let unqueueable = || Failure::Unqueueable(link.to_owned());
    let (slug, linked) = session.read_link(link).ok_or_else(unqueueable)?;
    let current = session.provider_slug().ok_or(Failure::SignedOut)?;
    if slug != current {
        let name = |slug: &'static str| session.account(slug).map_or(slug, |(name, _)| name);
        return Err(Failure::OtherProvider {
            link: name(slug).to_owned(),
            current: name(current).to_owned(),
        });
    }
    if session.client().is_none() {
        return Err(Failure::SignedOut);
    }
    match linked {
        Linked::Track(_) | Linked::Album(_) | Linked::Playlist(_) | Linked::Artist(_) => Ok(linked),
        Linked::User(_) | Linked::Jam(_) | Linked::Short => Err(unqueueable()),
    }
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
        position: position.as_secs(),
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
        duration: track.duration.as_secs(),
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
