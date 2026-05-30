//! Metadata handling with `lofty` and `rusqlite` (skeleton)

use anyhow::Result;
#[cfg(feature = "tags")]
use lofty::TaggedFileExt;

#[derive(Debug, Clone)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
}

impl Tags {
    pub fn empty() -> Self {
        Self {
            title: None,
            artist: None,
            album: None,
            track: None,
            disc: None,
        }
    }
}

#[cfg(feature = "tags")]
pub fn read_tags<P: AsRef<std::path::Path>>(path: P) -> Result<Tags> {
    // Use lofty to read tags when enabled.
    let p = path.as_ref();
    let tagged = lofty::read_from_path(p)?;
    let tag = tagged.primary_tag();
    let mut t = Tags::empty();
    if let Some(tag) = tag {
        t.title = tag
            .get_string(&lofty::ItemKey::TrackTitle)
            .map(|s| s.to_string());
        t.artist = tag
            .get_string(&lofty::ItemKey::TrackArtist)
            .map(|s| s.to_string());
        t.album = tag
            .get_string(&lofty::ItemKey::AlbumTitle)
            .map(|s| s.to_string());
    }
    Ok(t)
}

#[cfg(not(feature = "tags"))]
pub fn read_tags<P: AsRef<std::path::Path>>(_path: P) -> Result<Tags> {
    Ok(Tags::empty())
}

#[cfg(feature = "db")]
pub mod cache {
    use super::Tags;
    use anyhow::Result;
    use rusqlite::{params, Connection};

    pub struct Cache {
        conn: Connection,
    }

    impl Cache {
        pub fn open<P: AsRef<std::path::Path>>(p: P) -> Result<Self> {
            let conn = Connection::open(p)?;
            conn.execute(
				"CREATE TABLE IF NOT EXISTS tags (path TEXT PRIMARY KEY, mtime INTEGER, size INTEGER, title TEXT, artist TEXT, album TEXT)",
				[],
			)?;
            Ok(Self { conn })
        }

        pub fn get(&self, path: &str) -> Result<Option<Tags>> {
            let mut stmt = self
                .conn
                .prepare("SELECT title, artist, album FROM tags WHERE path = ?1")?;
            let mut rows = stmt.query(params![path])?;
            if let Some(r) = rows.next()? {
                let title: Option<String> = r.get(0)?;
                let artist: Option<String> = r.get(1)?;
                let album: Option<String> = r.get(2)?;
                return Ok(Some(Tags {
                    title,
                    artist,
                    album,
                    track: None,
                    disc: None,
                }));
            }
            Ok(None)
        }

        pub fn insert(&self, path: &str, tags: &Tags) -> Result<()> {
            self.conn.execute(
				"REPLACE INTO tags (path, mtime, size, title, artist, album) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
				params![path, 0i64, 0i64, tags.title, tags.artist, tags.album],
			)?;
            Ok(())
        }
    }
}

#[cfg(not(feature = "db"))]
pub mod cache {
    // No-op cache when `db` feature is disabled.
    use super::Tags;
    use anyhow::Result;

    pub struct Cache;

    impl Cache {
        pub fn open<P: AsRef<std::path::Path>>(_p: P) -> Result<Self> {
            Ok(Cache)
        }

        pub fn get(&self, _path: &str) -> Result<Option<Tags>> {
            Ok(None)
        }

        pub fn insert(&self, _path: &str, _tags: &Tags) -> Result<()> {
            Ok(())
        }
    }
}
