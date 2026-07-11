//! Persistent store-and-forward queue (docs/ARCHITECTURE.md §1, muaz.md H10–14).
//!
//! Every captured bundle is sealed immediately and persisted to redb **before** the
//! first datagram leaves, so a crash, reboot, or dead battery never loses a reading.
//! Only the encrypted envelope touches disk (§6: PHI is never at rest in the clear).
//!
//! Lifecycle: `queued → sending → delivered`, plus `stuck` after `max_retries` — a
//! bundle is *never* silently dropped; `stuck` stays visible in `tgw-field status`.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tgw_core::{Bundle, BundlePayload, Priority};
use time::OffsetDateTime;
use uuid::Uuid;

/// bundle UUID bytes → CBOR-encoded [`QueuedBundle`].
const BUNDLES: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("bundles");

/// Delivery state of a queued bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleState {
    /// Persisted, waiting for its turn on the link.
    Queued,
    /// Burst in progress (reset to `Queued` on restart — crash recovery).
    Sending,
    /// Authenticated receipt received; kept for the status history.
    Delivered,
    /// Retries exhausted; kept, visible, and re-attemptable — never dropped.
    Stuck,
}

impl BundleState {
    /// Human label for the status view.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            BundleState::Queued => "queued",
            BundleState::Sending => "sending",
            BundleState::Delivered => "delivered ✓",
            BundleState::Stuck => "STUCK (kept)",
        }
    }
}

/// One persisted bundle: sealed envelope plus non-PHI metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedBundle {
    /// Bundle/delivery/dedup id.
    pub id: Uuid,
    /// Scheduling class (vitals preempt images).
    pub priority: Priority,
    /// Current lifecycle state.
    pub state: BundleState,
    /// `"vitals"` or `"image"` — for the status view; deliberately no clinical values.
    pub kind: String,
    /// The sealed (AEAD) envelope — the only payload representation on disk.
    pub envelope: Vec<u8>,
    /// Capture instant.
    pub created_at: OffsetDateTime,
    /// Exhausted delivery passes (each pass spends `retry.max_retries` re-bursts
    /// before the bundle is flagged stuck).
    pub retries: u32,
}

impl QueuedBundle {
    /// Build the persistent record for a bundle: seal now, store ciphertext only.
    pub fn from_bundle(bundle: &Bundle, key: &tgw_core::Key) -> Result<Self> {
        let envelope =
            tgw_core::seal_bundle(bundle, key).context("sealing bundle for the queue")?;
        let kind = match &bundle.payload {
            BundlePayload::Vitals(_) => "vitals",
            BundlePayload::Image { .. } => "image",
        };
        Ok(QueuedBundle {
            id: bundle.id,
            priority: bundle.priority,
            state: BundleState::Queued,
            kind: kind.to_string(),
            envelope,
            created_at: OffsetDateTime::now_utc(),
            retries: 0,
        })
    }
}

/// The redb-backed queue. One instance per field-client process.
pub struct Queue {
    db: Database,
}

impl Queue {
    /// Open (or create) the queue at `path` and run crash recovery: any bundle left in
    /// `Sending` by a previous process reverts to `Queued` so it gets re-burst.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)
            .with_context(|| format!("opening queue db {}", path.display()))?;
        let queue = Queue { db };
        queue.recover_interrupted()?;
        Ok(queue)
    }

    /// Persist a new bundle (state `Queued`).
    pub fn enqueue(&self, record: &QueuedBundle) -> Result<()> {
        self.put(record)
    }

    /// Fetch one bundle by id.
    pub fn get(&self, id: Uuid) -> Result<Option<QueuedBundle>> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BUNDLES) {
            Ok(t) => t,
            // First open before any write: the table simply doesn't exist yet.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(guard) = table.get(id.as_bytes().as_slice())? else {
            return Ok(None);
        };
        Ok(Some(decode_record(guard.value())?))
    }

    /// All bundles, newest first (the `status` view).
    pub fn list(&self) -> Result<Vec<QueuedBundle>> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(BUNDLES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            records.push(decode_record(value.value())?);
        }
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(records)
    }

    /// The next bundle the sender should work on: `Queued` only, vitals before images
    /// (priority preemption), oldest first within a class.
    pub fn next_sendable(&self) -> Result<Option<QueuedBundle>> {
        let mut candidates: Vec<QueuedBundle> = self
            .list()?
            .into_iter()
            .filter(|r| r.state == BundleState::Queued)
            .collect();
        candidates.sort_by(|a, b| {
            a.priority
                .rank()
                .cmp(&b.priority.rank())
                .then(a.created_at.cmp(&b.created_at))
        });
        Ok(candidates.into_iter().next())
    }

    /// Is there a *vitals* bundle waiting? (The preemption probe used mid-image.)
    pub fn vitals_waiting(&self) -> Result<bool> {
        Ok(self
            .list()?
            .iter()
            .any(|r| r.state == BundleState::Queued && r.priority == Priority::Vitals))
    }

    /// Transition a bundle's state (persisted immediately).
    pub fn set_state(&self, id: Uuid, state: BundleState) -> Result<()> {
        let mut record = self
            .get(id)?
            .ok_or_else(|| anyhow!("bundle {id} not in queue"))?;
        record.state = state;
        self.put(&record)
    }

    /// Increment and persist a bundle's retry counter; returns the new value.
    pub fn bump_retries(&self, id: Uuid) -> Result<u32> {
        let mut record = self
            .get(id)?
            .ok_or_else(|| anyhow!("bundle {id} not in queue"))?;
        record.retries += 1;
        let retries = record.retries;
        self.put(&record)?;
        Ok(retries)
    }

    fn recover_interrupted(&self) -> Result<()> {
        for record in self.list()? {
            if record.state == BundleState::Sending {
                tracing::info!(bundle_id = %record.id, "recovering interrupted transfer");
                self.set_state(record.id, BundleState::Queued)?;
            }
        }
        Ok(())
    }

    fn put(&self, record: &QueuedBundle) -> Result<()> {
        let mut encoded = Vec::new();
        ciborium::into_writer(record, &mut encoded).context("encoding queue record")?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BUNDLES)?;
            table.insert(record.id.as_bytes().as_slice(), encoded.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }
}

