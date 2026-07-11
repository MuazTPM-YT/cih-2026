//! redb-backed persistent store for delivered bundles (Phase E/F). OWNER: Twaha.
//!
//! Three concerns live here:
//! - **Dedup** (`DELIVERED` table): the set of bundle IDs that have already been fully
//!   delivered, so a re-burst after a kill-and-restart is idempotent (no second record, but
//!   still a fresh receipt).
//! - **Observations** (`OBSERVATIONS` table): each decoded vitals bundle stored as its FHIR R5
//!   JSON, keyed by bundle ID.
//! - **Images** (`IMAGES` + `IMAGE_MIME` tables): raw image bytes keyed by bundle ID, with the
//!   MIME type stored alongside so `/api/images/<id>` can set the right `Content-Type`.
//!
//! Bundle IDs key as `u128` (the natural width of a [`Uuid`]), so no custom redb `Key`/`Value`
//! impls are needed. The store is pure redb logic (no `tgw-core` dependency), so it is fully
//! unit-testable on Twaha's branch without Muaz's FEC core.

use std::path::Path;

use anyhow::Context;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use uuid::Uuid;

/// Delivered-bundle set: key = bundle ID (`u128`), value = `received_at` RFC-3339 string.
const DELIVERED: TableDefinition<u128, &str> = TableDefinition::new("delivered");
/// Decoded vitals observations: key = bundle ID, value = FHIR R5 Observation JSON.
const OBSERVATIONS: TableDefinition<u128, &str> = TableDefinition::new("observations");
/// Image blob bytes: key = bundle ID, value = raw image bytes.
const IMAGES: TableDefinition<u128, &[u8]> = TableDefinition::new("images");
/// MIME type for each stored image: key = bundle ID, value = MIME string.
const IMAGE_MIME: TableDefinition<u128, &str> = TableDefinition::new("image_mime");

/// Fallback MIME type when an image has no recorded `Content-Type`.
const FALLBACK_MIME: &str = "application/octet-stream";

/// Persistent, thread-safe store backed by a single redb database file.
#[derive(Debug)]
pub struct Store {
    db: Database,
}

