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
    /// Fix F1 — when this bundle last entered `Stuck`, used by [`Queue::rearm_stuck`] to
    /// back off before the daemon auto-retries it. `#[serde(default)]` keeps records written
    /// by older builds (which lack the field) readable — they decode as `None` and re-arm
    /// immediately, which is the safe direction.
    #[serde(default)]
    pub last_stuck_at: Option<OffsetDateTime>,
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
            last_stuck_at: None,
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

    /// Fix F1 — flag a bundle `Stuck` and stamp `last_stuck_at` so the daemon's re-arm can back
    /// off before auto-retrying it. Use this instead of `set_state(id, Stuck)` on the delivery
    /// path so the backoff clock is always recorded.
    pub fn mark_stuck(&self, id: Uuid, now: OffsetDateTime) -> Result<()> {
        let mut record = self
            .get(id)?
            .ok_or_else(|| anyhow!("bundle {id} not in queue"))?;
        record.state = BundleState::Stuck;
        record.last_stuck_at = Some(now);
        self.put(&record)
    }

    /// Fix F1 — daemon auto-recovery: move eligible `Stuck` bundles back to `Queued` so they get
    /// another delivery pass (and, in daemon mode, another shot at peer-relay failover). Returns
    /// how many were re-armed.
    ///
    /// A bundle is eligible when it has spent fewer than `max_stuck_retries` passes (the existing
    /// `retries` counter is the cap, so a genuinely dead link converges to permanently `Stuck`
    /// instead of spinning) and its last stuck moment is at least `backoff` in the past. A record
    /// with no `last_stuck_at` (written by an older build) is treated as immediately eligible.
    pub fn rearm_stuck(
        &self,
        now: OffsetDateTime,
        backoff: time::Duration,
        max_stuck_retries: u32,
    ) -> Result<usize> {
        let mut rearmed = 0;
        for mut record in self.list()? {
            if record.state != BundleState::Stuck || record.retries >= max_stuck_retries {
                continue;
            }
            let due = record.last_stuck_at.is_none_or(|t| now - t >= backoff);
            if due {
                record.state = BundleState::Queued;
                self.put(&record)?;
                rearmed += 1;
            }
        }
        Ok(rearmed)
    }

    /// Fix F1 — explicit operator requeue of one `Stuck` bundle (`tgw-field requeue <id>`).
    /// Moves it to `Queued` and resets the backoff clock (`last_stuck_at = None`) so the next
    /// daemon pass picks it up immediately; the `retries` cap is left intact so a still-dead link
    /// still converges. Returns `false` if the id is unknown or the bundle is not `Stuck`.
    pub fn requeue(&self, id: Uuid) -> Result<bool> {
        let Some(mut record) = self.get(id)? else {
            return Ok(false);
        };
        if record.state != BundleState::Stuck {
            return Ok(false);
        }
        record.state = BundleState::Queued;
        record.last_stuck_at = None;
        self.put(&record)?;
        Ok(true)
    }

    /// Fix F1 — explicit operator requeue of every `Stuck` bundle (`tgw-field requeue --all`).
    /// Returns how many were moved back to `Queued`. Like [`Queue::requeue`], it resets the
    /// backoff clock and leaves `retries` intact.
    pub fn requeue_all_stuck(&self) -> Result<usize> {
        let mut requeued = 0;
        for mut record in self.list()? {
            if record.state == BundleState::Stuck {
                record.state = BundleState::Queued;
                record.last_stuck_at = None;
                self.put(&record)?;
                requeued += 1;
            }
        }
        Ok(requeued)
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
    fn rearm_stuck_respects_backoff_and_cap() {
        let path = temp_db("rearm");
        let key = Key::generate();
        let queue = must(Queue::open(&path), "open");
        let bundle = vitals_bundle();
        must(
            queue.enqueue(&must(QueuedBundle::from_bundle(&bundle, &key), "record")),
            "enqueue",
        );

        let t0 = OffsetDateTime::now_utc();
        must(queue.mark_stuck(bundle.id, t0), "mark stuck");
        let backoff = time::Duration::seconds(10);

        // Before the backoff elapses, nothing is re-armed.
        let early = must(
            queue.rearm_stuck(t0 + time::Duration::seconds(5), backoff, 3),
            "early",
        );
        assert_eq!(
            early, 0,
            "a bundle inside its backoff window is not re-armed"
        );
        assert_eq!(
            must(queue.get(bundle.id), "get").map(|r| r.state),
            Some(BundleState::Stuck)
        );

        // Past the backoff it re-arms to Queued.
        let late = must(
            queue.rearm_stuck(t0 + time::Duration::seconds(11), backoff, 3),
            "late",
        );
        assert_eq!(late, 1, "past the backoff the stuck bundle re-arms");
        assert_eq!(
            must(queue.get(bundle.id), "get").map(|r| r.state),
            Some(BundleState::Queued)
        );

        // Drive retries up to the cap; a bundle at/over the cap must never re-arm again.
        must(queue.bump_retries(bundle.id), "bump1");
        must(queue.bump_retries(bundle.id), "bump2");
        must(queue.bump_retries(bundle.id), "bump3");
        let t1 = OffsetDateTime::now_utc();
        must(queue.mark_stuck(bundle.id, t1), "re-stuck");
        let capped = must(
            queue.rearm_stuck(t1 + time::Duration::seconds(60), backoff, 3),
            "capped",
        );
        assert_eq!(
            capped, 0,
            "a bundle at the retry cap converges to permanently stuck"
        );
        assert_eq!(
            must(queue.get(bundle.id), "get").map(|r| r.state),
            Some(BundleState::Stuck)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn requeue_all_stuck_moves_only_stuck_and_resets_backoff_clock() {
        let path = temp_db("requeue");
        let key = Key::generate();
        let queue = must(Queue::open(&path), "open");

        let stuck = vitals_bundle();
        let delivered = vitals_bundle();
        let queued = image_bundle();
        for b in [&stuck, &delivered, &queued] {
            must(
                queue.enqueue(&must(QueuedBundle::from_bundle(b, &key), "record")),
                "enqueue",
            );
        }
        must(
            queue.mark_stuck(stuck.id, OffsetDateTime::now_utc()),
            "mark stuck",
        );
        must(
            queue.set_state(delivered.id, BundleState::Delivered),
            "deliver",
        );

        let n = must(queue.requeue_all_stuck(), "requeue all");
        assert_eq!(n, 1, "only the stuck bundle is requeued");
        let stuck_after = must(queue.get(stuck.id), "get stuck");
        assert_eq!(
            stuck_after.as_ref().map(|r| r.state),
            Some(BundleState::Queued)
        );
        assert!(
            stuck_after.and_then(|r| r.last_stuck_at).is_none(),
            "an operator requeue resets the backoff clock"
        );
        assert_eq!(
            must(queue.get(delivered.id), "get delivered").map(|r| r.state),
            Some(BundleState::Delivered),
            "a delivered bundle is untouched"
        );

        // Single-id requeue only acts on stuck bundles.
        assert!(
            !must(queue.requeue(delivered.id), "requeue delivered"),
            "requeue is a no-op for a non-stuck bundle"
        );
        let _ = std::fs::remove_file(&path);
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
