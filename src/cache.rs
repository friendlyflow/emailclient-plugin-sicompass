//! The envelope cache.
//!
//! Stores message headers (not bodies) per `(folder, uid)` so that repeated
//! visits to the same folder avoid a full IMAP round-trip when nothing has
//! changed.  The cache is keyed on UIDVALIDITY: when the server reports a
//! different UIDVALIDITY for a folder, the cached envelopes for that folder
//! are flushed and rebuilt from scratch.
//!
//! One JSON file per account, `cache/<hex_username>.json` in the plugin's
//! storage folder (natively, for the tests, `$XDG_CACHE_HOME/sicompass/email`
//! or `~/.cache/sicompass/email`), rewritten whole after each change. It used
//! to be SQLite, which does not build for the sandbox without C emulation
//! libraries, and has no file locking there. There is one writer: the
//! background worker, the only copy of the plugin that fetches.

use crate::MessageHeader;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where the cache files live.
fn cache_dir() -> Option<PathBuf> {
    #[cfg(target_arch = "wasm32")]
    {
        Some(PathBuf::from(sicompass_pdk::STORAGE_DIR).join("cache"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("sicompass").join("email"))
    }
}

/// One folder's cached state.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FolderCache {
    uidvalidity: u32,
    count: usize,
    envelopes: BTreeMap<u32, MessageHeader>,
}

pub struct EnvelopeCache {
    path: PathBuf,
    folders: RefCell<BTreeMap<String, FolderCache>>,
}

impl EnvelopeCache {
    /// Open (or create) the cache for `username`.
    ///
    /// Returns `None` if the cache directory cannot be created. The caller
    /// then silently falls back to uncached IMAP.
    pub fn open(username: &str) -> Option<Self> {
        let cache_dir = cache_dir()?;
        std::fs::create_dir_all(&cache_dir).ok()?;
        // Safe filename: hex-encode the username bytes.
        let hex: String = username.bytes().map(|b| format!("{b:02x}")).collect();
        Some(Self::open_at(cache_dir.join(format!("{hex}.json"))))
    }

    /// The cache kept in `path`. A missing or unreadable file is an empty
    /// cache: it is only ever a copy of what the server has.
    fn open_at(path: PathBuf) -> Self {
        let folders = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        EnvelopeCache {
            path,
            folders: RefCell::new(folders),
        }
    }

    /// Write the cache out, through a temporary file so a crash mid-write
    /// leaves the previous version.
    fn save(&self) {
        let Ok(json) = serde_json::to_vec(&*self.folders.borrow()) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    // -----------------------------------------------------------------------
    // Read helpers
    // -----------------------------------------------------------------------

    /// Stored UIDVALIDITY for `folder`, or `None` if not cached yet.
    pub fn get_uidvalidity(&self, folder: &str) -> Option<u32> {
        self.folders.borrow().get(folder).map(|f| f.uidvalidity)
    }

    /// Number of envelopes cached for `folder`.
    pub fn cached_count(&self, folder: &str) -> usize {
        self.folders.borrow().get(folder).map_or(0, |f| f.count)
    }

    /// Highest cached UID for `folder`, or `None` if the folder is not cached.
    pub fn max_uid(&self, folder: &str) -> Option<u32> {
        let folders = self.folders.borrow();
        folders.get(folder)?.envelopes.keys().next_back().copied()
    }

    /// Return the `limit` most-recent envelopes (by UID descending) for `folder`.
    pub fn get_latest(&self, folder: &str, limit: usize) -> Vec<MessageHeader> {
        let folders = self.folders.borrow();
        let Some(f) = folders.get(folder) else {
            return vec![];
        };
        f.envelopes.values().rev().take(limit).cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Write helpers
    // -----------------------------------------------------------------------

    /// Delete all cached envelopes for `folder` and record the new UIDVALIDITY.
    pub fn invalidate_folder(&self, folder: &str, new_uidvalidity: u32) {
        self.folders.borrow_mut().insert(
            folder.to_owned(),
            FolderCache {
                uidvalidity: new_uidvalidity,
                ..Default::default()
            },
        );
        self.save();
    }

    /// Insert or replace a batch of envelopes and update the folder count.
    pub fn upsert_all(&self, folder: &str, headers: &[MessageHeader]) {
        {
            let mut folders = self.folders.borrow_mut();
            let f = folders.entry(folder.to_owned()).or_default();
            for h in headers {
                f.envelopes.insert(h.uid, h.clone());
            }
            f.count = f.envelopes.len();
        }
        self.save();
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
        {
            let mut folders = self.folders.borrow_mut();
            let Some(h) = folders
                .get_mut(folder)
                .and_then(|f| f.envelopes.get_mut(&uid))
            else {
                return;
            };
            if let Some(seen) = new_seen {
                h.seen = seen;
            }
            if let Some(flagged) = new_flagged {
                h.flagged = flagged;
            }
        }
        self.save();
    }

    /// Remove a single cached envelope (after EXPUNGE).
    pub fn remove(&self, folder: &str, uid: u32) {
        {
            let mut folders = self.folders.borrow_mut();
            let Some(f) = folders.get_mut(folder) else {
                return;
            };
            f.envelopes.remove(&uid);
            f.count = f.envelopes.len();
        }
        self.save();
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
        EnvelopeCache::open_at(dir.join("test.json"))
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
        std::fs::write(dir.path().join("test.json"), "not json").unwrap();
        let cache = open_in(dir.path());
        assert_eq!(cache.get_uidvalidity("INBOX"), None);
        cache.upsert_all("INBOX", &[hdr(1, "A")]);
        assert_eq!(open_in(dir.path()).cached_count("INBOX"), 1);
    }
}
