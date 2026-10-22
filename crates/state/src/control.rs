use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cli::commands::{self, Item, Level, PlaybackCommand, QueueCommand, Request, Switch};
use cli::wire::{self, Current, Failure, Reply, State, Value};
use futures::future;
use gpui::{App, Context, Entity, Task};
use music::link::Linked;
use music::{MusicApi, MusicProvider, Track};
use tokio::sync::oneshot;

use crate::{Io, Origin, Playback, PlaybackState, Repeat, Session, join, noted};

/// Carries out what the command line asks of the running Sonora. Playback goes through
/// `Playback`, so a device steered over Connect is the one that obeys. A command that fetches
/// tracks first answers once they are queued, so a failure reaches the terminal.
pub struct Control {
    session: Entity<Session>,
    playback: Entity<Playback>,
    /// Queue commands still fetching, each removing itself once it answers.
    queueing: HashMap<u64, Task<()>>,
    issued: u64,
}

/// An item to queue, checked on the main thread and fetched on tokio.
enum Source {
    Path(PathBuf),
    /// The link as typed, for the failure that names it, and what it points at.
    Link(String, Linked),
}

/// The tracks one item stands for, and the collection they came from.
struct Batch {
    tracks: Vec<Track>,
    origin: Option<Origin>,
}

/// Why an item could not be fetched, with the network error behind it for `Network` to hear.
struct Missed {
    failure: Failure,
    error: Option<anyhow::Error>,
}

impl Control {
    pub fn new(session: Entity<Session>, playback: Entity<Playback>) -> Self {
        Self {
            session,
            playback,
            queueing: HashMap::new(),
            issued: 0,
        }
    }

    /// Carries out `request` and sends what to answer it into `reply`, at once or, for a
    /// command that fetches, once it is done.
    pub fn answer(
        &mut self,
        request: Request,
        reply: oneshot::Sender<Reply>,
        cx: &mut Context<Self>,
    ) {
        let answer = match request {
            Request::Playback(command) => self
                .playback
                .update(cx, |playback, cx| play(playback, command, cx)),
            Request::Queue(command) => return self.queue(command, reply, cx),
            Request::Provider | Request::Search => Reply::Failed(Failure::Unsupported),
        };
        reply.send(answer).ok();
    }

    /// Fetches every item at once and queues them together, in the order they were given. One
    /// that fails refuses the whole command, so nothing is queued half.
    fn queue(
        &mut self,
        command: QueueCommand,
        reply: oneshot::Sender<Reply>,
        cx: &mut Context<Self>,
    ) {
        let (items, next) = match command {
            QueueCommand::Add(items) => (items, false),
            QueueCommand::PlayNext(items) => (items, true),
        };
        let sources = items
            .into_iter()
            .map(|item| self.source(item, cx))
            .collect::<Result<Vec<_>, _>>();
        let sources = match sources {
            Ok(sources) => sources,
            Err(failure) => {
                reply.send(Reply::Failed(failure)).ok();
                return;
            }
        };

        let session = self.session.read(cx);
        let local = session.local_provider();
        let client = session.client();
        let fetch = Io::global(cx).spawn(async move {
            let fetches = sources
                .into_iter()
                .map(|source| fetch(source, &local, client.as_ref()));
            anyhow::Ok(future::try_join_all(fetches).await)
        });
        let key = self.issued;
        self.issued += 1;
        let task = cx.spawn(async move |this, cx| {
            let fetched = join(fetch).await;
            this.update(cx, |this, cx| {
                let answer = match fetched {
                    Ok(Ok(batches)) => {
                        this.place(batches, next, cx);
                        Reply::Done
                    }
                    Ok(Err(missed)) => {
                        if let Some(error) = &missed.error {
                            noted(error, cx);
                        }
                        Reply::Failed(missed.failure)
                    }
                    Err(error) => {
                        log::warn!("control: cannot queue: {error:#}");
                        Reply::Failed(Failure::Malformed)
                    }
                };
                reply.send(answer).ok();
                this.queueing.remove(&key);
            })
            .ok();
        });
        self.queueing.insert(key, task);
    }

    /// Checks an item before anything is fetched: a link has to be a track, album, playlist or
    /// artist of the provider signed in now.
    fn source(&self, item: Item, cx: &App) -> Result<Source, Failure> {
        let link = match item {
            Item::Path(path) => return Ok(Source::Path(path)),
            Item::Link(link) => link,
        };
        let session = self.session.read(cx);
        let Some((slug, linked)) = session.read_link(&link) else {
            return Err(Failure::Unqueueable(link));
        };
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
            Linked::Track(_) | Linked::Album(_) | Linked::Playlist(_) | Linked::Artist(_) => {
                Ok(Source::Link(link, linked))
            }
            Linked::User(_) | Linked::Jam(_) | Linked::Short => Err(Failure::Unqueueable(link)),
        }
    }

    /// Queues what was fetched in one go, so the items keep their order and one toast tells
    /// of them. A lone collection keeps its origin, several mixed ones have none.
    fn place(&self, batches: Vec<Batch>, next: bool, cx: &mut App) {
        let (tracks, origin) = match <[Batch; 1]>::try_from(batches) {
            Ok([batch]) => (batch.tracks, batch.origin),
            Err(batches) => (
                batches.into_iter().flat_map(|batch| batch.tracks).collect(),
                None,
            ),
        };
        self.playback.update(cx, |playback, cx| match next {
            true => playback.play_next_all(tracks, origin, cx),
            false => playback.enqueue_all(tracks, origin, cx),
        });
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

/// The tracks a checked item stands for. A file is read by the local provider, a link
/// fetched from the provider it belongs to.
async fn fetch(
    source: Source,
    local: &Arc<dyn MusicProvider>,
    client: Option<&Arc<dyn MusicApi>>,
) -> Result<Batch, Missed> {
    let (link, linked) = match source {
        Source::Path(path) => {
            let track = local.track_from_path(&path).ok_or_else(|| Missed {
                failure: Failure::Unreadable(path.display().to_string()),
                error: None,
            })?;
            return Ok(Batch {
                tracks: vec![track],
                origin: None,
            });
        }
        Source::Link(link, linked) => (link, linked),
    };
    let Some(client) = client else {
        return Err(Missed {
            failure: Failure::SignedOut,
            error: None,
        });
    };
    let fetched = match linked {
        Linked::Track(id) => client.track(&id).await.map(|track| Batch {
            tracks: vec![track],
            origin: None,
        }),
        Linked::Album(id) => client.album_tracks(&id).await.map(|tracks| {
            let name = tracks.first().map(|track| track.album.clone());
            let origin = Origin::album(id);
            Batch {
                origin: Some(match name {
                    Some(name) => origin.named(name),
                    None => origin,
                }),
                tracks,
            }
        }),
        Linked::Playlist(id) => client.playlist_tracks(&id).await.map(|tracks| Batch {
            tracks,
            origin: Some(Origin::playlist(id)),
        }),
        Linked::Artist(id) => client.artist(&id).await.map(|artist| Batch {
            tracks: artist.top_tracks,
            origin: Some(Origin::artist(id).named(artist.name)),
        }),
        Linked::User(_) | Linked::Jam(_) | Linked::Short => {
            return Err(Missed {
                failure: Failure::Unqueueable(link),
                error: None,
            });
        }
    };
    fetched.map_err(|error| {
        log::warn!("control: cannot fetch {link}: {error:#}");
        Missed {
            failure: Failure::Unfetched(link),
            error: Some(error),
        }
    })
}
