//! SQLite-backed envelope cache.
//!
//! Stores message headers (not bodies) per `(folder, uid)` so that repeated
//! visits to the same folder avoid a full IMAP round-trip when nothing has
//! changed.  The cache is keyed on UIDVALIDITY: when the server reports a
//! different UIDVALIDITY for a folder, the cached envelopes for that folder
//! are flushed and rebuilt from scratch.
//!
//! One database per account, `cache/<hex_username>.db` in the plugin's
//! storage folder (in the unit tests, which run outside sicompass,
//! `$XDG_CACHE_HOME/sicompass/email`). A change writes only the rows it
//! touches. Two tabs are two processes on the same file: the database is in
//! WAL mode and waits for the other's write instead of failing.

use crate::MessageHeader;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a write waits for the other tab's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the cache files live.
fn cache_dir() -> Option<PathBuf> {
    if let Some(storage) = sicompass_sdk::plugin::storage_dir() {
        return Some(storage.join("cache"));
    }
    // Outside sicompass there is no storage folder. Only the tests get one
    // instead (the fake IMAP tests point `XDG_CACHE_HOME` at a scratch
    // directory); anything else runs uncached.
    #[cfg(not(test))]
    {
        None
    }
    #[cfg(test)]
    {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("sicompass").join("email"))
    }
}

pub struct EnvelopeCache {
    conn: Connection,
}

impl EnvelopeCache {
    /// Open (or create) the cache DB for `username`.
    ///
    /// Returns `None` if the cache directory cannot be created or the DB
    /// cannot be opened — the caller silently falls back to uncached IMAP.
    pub fn open(username: &str) -> Option<Self> {
        Self::open_in_dir(&cache_dir()?, username)
    }

    /// [`EnvelopeCache::open`], in `cache_dir`.
    fn open_in_dir(cache_dir: &Path, username: &str) -> Option<Self> {
        std::fs::create_dir_all(cache_dir).ok()?;
        // Safe filename: hex-encode the username bytes.
        let hex: String = username.bytes().map(|b| format!("{b:02x}")).collect();
        let cache = Self::open_at(&cache_dir.join(format!("{hex}.db")))?;
        // The JSON file the cache was kept in before. Only a copy of what the
        // server has, so it is dropped rather than carried over.
        let _ = std::fs::remove_file(cache_dir.join(format!("{hex}.json")));
        let _ = std::fs::remove_file(cache_dir.join(format!("{hex}.json.tmp")));
        Some(cache)
    }

