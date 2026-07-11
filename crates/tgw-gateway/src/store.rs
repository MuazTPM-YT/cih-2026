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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Delivered-bundle set: key = bundle ID (`u128`), value = `received_at` RFC-3339 string.
const DELIVERED: TableDefinition<u128, &str> = TableDefinition::new("delivered");
/// Decoded vitals observations: key = bundle ID, value = FHIR R5 Observation JSON.
const OBSERVATIONS: TableDefinition<u128, &str> = TableDefinition::new("observations");
/// Image blob bytes: key = bundle ID, value = raw image bytes.
const IMAGES: TableDefinition<u128, &[u8]> = TableDefinition::new("images");
/// MIME type for each stored image: key = bundle ID, value = MIME string.
const IMAGE_MIME: TableDefinition<u128, &str> = TableDefinition::new("image_mime");
/// Patient identifier associated with each stored image bundle.
const IMAGE_PATIENT: TableDefinition<u128, &str> = TableDefinition::new("image_patient");
/// Per-bundle transfer state rendered by Contract-3's `/api/queue` endpoint.
const QUEUE: TableDefinition<u128, &str> = TableDefinition::new("queue");

/// Fallback MIME type when an image has no recorded `Content-Type`.
const FALLBACK_MIME: &str = "application/octet-stream";

