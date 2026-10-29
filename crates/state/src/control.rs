use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cli::commands::{
    self, Item, Kind, Level, PlaybackCommand, ProviderCommand, QueueCommand, Request, SearchQuery,
    Switch,
};
use cli::wire::{self, Current, Failure, Reply, State, Value};
use futures::future;
use gpui::{App, Context, Entity, Subscription, Task};
use music::link::Linked;
use music::{Album, MediaKind, MusicApi, MusicProvider, Playlist, Track};
use tokio::sync::oneshot;

use crate::search::{self, Catalog};
use crate::{
    Hit, Io, Library, Origin, Playback, PlaybackState, QueuePlacement, Repeat, Session,
    SessionState, Shelf, join, noted,
};

/// What the three searches of a query answered: songs, albums and playlists.
type Found = (
    anyhow::Result<Vec<Track>>,
    anyhow::Result<Vec<Album>>,
    anyhow::Result<Vec<Playlist>>,
);

/// Carries out what the command line asks of the running Sonora. Playback goes through
/// `Playback`, so a device steered over Connect is the one that obeys. A command that fetches
/// tracks first answers once they are queued, so a failure reaches the terminal.
pub struct Control {
    session: Entity<Session>,
    playback: Entity<Playback>,
    library: Entity<Library>,
    /// Queue commands and searches still fetching, each removing itself once it answers.
    pending: HashMap<u64, Task<()>>,
    issued: u64,
    /// A provider switch waiting for the session to settle before it answers.
    switching: Option<Switching>,
    _session: Subscription,
}

/// A switch to the provider `slug`, answered once the session lands somewhere.
struct Switching {
    slug: String,
    name: &'static str,
    reply: oneshot::Sender<Reply>,
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
    pub fn new(
        session: Entity<Session>,
        playback: Entity<Playback>,
        library: Entity<Library>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _session = cx.observe(&session, |this, _, cx| this.settle(cx));
        Self {
            session,
            playback,
            library,
            pending: HashMap::new(),
            issued: 0,
            switching: None,
            _session,
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
            Request::Provider(command) => return self.provider(command, reply, cx),
            Request::Search(query) => return self.search(query, reply, cx),
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
        let (items, placement) = match command {
            QueueCommand::Add(items) => (items, QueuePlacement::End),
            QueueCommand::PlayNext(items) => (items, QueuePlacement::Next),
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
                        this.place(batches, placement, cx);
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
                this.pending.remove(&key);
            })
            .ok();
        });
        self.pending.insert(key, task);
    }

    /// Searches the current provider the way the search page does, ranked against the library
    /// alike, and answers with the hits that have a link to act on.
    fn search(
        &mut self,
        query: SearchQuery,
        reply: oneshot::Sender<Reply>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.session.read(cx).client() else {
            reply.send(Reply::Failed(Failure::SignedOut)).ok();
            return;
        };
        let fetch = {
            let client = client.clone();
            let asked = query.text.clone();
            Io::global(cx).spawn(async move {
                anyhow::Ok(
                    future::join3(
                        client.search(&asked),
                        client.search_albums(&asked),
                        client.search_playlists(&asked),
                    )
                    .await,
                )
            })
        };
        let key = self.issued;
        self.issued += 1;
        let task = cx.spawn(async move |this, cx| {
            let fetched = join(fetch).await;
            this.update(cx, |this, cx| {
                let answer = match fetched {
                    Ok(found) => this.hits(&query, found, client.as_ref(), cx),
                    Err(error) => {
                        log::warn!("control: cannot search: {error:#}");
                        Reply::Failed(Failure::Unfetched(query.text.clone()))
                    }
                };
                reply.send(answer).ok();
                this.pending.remove(&key);
            })
            .ok();
        });
        self.pending.insert(key, task);
    }

    /// Ranks what the provider found against the library and keeps `query.limit` hits of the
    /// kind asked for. A part of the search that failed is left out, as the search page leaves
    /// it, and only a search that failed whole is refused.
    fn hits(
        &self,
        query: &SearchQuery,
        (tracks, albums, playlists): Found,
        client: &dyn MusicApi,
        cx: &mut App,
    ) -> Reply {
        if let (Err(error), Err(_), Err(_)) = (&tracks, &albums, &playlists) {
            log::warn!("control: cannot search: {error:#}");
            noted(error, cx);
            return Reply::Failed(Failure::Unfetched(query.text.clone()));
        }
        let catalog = Catalog {
            tracks: tracks.unwrap_or_default(),
            albums: albums.unwrap_or_default(),
            playlists: playlists.unwrap_or_default(),
        };
        let library = self.library.read(cx).state(Shelf::Streaming);
        let ranked = search::rank(
            library.tracks(),
            library.albums(),
            library.playlists(),
            &catalog,
            &HashMap::new(),
            &query.text,
        );
        let hits = ranked
            .into_iter()
            .filter_map(|hit| wire_hit(hit, client))
            .filter(|hit| query.kind.is_none_or(|kind| hit.kind == kind))
            .take(query.limit.into())
            .collect();
        Reply::Value(Value::Hits(hits))
    }

    /// Lists the providers or tells the current one at once. A switch answers once the session
    /// has signed in, or failed to, so a script can go on to use the provider it asked for.
    fn provider(
        &mut self,
        command: ProviderCommand,
        reply: oneshot::Sender<Reply>,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.read(cx);
        let value = match command {
            ProviderCommand::List => Value::Providers(
                session
                    .providers()
                    .map(|info| wire::Provider {
                        slug: info.slug.to_owned(),
                        stored: info.stored,
                        current: info.active,
                    })
                    .collect(),
            ),
            ProviderCommand::Current(None) => {
                Value::Provider(session.provider_slug().map(str::to_owned))
            }
            ProviderCommand::Current(Some(slug)) => return self.switch(slug, reply, cx),
        };
        reply.send(Reply::Value(value)).ok();
    }

    fn switch(&mut self, slug: String, reply: oneshot::Sender<Reply>, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let refusal = match session.account(&slug) {
            None => Some(Failure::UnknownProvider(slug.clone())),
            Some((name, false)) => Some(Failure::NoAccount(name.to_owned())),
            Some(_) if self.switching.is_some() || session.is_pending() => Some(Failure::Busy),
            Some(_) => None,
        };
        if let Some(failure) = refusal {
            reply.send(Reply::Failed(failure)).ok();
            return;
        }
        if settled_on(session, &slug) {
            reply.send(Reply::Done).ok();
            return;
        }
        let name = session.account(&slug).map_or("", |(name, _)| name);
        self.switching = Some(Switching {
            slug: slug.clone(),
            name,
            reply,
        });
        self.session
            .update(cx, |session, cx| session.switch(&slug, cx));
    }

    /// Answers a waiting switch once the session is no longer signing in. A provider that could
    /// not sign in hands over to another, so landing elsewhere is a failure.
    fn settle(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        if session.is_pending() {
            return;
        }
        let Some(switching) = self.switching.take() else {
            return;
        };
        let answer = match settled_on(session, &switching.slug) {
            true => Reply::Done,
            false => Reply::Failed(Failure::SignInFailed(switching.name.to_owned())),
        };
        switching.reply.send(answer).ok();
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
    fn place(&self, batches: Vec<Batch>, placement: QueuePlacement, cx: &mut App) {
        let (tracks, origin) = match <[Batch; 1]>::try_from(batches) {
            Ok([batch]) => (batch.tracks, batch.origin),
            Err(batches) => (
                batches.into_iter().flat_map(|batch| batch.tracks).collect(),
                None,
            ),
        };
        self.playback.update(cx, |playback, cx| {
            playback.enqueue_tracks(tracks, origin, placement, cx)
        });
    }
}