impl Store {
    /// Open or create the redb database at `path`, ensuring all tables exist.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Database::create(path).context("gateway store: create/open db")?;
        let store = Self { db };
        store.ensure_tables()?;
        Ok(store)
    }

    /// Create the four tables if they do not already exist (idempotent).
    fn ensure_tables(&self) -> anyhow::Result<()> {
        let txn = self
            .db
            .begin_write()
            .context("gateway store: begin write")?;
        {
            let _ = txn.open_table(DELIVERED)?;
            let _ = txn.open_table(OBSERVATIONS)?;
            let _ = txn.open_table(IMAGES)?;
            let _ = txn.open_table(IMAGE_MIME)?;
        }
        txn.commit().context("gateway store: commit tables")?;
        Ok(())
    }

    /// True if `id` has already been recorded as delivered (the dedup check).
    pub fn is_delivered(&self, id: Uuid) -> anyhow::Result<bool> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DELIVERED)?;
        Ok(table.get(id.as_u128())?.is_some())
    }

    /// Record a bundle ID as delivered, with its `received_at` RFC-3339 timestamp.
    pub fn mark_delivered(&self, id: Uuid, received_at: &str) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(DELIVERED)?;
            table.insert(id.as_u128(), received_at)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Store a decoded vitals observation's FHIR JSON, keyed by bundle ID.
    pub fn store_observation(&self, id: Uuid, json: &str) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(OBSERVATIONS)?;
            table.insert(id.as_u128(), json)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Store an image blob (`mime` + raw bytes) keyed by bundle ID.
    pub fn store_image(&self, id: Uuid, mime: &str, data: &[u8]) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut img = txn.open_table(IMAGES)?;
            img.insert(id.as_u128(), data)?;
            let mut mime_table = txn.open_table(IMAGE_MIME)?;
            mime_table.insert(id.as_u128(), mime)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Retrieve a stored image's bytes and MIME type, or `None` if `id` is unknown.
    pub fn get_image(&self, id: Uuid) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        let txn = self.db.begin_read()?;
        let img = txn.open_table(IMAGES)?;
        let Some(bytes) = img.get(id.as_u128())? else {
            return Ok(None);
        };
        let mime = txn
            .open_table(IMAGE_MIME)?
            .get(id.as_u128())?
            .map(|g| g.value().to_string())
            .unwrap_or_else(|| FALLBACK_MIME.to_string());
        Ok(Some((mime, bytes.value().to_vec())))
    }

    /// Retrieve a stored observation's JSON, or `None` if `id` is unknown.
    pub fn get_observation(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(OBSERVATIONS)?;
        Ok(table.get(id.as_u128())?.map(|g| g.value().to_string()))
    }

    /// List all delivered bundles newest-first as `(bundle_id, received_at, kind)` tuples.
    ///
    /// `kind` is `"vitals"` if an observation record exists, `"image"` if an image record
    /// exists. The iteration walks the DELIVERED table (key = bundle-id `u128`), collects
    /// `received_at` timestamps, and sorts newest-first by timestamp string (RFC-3339 sorts
    /// lexicographically).
    pub fn list_delivered(&self) -> anyhow::Result<Vec<(Uuid, String, &'static str)>> {
        let txn = self.db.begin_read()?;
        let delivered = txn.open_table(DELIVERED)?;
        let obs_table = txn.open_table(OBSERVATIONS)?;
        let img_table = txn.open_table(IMAGES)?;

        let mut rows: Vec<(Uuid, String, &'static str)> = Vec::new();
        for entry in delivered.iter()? {
            let (key, val) = entry?;
            let id_u128 = key.value();
            let id = Uuid::from_u128(id_u128);
            let received_at = val.value().to_string();
            let kind: &'static str = if obs_table.get(id_u128)?.is_some() {
                "vitals"
            } else if img_table.get(id_u128)?.is_some() {
                "image"
            } else {
                "vitals"
            };
            rows.push((id, received_at, kind));
        }
        // Newest-first: descending RFC-3339 string sort.
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a fresh store in a temp file named `name` (distinct per test to avoid parallel
    /// collisions); any pre-existing file is removed first.
    fn fresh_store(name: &str) -> Store {
        let path = std::env::temp_dir().join(format!("tgw-store-test-{name}.redb"));
        let _ = std::fs::remove_file(&path);
        Store::open(&path).expect("open fresh store")
    }

    #[test]
    fn is_delivered_is_false_for_unseen_id() {
        let s = fresh_store("dedup_unseen");
        assert!(!s.is_delivered(Uuid::new_v4()).expect("read"));
    }

    #[test]
    fn mark_delivered_makes_is_delivered_true() {
        let s = fresh_store("dedup_mark");
        let id = Uuid::new_v4();
        s.mark_delivered(id, "2026-07-11T14:03:22Z").expect("mark");
        assert!(s.is_delivered(id).expect("read"));
    }

    #[test]
    fn re_marking_delivered_is_idempotent() {
        // A re-burst of an already-delivered bundle re-marks the same ID; no error, still true.
        let s = fresh_store("dedup_remark");
        let id = Uuid::new_v4();
        s.mark_delivered(id, "2026-07-11T14:03:22Z").expect("mark1");
        s.mark_delivered(id, "2026-07-11T14:03:25Z")
            .expect("remark");
        assert!(s.is_delivered(id).expect("read"));
    }

    #[test]
    fn store_and_get_observation_round_trips() {
        let s = fresh_store("obs_roundtrip");
        let id = Uuid::new_v4();
        let json = r#"{"resourceType":"Observation","status":"final"}"#;
        s.store_observation(id, json).expect("store");
        let got = s.get_observation(id).expect("get").expect("present");
        assert_eq!(got, json);
    }

    #[test]
    fn store_and_get_image_round_trips() {
        let s = fresh_store("img_roundtrip");
        let id = Uuid::new_v4();
        let bytes = vec![0xABu8; 25_000];
        s.store_image(id, "image/jpeg", &bytes).expect("store");
        let (mime, data) = s.get_image(id).expect("get").expect("present");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, bytes);
    }

    #[test]
    fn get_image_for_unknown_id_returns_none() {
        let s = fresh_store("img_missing");
        assert!(s.get_image(Uuid::new_v4()).expect("get").is_none());
    }
}
