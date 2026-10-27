use music::{Album, Playlist, SavedArtist, Track};
use state::{Hit, Leaving, Origin, Whence};
use ui::{Pin, PinKind};

/// The origin a song dragged from the liked songs names. A playlist names itself by its id.
const LIKED: &str = "liked";

pub(crate) trait Pinned {
    fn pin(&self) -> Option<Pin>;
}

impl Pinned for Track {
    fn pin(&self) -> Option<Pin> {
        let id = self.id.clone()?;

        Some(Pin::new(PinKind::Song, id, self.name.clone()).cover(self.cover.clone()))
    }
}

impl Pinned for Album {
    fn pin(&self) -> Option<Pin> {
        let cover = self.cover_large.clone().or_else(|| self.cover.clone());

        Some(Pin::new(PinKind::Album, self.id.clone(), self.name.clone()).cover(cover))
    }
}

impl Pinned for Playlist {
    fn pin(&self) -> Option<Pin> {
        Some(
            Pin::new(PinKind::Playlist, self.id.clone(), self.name.clone())
                .cover(self.cover.clone()),
        )
    }
}

impl Pinned for SavedArtist {
    fn pin(&self) -> Option<Pin> {
        Some(
            Pin::new(PinKind::Artist, self.id.clone(), self.name.clone()).cover(self.cover.clone()),
        )
    }
}

impl Pinned for Hit {
    fn pin(&self) -> Option<Pin> {
        let (kind, id, name, cover) = match self {
            Self::Song(track) => return track.pin(),
            Self::Artist(artist) => (
                PinKind::Artist,
                artist.id.clone()?,
                artist.name.clone(),
                artist.cover.clone(),
            ),
            Self::Album(album) => (
                PinKind::Album,
                album.id.clone(),
                album.name.clone(),
                album.cover.clone(),
            ),
            Self::Playlist(playlist) => (
                PinKind::Playlist,
                playlist.id.clone(),
                playlist.name.clone(),
                playlist.cover.clone(),
            ),
        };

        Some(Pin::new(kind, id, name).cover(cover))
    }
}

/// What a song dragged from `origin` can leave when it moves: the liked songs or a playlist.
pub(crate) fn origin_tag(origin: &Origin) -> Option<String> {
    match origin.whence {
        Whence::Saved => Some(LIKED.to_owned()),
        Whence::Playlist => Some(origin.id.clone()),
        _ => None,
    }
}

/// Reads back a tag written by `origin_tag`.
pub(crate) fn leaving(tag: &str) -> Leaving {
    match tag {
        LIKED => Leaving::Liked,
        id => Leaving::Playlist(id.to_owned()),
    }
}