fn play(playback: &mut Playback, command: PlaybackCommand, cx: &mut Context<Playback>) -> Reply {
    match command {
        PlaybackCommand::Play => playback.resume(cx),
        PlaybackCommand::Pause => playback.pause(cx),
        PlaybackCommand::Toggle => playback.toggle_play(cx),
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

/// A search hit as the command line lists it, or `None` for one with no link to act on, as an
/// artist drawn from a track that names no id.
fn wire_hit(hit: Hit, client: &dyn MusicApi) -> Option<wire::Hit> {
    let (kind, media, id, title, by, duration) = match hit {
        Hit::Song(track) => (
            Kind::Song,
            MediaKind::Track,
            track.id?,
            track.name,
            track.artists,
            Some(track.duration.as_secs()),
        ),
        Hit::Album(album) => (
            Kind::Album,
            MediaKind::Album,
            album.id,
            album.name,
            album.artists,
            None,
        ),
        Hit::Artist(artist) => (
            Kind::Artist,
            MediaKind::Artist,
            artist.id?,
            artist.name,
            String::new(),
            None,
        ),
        Hit::Playlist(playlist) => (
            Kind::Playlist,
            MediaKind::Playlist,
            playlist.id,
            playlist.name,
            playlist.owner,
            None,
        ),
    };
    let link = client.share_url(media, &id)?;
    Some(wire::Hit {
        kind,
        title,
        by,
        duration,
        link,
    })
}

/// Whether the session is on `slug` and usable, offline counting since the account is kept.
fn settled_on(session: &Session, slug: &str) -> bool {
    session.provider_slug() == Some(slug)
        && matches!(
            session.state(),
            SessionState::SignedIn(_) | SessionState::Offline(_)
        )
}

/// What plays now, on the steered device when there is one, or `None` when nothing is loaded.
fn current(playback: &Playback, cx: &App) -> Option<Current> {
    let track = playback.shown_track(cx)?;
    let (state, position) = match playback.steered(cx) {
        Some(steered) => {
            let state = match steered.state.playing {
                true => State::Playing,
                false => State::Paused,
            };
            (state, steered.state.live_position())
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