    /// The cache kept in `path`. A file that is not a usable database is
    /// replaced by an empty one: it is only ever a copy of what the server
    /// has.
    fn open_at(path: &Path) -> Option<Self> {
        if let Some(cache) = Self::try_open(path) {
            return Some(cache);
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        Self::try_open(path)
    }

    fn try_open(path: &Path) -> Option<Self> {
        let conn = Connection::open(path).ok()?;
        conn.busy_timeout(BUSY_TIMEOUT).ok()?;
        // `journal_mode` answers with the mode it settled on, so it is a query.
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .ok()?;
        let cache = EnvelopeCache { conn };
        cache.init_schema().ok()?;
        Some(cache)
    }

    fn init_schema(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS folder_meta (
                folder       TEXT PRIMARY KEY,
                uidvalidity  INTEGER NOT NULL,
                count        INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS envelopes (
                folder    TEXT    NOT NULL,
                uid       INTEGER NOT NULL,
                from_addr TEXT    NOT NULL,
                subject   TEXT    NOT NULL,
                date      TEXT    NOT NULL,
                seen      INTEGER NOT NULL,
                flagged   INTEGER NOT NULL,
                message_id TEXT   NOT NULL DEFAULT '',
                PRIMARY KEY (folder, uid)
             );",
        )
    }

    // -----------------------------------------------------------------------
    // Read helpers
    // -----------------------------------------------------------------------

    /// Stored UIDVALIDITY for `folder`, or `None` if not cached yet.
    pub fn get_uidvalidity(&self, folder: &str) -> Option<u32> {
        self.conn
            .query_row(
                "SELECT uidvalidity FROM folder_meta WHERE folder = ?1",
                params![folder],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|v| v as u32)
    }

    /// Number of envelopes cached for `folder`.
    pub fn cached_count(&self, folder: &str) -> usize {
        self.conn
            .query_row(
                "SELECT count FROM folder_meta WHERE folder = ?1",
                params![folder],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|v| v as usize)
            .unwrap_or(0)
    }

    /// Highest cached UID for `folder`, or `None` if the folder is not cached.
    pub fn max_uid(&self, folder: &str) -> Option<u32> {
        self.conn
            .query_row(
                "SELECT MAX(uid) FROM envelopes WHERE folder = ?1",
                params![folder],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
            .map(|v| v as u32)
    }

    /// Return the `limit` most-recent envelopes (by UID descending) for `folder`.
    pub fn get_latest(&self, folder: &str, limit: usize) -> Vec<MessageHeader> {
        let mut stmt = match self.conn.prepare(
            "SELECT uid, from_addr, subject, date, seen, flagged, message_id
               FROM envelopes
              WHERE folder = ?1
           ORDER BY uid DESC
              LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![folder, limit as i64], |row| {
            Ok(MessageHeader {
                uid: row.get::<_, i64>(0)? as u32,
                from: row.get(1)?,
                subject: row.get(2)?,
                date: row.get(3)?,
                seen: row.get::<_, i64>(4)? != 0,
                flagged: row.get::<_, i64>(5)? != 0,
                message_id: row.get(6).unwrap_or_default(),
            })
        })
        .ok()
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Write helpers
    // -----------------------------------------------------------------------

    /// Delete all cached envelopes for `folder` and record the new UIDVALIDITY.
    pub fn invalidate_folder(&self, folder: &str, new_uidvalidity: u32) {
        let _ = self.in_transaction(|conn| {
            conn.execute("DELETE FROM envelopes WHERE folder = ?1", params![folder])?;
            conn.execute(
                "INSERT INTO folder_meta (folder, uidvalidity, count)
                 VALUES (?1, ?2, 0)
                 ON CONFLICT(folder) DO UPDATE SET uidvalidity = excluded.uidvalidity, count = 0",
                params![folder, new_uidvalidity as i64],
            )?;
            Ok(())
        });
    }

    /// Insert or replace a batch of envelopes and update the folder count.
    pub fn upsert_all(&self, folder: &str, headers: &[MessageHeader]) {
        let _ = self.in_transaction(|conn| {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO envelopes (folder, uid, from_addr, subject, date, seen, flagged, message_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(folder, uid) DO UPDATE SET
                   from_addr  = excluded.from_addr,
                   subject    = excluded.subject,
                   date       = excluded.date,
                   seen       = excluded.seen,
                   flagged    = excluded.flagged,
                   message_id = excluded.message_id",
            )?;
            for h in headers {
                stmt.execute(params![
                    folder,
                    h.uid as i64,
                    &h.from,
                    &h.subject,
                    &h.date,
                    h.seen as i64,
                    h.flagged as i64,
                    &h.message_id,
                ])?;
            }
            conn.execute(
                "INSERT INTO folder_meta (folder, uidvalidity, count)
                 VALUES (?1, 0, (SELECT COUNT(*) FROM envelopes WHERE folder = ?1))
                 ON CONFLICT(folder) DO UPDATE SET count = excluded.count",
                params![folder],
            )?;
            Ok(())
        });
    }

    /// Update seen/flagged status for a single cached envelope.
    pub fn update_flags(&self, folder: &str, uid: u32, seen: bool, flagged: bool) {
        self.patch_flags(folder, uid, Some(seen), Some(flagged));
    }

    /// Selectively update only the flags that were explicitly changed.
    ///
    /// `new_seen` / `new_flagged` are `None` when that flag was not touched.
    pub fn patch_flags(
        &self,
        folder: &str,
        uid: u32,
        new_seen: Option<bool>,
        new_flagged: Option<bool>,
    ) {
        if let Some(seen) = new_seen {
            let _ = self.conn.execute(
                "UPDATE envelopes SET seen = ?3 WHERE folder = ?1 AND uid = ?2",
                params![folder, uid as i64, seen as i64],
            );
        }
        if let Some(flagged) = new_flagged {
            let _ = self.conn.execute(
                "UPDATE envelopes SET flagged = ?3 WHERE folder = ?1 AND uid = ?2",
                params![folder, uid as i64, flagged as i64],
            );
        }
    }

    /// Remove a single cached envelope (after EXPUNGE).
    pub fn remove(&self, folder: &str, uid: u32) {
        let _ = self.in_transaction(|conn| {
            conn.execute(
                "DELETE FROM envelopes WHERE folder = ?1 AND uid = ?2",
                params![folder, uid as i64],
            )?;
            conn.execute(
                "UPDATE folder_meta SET count = (SELECT COUNT(*) FROM envelopes WHERE folder = ?1)
                  WHERE folder = ?1",
                params![folder],
            )?;
            Ok(())
        });
    }

    /// Run `f` in one transaction, so the other tab never sees half of it.
    fn in_transaction(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        f(&tx)?;
        tx.commit()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_in(dir: &std::path::Path) -> EnvelopeCache {
        EnvelopeCache::open_at(&dir.join("test.db")).expect("the cache opens")
    }

    fn hdr(uid: u32, subject: &str) -> MessageHeader {
        MessageHeader {
            uid,
            from: "alice@example.com".to_owned(),
            subject: subject.to_owned(),
            date: "2025-01-01".to_owned(),
            seen: true,
            flagged: false,
            message_id: format!("<msg{uid}@example.com>"),
        }
    }

    #[test]
    fn test_cache_miss_returns_none_uidvalidity() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        assert_eq!(cache.get_uidvalidity("INBOX"), None);
    }

    #[test]
    fn test_upsert_and_get_latest() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 42);
        let msgs = vec![hdr(1, "A"), hdr(2, "B"), hdr(3, "C")];
        cache.upsert_all("INBOX", &msgs);
        let latest = cache.get_latest("INBOX", 2);
        // Most-recent-first by UID.
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].uid, 3);
        assert_eq!(latest[1].uid, 2);
    }

    #[test]
    fn test_invalidate_flushes_envelopes() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 1);
        cache.upsert_all("INBOX", &[hdr(1, "A")]);
        assert_eq!(cache.get_latest("INBOX", 10).len(), 1);
        cache.invalidate_folder("INBOX", 2);
        assert_eq!(cache.get_latest("INBOX", 10).len(), 0);
        assert_eq!(cache.get_uidvalidity("INBOX"), Some(2));
    }

    #[test]
    fn test_cached_count_matches_upserted() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 5);
        cache.upsert_all("INBOX", &[hdr(1, "A"), hdr(2, "B")]);
        assert_eq!(cache.cached_count("INBOX"), 2);
    }

    #[test]
    fn test_max_uid() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 1);
        cache.upsert_all("INBOX", &[hdr(10, "A"), hdr(20, "B"), hdr(5, "C")]);
        assert_eq!(cache.max_uid("INBOX"), Some(20));
    }

    #[test]
    fn test_update_flags() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 1);
        cache.upsert_all("INBOX", &[hdr(1, "A")]);
        cache.update_flags("INBOX", 1, false, true);
        let latest = cache.get_latest("INBOX", 1);
        assert!(!latest[0].seen);
        assert!(latest[0].flagged);
    }

    #[test]
    fn test_remove_decrements_count() {
        let dir = tempdir().unwrap();
        let cache = open_in(dir.path());
        cache.invalidate_folder("INBOX", 1);
        cache.upsert_all("INBOX", &[hdr(1, "A"), hdr(2, "B")]);
        cache.remove("INBOX", 1);
        assert_eq!(cache.cached_count("INBOX"), 1);
        assert!(cache.get_latest("INBOX", 10).iter().all(|h| h.uid == 2));
    }

    #[test]
    fn test_the_cache_survives_reopening() {
        let dir = tempdir().unwrap();
        {
            let cache = open_in(dir.path());
            cache.invalidate_folder("INBOX", 7);
            cache.upsert_all("INBOX", &[hdr(1, "A"), hdr(2, "B")]);
            cache.patch_flags("INBOX", 2, Some(false), None);
        }
        let cache = open_in(dir.path());
        assert_eq!(cache.get_uidvalidity("INBOX"), Some(7));
        assert_eq!(cache.cached_count("INBOX"), 2);
        let latest = cache.get_latest("INBOX", 10);
        assert_eq!(latest[0].uid, 2);
        assert!(!latest[0].seen, "a flag change is kept too");
        assert_eq!(latest[0].message_id, "<msg2@example.com>");
    }

    #[test]
    fn test_an_unreadable_file_starts_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("test.db"), "not a database, not even close").unwrap();
        let cache = open_in(dir.path());
        assert_eq!(cache.get_uidvalidity("INBOX"), None);
        cache.upsert_all("INBOX", &[hdr(1, "A")]);
        assert_eq!(open_in(dir.path()).cached_count("INBOX"), 1);
    }

    /// Two tabs are two processes with the same account: each sees what the
    /// other wrote, and neither's write is lost to the other's.
    #[test]
    fn two_tabs_share_the_cache() {
        let dir = tempdir().unwrap();
        let first = open_in(dir.path());
        let second = open_in(dir.path());
        first.invalidate_folder("INBOX", 3);
        first.upsert_all("INBOX", &[hdr(1, "A")]);
        second.upsert_all("INBOX", &[hdr(2, "B")]);
        assert_eq!(first.cached_count("INBOX"), 2);
        second.patch_flags("INBOX", 1, None, Some(true));
        let latest = first.get_latest("INBOX", 10);
        assert_eq!(latest.len(), 2);
        assert!(latest[1].flagged, "the other tab's flag change is seen");
        assert_eq!(second.get_uidvalidity("INBOX"), Some(3));
    }

    /// The cache used to be one JSON file per account. Opening the database
    /// removes it, so it does not linger next to its replacement.
    #[test]
    fn opening_the_database_removes_the_old_json_file() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path();
        let hex: String = "old@example.com"
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        let json = cache_dir.join(format!("{hex}.json"));
        std::fs::write(&json, "{}").unwrap();
        std::fs::write(cache_dir.join(format!("{hex}.json.tmp")), "{").unwrap();

        let cache =
            EnvelopeCache::open_in_dir(cache_dir, "old@example.com").expect("the cache opens");
        cache.upsert_all("INBOX", &[hdr(1, "A")]);
        assert!(!json.exists(), "the JSON cache is removed");
        assert!(!cache_dir.join(format!("{hex}.json.tmp")).exists());
        assert!(cache_dir.join(format!("{hex}.db")).exists());
    }
}