fn decode_record(bytes: &[u8]) -> Result<QueuedBundle> {
    ciborium::from_reader(bytes).context("decoding queue record")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tgw_core::Key;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tgw-queue-test-{}-{tag}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("temp dir: {e}");
        }
        dir.join("queue.redb")
    }

    fn vitals_bundle() -> Bundle {
        Bundle::new_vitals(vec![])
    }

    fn image_bundle() -> Bundle {
        Bundle::new_image("image/jpeg".into(), vec![9; 2048], "P-TEST".into())
    }

    fn must<T>(result: Result<T>, what: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => panic!("{what}: {e:#}"),
        }
    }

    #[test]
    fn survives_reopen_with_states_intact() {
        let path = temp_db("reopen");
        let key = Key::generate();
        let vitals = vitals_bundle();
        let image = image_bundle();
        {
            let queue = must(Queue::open(&path), "open");
            must(
                queue.enqueue(&must(QueuedBundle::from_bundle(&vitals, &key), "record")),
                "enqueue vitals",
            );
            must(
                queue.enqueue(&must(QueuedBundle::from_bundle(&image, &key), "record")),
                "enqueue image",
            );
            must(
                queue.set_state(vitals.id, BundleState::Delivered),
                "set state",
            );
        } // drop = process death

        let queue = must(Queue::open(&path), "reopen");
        let records = must(queue.list(), "list");
        assert_eq!(records.len(), 2, "both bundles must survive the restart");
        let recovered_vitals = must(queue.get(vitals.id), "get").map(|r| r.state);
        assert_eq!(recovered_vitals, Some(BundleState::Delivered));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn crash_recovery_resets_sending_to_queued() {
        let path = temp_db("recover");
        let key = Key::generate();
        let bundle = vitals_bundle();
        {
            let queue = must(Queue::open(&path), "open");
            must(
                queue.enqueue(&must(QueuedBundle::from_bundle(&bundle, &key), "record")),
                "enqueue",
            );
            must(
                queue.set_state(bundle.id, BundleState::Sending),
                "set state",
            );
        } // killed mid-transfer

        let queue = must(Queue::open(&path), "reopen");
        let state = must(queue.get(bundle.id), "get").map(|r| r.state);
        assert_eq!(
            state,
            Some(BundleState::Queued),
            "an interrupted transfer must resume, not dangle in `sending`"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn vitals_preempt_images_in_scheduling() {
        let path = temp_db("priority");
        let key = Key::generate();
        let queue = must(Queue::open(&path), "open");

        // Image arrives FIRST — vitals must still be scheduled ahead of it.
        let image = image_bundle();
        must(
            queue.enqueue(&must(QueuedBundle::from_bundle(&image, &key), "record")),
            "enqueue image",
        );
        let vitals = vitals_bundle();
        must(
            queue.enqueue(&must(QueuedBundle::from_bundle(&vitals, &key), "record")),
            "enqueue vitals",
        );

        let next = must(queue.next_sendable(), "next").map(|r| r.id);
        assert_eq!(next, Some(vitals.id), "vitals always go first");
        assert!(must(queue.vitals_waiting(), "probe"));

        must(
            queue.set_state(vitals.id, BundleState::Delivered),
            "deliver vitals",
        );
        let next = must(queue.next_sendable(), "next").map(|r| r.id);
        assert_eq!(next, Some(image.id), "then the image resumes");
        assert!(!must(queue.vitals_waiting(), "probe"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn envelope_on_disk_is_sealed_not_plaintext() {
        let key = Key::generate();
        let observation_marker = b"image/jpeg";
        let bundle = image_bundle();
        let record = must(QueuedBundle::from_bundle(&bundle, &key), "record");
        // The record's envelope must not contain the recognizable plaintext marker.
        assert!(
            !record
                .envelope
                .windows(observation_marker.len())
                .any(|w| w == observation_marker),
            "stored envelope leaks plaintext"
        );
    }

    #[test]
    fn retries_bump_and_persist() {
        let path = temp_db("retries");
        let key = Key::generate();
        let queue = must(Queue::open(&path), "open");
        let bundle = vitals_bundle();
        must(
            queue.enqueue(&must(QueuedBundle::from_bundle(&bundle, &key), "record")),
            "enqueue",
        );
        assert_eq!(must(queue.bump_retries(bundle.id), "bump"), 1);
        assert_eq!(must(queue.bump_retries(bundle.id), "bump"), 2);
        let record = must(queue.get(bundle.id), "get");
        assert_eq!(record.map(|r| r.retries), Some(2));
        let _ = std::fs::remove_file(&path);
    }
}