/// Persistent queue data exposed by the Contract-3 queue endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Delivered bundle identifier.
    pub bundle_id: Uuid,
    /// `receiving`, `complete`, or `receipt_sent`.
    pub state: String,
    /// Distinct FEC symbols received so far.
    pub symbols_received: u32,
    /// Source symbols needed to decode, once known.
    pub symbols_needed: u32,
    /// First observed DATA-frame timestamp.
    pub first_seen: String,
    /// Completion timestamp, absent while still receiving.
    pub completed_at: Option<String>,
}

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
            let _ = txn.open_table(IMAGE_PATIENT)?;
            let _ = txn.open_table(QUEUE)?;
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

    /// Store every FHIR observation belonging to a vitals bundle as one JSON array.
    pub fn store_observations(&self, id: Uuid, observations: &[Value]) -> anyhow::Result<()> {
        let json = serde_json::to_string(observations).context("serialize FHIR observations")?;
        self.store_observation(id, &json)
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

    /// Store an image and the patient identifier required by Contract 3.
    pub fn store_image_with_patient(
        &self,
        id: Uuid,
        mime: &str,
        data: &[u8],
        patient_id: &str,
    ) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut img = txn.open_table(IMAGES)?;
            img.insert(id.as_u128(), data)?;
            let mut mime_table = txn.open_table(IMAGE_MIME)?;
            mime_table.insert(id.as_u128(), mime)?;
            let mut patient_table = txn.open_table(IMAGE_PATIENT)?;
            patient_table.insert(id.as_u128(), patient_id)?;
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

    /// Retrieve all FHIR observations for a vitals bundle, accepting legacy single objects.
    pub fn get_observations(&self, id: Uuid) -> anyhow::Result<Option<Vec<Value>>> {
        let Some(json) = self.get_observation(id)? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_str(&json).context("parse stored FHIR JSON")?;
        match value {
            Value::Array(items) => Ok(Some(items)),
            item => Ok(Some(vec![item])),
        }
    }

    /// Record receiver progress, preserving the original first-seen timestamp.
    pub fn record_receiving(
        &self,
        id: Uuid,
        symbols_received: u32,
        symbols_needed: u32,
        first_seen: &str,
    ) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(QUEUE)?;
            let entry = table.get(id.as_u128())?
                .map(|value| serde_json::from_str::<QueueEntry>(value.value()))
                .transpose()
                .context("parse stored queue entry")?
                .unwrap_or(QueueEntry {
                    bundle_id: id,
                    state: "receiving".to_string(),
                    symbols_received: 0,
                    symbols_needed,
                    first_seen: first_seen.to_string(),
                    completed_at: None,
                });
            let updated = QueueEntry {
                symbols_received: entry.symbols_received.max(symbols_received),
                symbols_needed,
                ..entry
            };
            let json = serde_json::to_string(&updated).context("serialize queue entry")?;
            table.insert(id.as_u128(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Atomically persist all vitals observations and mark the bundle complete.
    pub fn complete_vitals(
        &self,
        id: Uuid,
        observations: &[Value],
        received_at: &str,
    ) -> anyhow::Result<()> {
        let fhir = serde_json::to_string(observations).context("serialize FHIR observations")?;
        self.complete_bundle(id, received_at, |txn| {
            let mut observations = txn.open_table(OBSERVATIONS)?;
            observations.insert(id.as_u128(), fhir.as_str())?;
            Ok(())
        })
    }

    /// Atomically persist an image and mark the bundle complete.
    pub fn complete_image(
        &self,
        id: Uuid,
        mime: &str,
        data: &[u8],
        patient_id: &str,
        received_at: &str,
    ) -> anyhow::Result<()> {
        self.complete_bundle(id, received_at, |txn| {
            let mut images = txn.open_table(IMAGES)?;
            images.insert(id.as_u128(), data)?;
            let mut mimes = txn.open_table(IMAGE_MIME)?;
            mimes.insert(id.as_u128(), mime)?;
            let mut patients = txn.open_table(IMAGE_PATIENT)?;
            patients.insert(id.as_u128(), patient_id)?;
            Ok(())
        })
    }

    /// Mark a previously completed bundle receipt as sent.
    pub fn mark_receipt_sent(&self, id: Uuid) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(QUEUE)?;
            let entry = table
                .get(id.as_u128())?
                .map(|value| serde_json::from_str::<QueueEntry>(value.value()))
                .transpose()
                .context("parse queue entry")?;
            let Some(mut entry) = entry else {
                return Ok(());
            };
            entry.state = "receipt_sent".to_string();
            let json = serde_json::to_string(&entry).context("serialize queue entry")?;
            table.insert(id.as_u128(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// List queue records newest-first by first-seen timestamp.
    pub fn list_queue(&self) -> anyhow::Result<Vec<QueueEntry>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(QUEUE)?;
        let mut entries: Vec<QueueEntry> = Vec::new();
        for item in table.iter()? {
            let (_, value) = item?;
            entries.push(serde_json::from_str(value.value()).context("parse queue entry")?);
        }
        entries.sort_by(|a, b| b.first_seen.cmp(&a.first_seen));
        Ok(entries)
    }

    /// Look up the patient identifier associated with an image bundle.
    pub fn get_image_patient(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(IMAGE_PATIENT)?;
        Ok(table.get(id.as_u128())?.map(|value| value.value().to_string()))
    }

    fn complete_bundle(
        &self,
        id: Uuid,
        received_at: &str,
        write_payload: impl FnOnce(&redb::WriteTransaction) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        write_payload(&txn)?;
        {
            let mut delivered = txn.open_table(DELIVERED)?;
            delivered.insert(id.as_u128(), received_at)?;
            let mut queue = txn.open_table(QUEUE)?;
            let existing = queue.get(id.as_u128())?
                .map(|value| serde_json::from_str::<QueueEntry>(value.value()))
                .transpose()
                .context("parse queue entry")?;
            let entry = existing.map(|entry| QueueEntry {
                state: "complete".to_string(),
                completed_at: Some(received_at.to_string()),
                ..entry
            }).unwrap_or(QueueEntry {
                bundle_id: id,
                state: "complete".to_string(),
                symbols_received: 0,
                symbols_needed: 0,
                first_seen: received_at.to_string(),
                completed_at: Some(received_at.to_string()),
            });
            let json = serde_json::to_string(&entry).context("serialize queue entry")?;
            queue.insert(id.as_u128(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
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

    #[test]
    fn complete_vitals_keeps_every_observation_and_queue_lifecycle() {
        let s = fresh_store("vitals_lifecycle");
        let id = Uuid::new_v4();
        s.record_receiving(id, 3, 5, "2026-07-11T14:03:20Z")
            .expect("receiving");
        s.complete_vitals(
            id,
            &[serde_json::json!({"id":"first"}), serde_json::json!({"id":"second"})],
            "2026-07-11T14:03:22Z",
        )
        .expect("complete");
        assert_eq!(s.get_observations(id).expect("read").expect("present").len(), 2);
        s.mark_receipt_sent(id).expect("receipt");
        let queue = s.list_queue().expect("queue");
        assert_eq!(queue[0].state, "receipt_sent");
        assert_eq!(queue[0].symbols_received, 3);
        assert_eq!(queue[0].symbols_needed, 5);
    }

    #[test]
    fn complete_image_keeps_patient_identifier() {
        let s = fresh_store("image_patient");
        let id = Uuid::new_v4();
        s.complete_image(
            id,
            "image/jpeg",
            &[0xAB],
            "P-1023",
            "2026-07-11T14:03:22Z",
        )
        .expect("complete image");
        assert_eq!(s.get_image_patient(id).expect("patient"), Some("P-1023".into()));
    }
}
