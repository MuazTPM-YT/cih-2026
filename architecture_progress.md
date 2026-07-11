# CIH-2026 Architecture & Progress

## 1. Project Overview

**CIH-2026** is a Low-Bandwidth Telemedicine Gateway (Problem Statement 2): a Rust workspace implementing resilient clinical data delivery over lossy UDP links using fountain-code FEC (RaptorQ), AEAD encryption (XChaCha20-Poly1305), and store-and-forward persistence (redb). The gateway emits FHIR R5 Observation JSON and serves a dashboard over HTTP (axum).

- **Toolchain:** Rust 1.94, edition 2024, resolver 3
- **Workspace root:** `/home/lenovo/hackathon/cih-2026`
- **Branch:** `twaha/gateway` (9 commits, clean working tree, merged from `muaz/core`)
- **CI pipeline (`ci.sh`):** `cargo fmt --all --check` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace`

### Workspace members (`crates/*`)

| Crate | Owner | Role |
|---|---|---|
| `tgw-core` | Muaz | Frozen contract crate: bundle model, wire protocol, FEC, crypto envelope, config |
| `tgw-fhir` | Twaha | FHIR R5 Observation mapper |
| `tgw-gateway` | Twaha (except `static/`) | UDP receiver/decoder + axum HTTP API + redb persistence |
| `tgw-netsim` | Twaha | Deterministic seeded lossy UDP proxy (test instrument) |
| `tgw-field` | Muaz | Field-client binary: keygen, send-vitals, send-image, daemon |

### Pinned dependency versions (root `Cargo.toml`)

| Crate | Version / Features |
|---|---|
| `serde` | 1, `["derive"]` |
| `serde_json` | 1 |
| `uuid` | 1, `["v4", "serde"]` |
| `time` | 0.3, `["serde-well-known", "formatting", "parsing", "macros"]` |
| `ciborium` | 0.2.2 |
| `lz4_flex` | 0.13 |
| `chacha20poly1305` | 0.11 |
| `raptorq` | 2.0.1 |
| `redb` | 4.1 |
| `tokio` | 1, `["rt-multi-thread", "net", "macros", "time", "sync", "fs", "io-util", "signal"]` |
| `axum` | 0.8 |
| `tower-http` | 0.6, `["fs"]` |
| `tower` | 0.5, `["util"]` (dev-only) |
| `clap` | 4.6, `["derive"]` |
| `anyhow` | 1 |
| `thiserror` | 2 |
| `tracing` | 0.1 |
| `tracing-subscriber` | 0.3, `["env-filter"]` |
| `rand` | 0.8 |
| `sha2` | 0.10 |
| `toml` | 1 |

### Release profile

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
```

---

## 2. Contract Summary

### Contract 1 — `tgw-core` public API

All types are real (no `todo!()` remaining). The crate is organized into 7 modules:

**Model types** (`model.rs`):
- `Measure { value: f64, ucum_unit: String }`
- `Component { loinc: String, value: Measure }`
- `VitalsObservation { patient_id, loinc, effective: OffsetDateTime, value: Option<Measure>, components: Vec<Component>, device_id, performer_id }`
- `BundlePayload::Vitals(Vec<VitalsObservation>) | Image { mime, data, patient_id }` — note: `Image` now carries `patient_id` (Contract 1 delta, added during implementation)
- `Priority::Vitals | Image` with `rank()` method
- `Bundle { id: Uuid, priority: Priority, payload: BundlePayload }` with `new_vitals()` / `new_image()` constructors
- `Datagram = Vec<u8>`

**Config types** (`config.rs`):
- `FecConfig { symbol_size: u16, overhead_factor: f32 }`
- `Config { link, retry, net, crypto, media }` with `load(path)`, `from_toml_str(text)`, `apply_env_overrides(get)`, `validate()`, `fec()` methods
- Env overrides: `TGW_GATEWAY_ADDR`, `TGW_LISTEN_ADDR`, `TGW_HTTP_ADDR`, `TGW_KEY_FILE`, `TGW_BANDWIDTH_BPS`

**Key** (`key.rs`):
- `Key` — opaque, redacted `Debug`, no `Serialize`/`Display`
- `Key::from_bytes([u8; 32])`, `Key::generate()`, `Key::from_file(path)`, `Key::from_hex(hex)`, `Key::to_hex()`, `KEY_LEN = 32`
- Loads 64 hex characters from file; errors name the path, never echo content

**Error** (`error.rs`):
- `CoreError`: `Decode(String)`, `Encode(String)`, `Crypto`, `MalformedFrame`, `Key(String)`, `Config(String)`, `Io(#[from] std::io::Error)`

**Envelope** (`envelope.rs`):
- `seal_bundle(bundle, key) -> Result<Vec<u8>, CoreError>` — CBOR → lz4 → XChaCha20-Poly1305 (AAD = `[wire_version | bundle_uuid]`)
- `open_envelope(bundle_id, envelope, key) -> Result<Bundle, CoreError>` — reverse; rejects wrong key, wrong UUID, tampered ciphertext
- `NONCE_LEN = 24`, `TAG_LEN = 16` (crate-internal)

**Wire protocol** (`wire.rs`):
- `WIRE_VERSION = 0x01`, `FRAME_DATA = 0x01`, `FRAME_NACK = 0x02`, `FRAME_RECEIPT = 0x03`
- `Frame::Data { bundle_id } | Nack(NackFrame) | Receipt { bundle_id, delivered }`
- `NackFrame { bundle_id: Uuid, needed: Vec<u32> }`
- `parse_frame(dgram) -> Result<Frame, CoreError>` — classifies without a key
- `build_receipt(bundle_id, key) -> Vec<u8>` — AEAD-authenticated `DELIVERED` receipt (infallible, `#[must_use]`)
- `verify_receipt(dgram, key) -> Result<Uuid, CoreError>` — field client must call this before clearing a bundle
- `encode_nack(nack) -> Datagram` — NACK serialization (gateway → field)
- DATA body: `uuid(16) | OTI(12) | RaptorQ EncodingPacket`
- RECEIPT body: `uuid(16) | status(1) | nonce(24) | AEAD tag(16)`

**FEC** (`fec.rs`, 665 lines):
- `Absorb::NeedMore | Complete(Bundle) | Nack(NackFrame)` — `Nack` variant is currently never constructed by `absorb` (stalls detected by gateway timer)
- `encode_bundle(bundle, key, cfg) -> Result<Vec<Datagram>, CoreError>` — one-shot encode
- `BundleSender` — stateful sender: `new(bundle, key, cfg)`, `from_envelope(bundle_id, envelope, cfg)` (resume path), `initial_burst()`, `respond_to_nack(nack)`, `repair_burst(fraction)`, `bundle_id()`, `total_source_symbols()`
- `BundleReceiver` — per-bundle decode state machine: `new(key)`, `absorb(dgram) -> Result<Absorb, CoreError>`, `bundle_id()`, `is_complete()`, `symbols_received()`, `symbols_needed()`, `build_nack()`
- Internal states: `Idle → Active(Box<Active>) → Done(Box<Bundle>) | Failed`
- `NACK_MARGIN = 2` extra repair symbols beyond arithmetic shortfall

### Contract 2 — Wire protocol

Every datagram: `[1B version | 1B frame type | body]`. Version = `0x01`. Symbol size = 1100 bytes (fits one UDP datagram under 1500 MTU).

### Contract 3 — HTTP API shapes

Base: `http://<gateway>:8080`. Dashboard = static files at `/` (axum `ServeDir`), polling every 2s, no SSE/WebSocket.

**`GET /api/observations`** (newest-first JSON array):
- vitals: `{ bundle_id, received_at, patient_id, kind:"vitals", summary, fhir:{...R5 Observation...} }`
- image: `{ bundle_id, received_at, patient_id, kind:"image", image_url:"/api/images/<id>" }`

**`GET /api/queue`** (JSON array):
- `{ bundle_id, state:"receiving|complete|receipt_sent", symbols_received, symbols_needed, first_seen, completed_at|null }`

**`GET /api/images/<bundle_id>`** → image bytes + correct `Content-Type`

**`POST /naive-upload`** → demo-only sink (200 OK)

### Contract 4 — Config TOML

```toml
[link]      bandwidth_bps, symbol_size, overhead_factor
[retry]     nack_timeout_ms, retry_backoff_ms, max_retries
[net]       gateway_addr, listen_addr, http_addr
[crypto]    key_file
[media]     image_max_bytes
[storage]   db_path        # gateway-only extension
```

Env overrides: `TGW_GATEWAY_ADDR`, `TGW_LISTEN_ADDR`, `TGW_HTTP_ADDR`, `TGW_KEY_FILE`, `TGW_BANDWIDTH_BPS`, `TGW_DB_PATH`.

---

## 3. Crate-by-Crate Status

### 3.1 `tgw-core` (Muaz — FROZEN, fully implemented)

**Status: COMPLETE.** All `todo!()` markers replaced with real implementations. 30 unit tests + 3 raptorq spike tests, all GREEN.

| File | Lines | Role |
|---|---|---|
| `lib.rs` | 49 | Module re-exports, `#![warn(missing_docs)]`, `#![forbid(unsafe_code)]` |
| `model.rs` | 129 | Bundle/observation/priority types |
| `config.rs` | 262 | Config parsing, env overrides, validation (4 unit tests) |
| `error.rs` | 32 | `CoreError` enum (thiserror) |
| `key.rs` | 173 | PSK loading, hex round-trip, redacted Debug (5 unit tests) |
| `envelope.rs` | 211 | CBOR→lz4→AEAD seal/open (6 unit tests) |
| `wire.rs` | 392 | Frame parsing, receipt build/verify, NACK encode (7 unit tests) |
| `fec.rs` | 665 | RaptorQ encode/decode, BundleSender/BundleReceiver (8+ unit tests) |
| `tests/raptorq_spike.rs` | 161 | H1–2 API validation spike (3 tests) |

**No `todo!()` markers remain.** The crate is `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]`.

### 3.2 `tgw-fhir` (Twaha — Phase A)

**Status: COMPLETE.** 10 tests GREEN, clippy clean, fmt clean.

**File: `crates/tgw-fhir/src/lib.rs` (111 lines)**

`pub fn to_fhir_json(obs: &VitalsObservation) -> serde_json::Value`:
- Emits `resourceType:"Observation"`, `status:"final"`, `code.coding[0]` (LOINC system + code + display), `subject.reference`, `performer[0].reference`, `device.reference`, `effectiveDateTime` (RFC-3339)
- Single value + empty components → top-level `valueQuantity` (no `component`)
- Components non-empty → `component[]` (no top-level `valueQuantity`)
- Neither → valid Observation with no fabricated value, no panic
- `unit` derived from UCUM code by stripping brackets (`mm[Hg]` → `mmHg`); `code` is the raw UCUM code
- LOINC display strings provided via `loinc_display()` lookup for 5 codes: `85354-9`, `8480-6`, `8462-4`, `59408-5`, `8867-4`

**Test file: `crates/tgw-fhir/tests/r5_contract.rs` (262 lines, 10 tests)**

| # | Test fn | Asserts |
|---|---|---|
| 1 | `resource_type_and_status` | `resourceType=="Observation"`, `status=="final"` |
| 2 | `code_is_loinc_matching_observation` | LOINC system + code match obs.loinc |
| 3 | `subject_and_performer_references` | `subject.reference` + `performer[0].reference` |
| 4 | `effective_datetime_is_rfc3339_input_instant` | RFC-3339 with `Z`, exact instant |
| 5 | `single_value_uses_value_quantity_not_component` | valueQuantity present, NO component |
| 6 | `pulse_value_quantity_ucum_per_minute` | pulse UCUM code `/min` |
| 7 | `bp_panel_emits_two_components_with_correct_codes_and_units` | 2 components, NO top-level valueQuantity, `unit:"mmHg"` but `code:"mm[Hg]"` |
| 8 | `bp_matches_golden_contract_fixture` | normalize(output) == normalize(observations.json[0].fhir) |
| 9 | `round_trip_values_stable` | struct→JSON→read-back preserves coded values |
| 10 | `valueless_observation_does_not_panic_and_stays_valid` | no panic, no fabricated value |

### 3.3 `tgw-netsim` (Twaha — Phase C)

**Status: COMPLETE.** 8 tests GREEN, clippy clean, fmt clean.

**File: `crates/tgw-netsim/src/lib.rs` (235 lines)**

- `NetsimConfig { loss, burst_every, burst_len, rate_bps, jitter, seed }` — `Default` uses named constants (`DEFAULT_LOSS = 0.25`, etc.)
- `NetsimConfig::validate()` — rejects `loss` outside [0,1], zero `burst_every`, zero `rate_bps`
- `run_proxy(cfg, listen, forward)` — bidirectional via `tokio::select!`: forward path applies `LossModel` + `Pacer` + jitter; reverse path relays gateway replies (NACKs/receipts) back to remembered field address
- `Action::Forward { delay } | Drop`
- `LossModel::new(cfg)` seeds `StdRng::seed_from_u64(cfg.seed)`; `decide(elapsed)` always advances RNG via `gen_bool(loss)` first, then overrides to `true` if in burst window
- `Pacer::new(rate_bps)`; `schedule(now, packet_bits)` — token-bucket: `send_at = max(now, available_at)`, delay = `send_at - now`, `available_at = send_at + packet_bits/rate_bps`

**Test files:**

`crates/tgw-netsim/tests/loss_model.rs` (76 lines, 5 tests):

| # | Test fn | Asserts |
|---|---|---|
| 1 | `same_seed_is_deterministic` | same seed → identical drop sequence (1000 calls) |
| 2 | `different_seeds_differ` | different seeds → different patterns |
| 3 | `loss_fraction_matches_configured_rate` | ~25% drop (0.235–0.265 band, 10_000 samples) |
| 4 | `burst_window_drops_everything` | every packet inside burst window drops |
| 5 | `burst_free_window_forwards_some` | outside bursts, not everything drops |

`crates/tgw-netsim/tests/pacer.rs` (51 lines, 3 tests):

| # | Test fn | Asserts |
|---|---|---|
| 1 | `single_packet_on_idle_link_has_zero_delay` | first packet delay 0 |
| 2 | `back_to_back_packets_serialise_at_rate` | delays: `[0, 0.125, 0.25, 0.375, 0.5]` s |
| 3 | `sending_n_bits_takes_at_least_n_over_rate` | total ≥ N/rate |

### 3.4 `tgw-gateway` (Twaha — Phases B/E/F)

**Status: COMPLETE.** 12 tests GREEN (8 store unit tests + 4 api_contract), clippy clean, fmt clean.

**Files:**

`crates/tgw-gateway/src/lib.rs` (457 lines):
- `AppState { static_dir: PathBuf }` — mock mode state (for `api_contract` tests)
- `StoreState { base: AppState, store: Arc<Store> }` — redb-backed mode state
- `router(state: AppState) -> Router` — mock mode (serves `static/mock/*.json`)
- `router_with_store(state: StoreState) -> Router` — redb-backed mode (reads from `Store`)
- `run_http_server(addr, state)` / `run_http_server_store(addr, state)`
- `run_udp_listener(addr, store, key)` — UDP bind → `parse_frame` → per-bundle `BundleReceiver::absorb` → on `Complete`: dedup via `Store::is_delivered`, persist, send `build_receipt` back to source, mark receipt sent; on `Nack`: send `encode_nack` back; on `NeedMore`: record receiving progress in queue table
- `handle_complete(store, bundle) -> Result<bool>` — dedup check; re-burst of delivered bundle → returns `true` (receipt owed) but no second record
- `persist_bundle(store, bundle)` — vitals: stores ALL observations as JSON array via `store.complete_vitals`; image: stores bytes + MIME + patient_id via `store.complete_image`
- `timestamp()` — `OffsetDateTime::now_utc().format(&Rfc3339)` with `?` (no `unwrap_or_default`)
- `build_observations_json(store)` — Contract-3 shaped; iterates `list_delivered`, reads observations/images, propagates errors as `Err` (no silent `.ok()`)
- `build_queue_json(store)` — reads from `Store::list_queue()` with real state/symbol counts
- `get_image_store` — parses UUID, returns bytes + Content-Type, 404 on missing, 500 on error

`crates/tgw-gateway/src/store.rs` (482 lines):
- 6 redb tables: `DELIVERED`, `OBSERVATIONS`, `IMAGES`, `IMAGE_MIME`, `IMAGE_PATIENT`, `QUEUE` — all keyed by `u128` (UUID)
- `QueueEntry { bundle_id, state, symbols_received, symbols_needed, first_seen, completed_at: Option<String> }` — serde serialized
- Methods: `open`, `is_delivered`, `mark_delivered`, `store_observation`, `store_observations`, `store_image`, `store_image_with_patient`, `get_image`, `get_observation`, `get_observations` (accepts legacy single-object or array), `record_receiving`, `complete_vitals` (stores ALL observations as array), `complete_image` (stores bytes + MIME + patient_id), `mark_receipt_sent`, `list_queue` (newest-first by `first_seen`), `get_image_patient`, `list_delivered` (newest-first)
- `complete_bundle` — atomic transaction: writes payload + marks delivered + updates queue state to `complete` with `completed_at` timestamp
- 8 unit tests: dedup, round-trips, lifecycle (`receiving → complete → receipt_sent`), image patient, idempotency

`crates/tgw-gateway/src/main.rs` (75 lines):
- Loads `tgw_core::Config::load(&cli.config)` — real Contract-4 TOML + env overrides
- Parses `listen_addr` and `http_addr` from config
- Loads `Key::from_file(&config.crypto.key_file)`
- Resolves db path from `[storage].db_path` (with `TGW_DB_PATH` env override)
- Opens `Store::open(&db)`
- Runs `run_udp_listener` + `run_http_server_store` concurrently via `tokio::try_join!`

`crates/tgw-gateway/Cargo.toml`:
- Dependencies: tgw-core, tgw-fhir, tokio, axum, tower-http, redb, serde, serde_json, time, anyhow, clap, tracing, tracing-subscriber, uuid, thiserror, toml
- Dev-dependencies: tower (oneshot tests), serde_json, tokio

**Test file: `crates/tgw-gateway/tests/api_contract.rs` (165 lines, 4 tests)**

| # | Test fn | Asserts |
|---|---|---|
| 1 | `observations_endpoint_matches_contract` | 200, `application/json`, non-empty array; each item has required keys; at least one vitals + one image |
| 2 | `queue_endpoint_matches_contract` | 200, `application/json`, non-empty array; each item has valid state + integer symbol counts + first_seen + completed_at |
| 3 | `naive_upload_returns_200` | POST → 200 |
| 4 | `image_endpoint_is_reachable_and_never_500s` | 404 or 200, never 5xx |

### 3.5 `tgw-field` (Muaz — fully implemented)

**Status: COMPLETE.** 10 lib unit tests + 3 delivery integration tests, all GREEN.

Modules: `lib.rs`, `main.rs` (CLI: keygen, send-vitals, send-image, status, daemon), `pacer.rs`, `queue.rs` (store-and-forward with redb), `sender.rs` (deliver function), `vitals.rs` (build_observations from CLI input).

`tests/delivery.rs` (260 lines, 3 tests): end-to-end field delivery tests.

### 3.6 Integration test (Twaha — Phase D)

**File: `tests/lossy_delivery.rs` (112 lines)**

**Status: COMPLETE (GREEN, no longer `#[ignore]`d).** The test now passes with Muaz's real core.

- `vitals_and_image_survive_25pct_loss` — writes 32-byte PSK to temp file, `Key::from_file`, loops 5 vitals bundles + 1 image bundle through `encode_bundle` → `LossModel::decide` (25% deterministic drop) → `BundleReceiver::absorb` → asserts byte-identical SHA-256 digest of decoded payload
- Uses `Key::from_bytes([7u8; 32]).to_hex()` to write the key file
- `FecConfig { symbol_size: 1100, overhead_factor: 2.0 }` — 2× repair overhead for single-burst decode without NACK loop
- `BundleReceiver::new(key.clone())` — receiver now requires a Key argument

---

## 4. Phase Status Table

| Phase | Test Map Command | Status | Implemented | Remaining |
|---|---|---|---|---|
| A | `cargo test -p tgw-fhir` | **GREEN** | `to_fhir_json` with LOINC display, UCUM unit mapping, golden fixture match | — |
| B | `cargo build -p tgw-gateway` + `cargo test -p tgw-gateway` | **GREEN** | `run_udp_listener` with real `parse_frame`/`absorb`/`build_receipt`/`encode_nack` | — |
| C | `cargo test -p tgw-netsim` | **GREEN** | `LossModel` (always-advance RNG, burst override), `Pacer`, `run_proxy` (bidirectional `select!` with reverse path), `NetsimConfig::validate()` | — |
| D | `cargo test --test lossy_delivery` | **GREEN** | Test un-`#[ignore]`d, passes with Muaz's core; `Key::from_bytes().to_hex()` key file | Full gateway+proxy e2e extension (optional per spec) |
| E | `cargo test -p tgw-gateway` | **GREEN** | redb `Store` with 6 tables, dedup, receipts sent via `build_receipt`, queue lifecycle (`receiving→complete→receipt_sent`), image patient_id | — |
| F | `cargo test -p tgw-gateway` | **GREEN** | `router_with_store` + redb-backed handlers, `Config::load` in main.rs, `[storage].db_path` + `TGW_DB_PATH`, real queue state | — |
| G | grep magic numbers | **DONE** | Named constants in netsim (`DEFAULT_LOSS`, etc.); config-driven values in gateway | — |
| H | `cargo license` / `cargo deny` | **NOT STARTED** | License is `MIT OR Apache-2.0` in workspace | License audit table for README |
| I | `cargo clippy -- -D warnings` + `///` on public items | **GREEN** | clippy clean across workspace; `tgw-core` has `#![warn(missing_docs)]` | — |
| J | tracing: no PHI/keys in logs | **DONE** | All tracing sites use UUID/LOINC/state/MIME/byte-counts; `Key` has redacted Debug | — |

---

## 5. Test Baseline

### Commands and current output

```
$ cargo fmt --all --check
EXIT=0  (clean)

$ cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile [unoptimized + debuginfo] target(s)  (clean, no warnings)

$ cargo test --workspace
```

| Suite | Tests | Passed | Failed | Ignored |
|---|---|---|---|---|
| `telemed-gw` (root lib) | 0 | 0 | 0 | 0 |
| `tests/lossy_delivery.rs` | 1 | 1 | 0 | 0 |
| `tgw-core` (lib unit tests) | 30 | 30 | 0 | 0 |
| `tgw-core/tests/raptorq_spike.rs` | 3 | 3 | 0 | 0 |
| `tgw-fhir/tests/r5_contract.rs` | 10 | 10 | 0 | 0 |
| `tgw-field` (lib unit tests) | 10 | 10 | 0 | 0 |
| `tgw-field/tests/delivery.rs` | 3 | 3 | 0 | 0 |
| `tgw-gateway` (store unit tests) | 8 | 8 | 0 | 0 |
| `tgw-gateway/tests/api_contract.rs` | 4 | 4 | 0 | 0 |
| `tgw-netsim/tests/loss_model.rs` | 5 | 5 | 0 | 0 |
| `tgw-netsim/tests/pacer.rs` | 3 | 3 | 0 | 0 |
| **Total** | **77** | **77** | **0** | **0** |

**`./ci.sh` equivalent: fully green.**

---

## 6. `todo!()` Inventory

**No `todo!()` calls remain in the codebase.** All instances have been replaced with real implementations by Muaz (tgw-core, tgw-field) and Twaha (tgw-fhir, tgw-gateway, tgw-netsim). References to `todo!()` exist only in historical comments in test files documenting the TDD baseline.

---

## 7. Known Issues (from code review)

A code review was performed on the initial implementation. The following issues were identified and have been **addressed** in subsequent commits:

| Issue | Severity | Resolution |
|---|---|---|
| No reverse path in netsim | MAJOR | **Fixed**: `run_proxy` now uses `tokio::select!` with a separate `gateway_sock` that receives replies and relays them to the remembered `field_addr` |
| Multiple vitals silently discarded | MAJOR | **Fixed**: `persist_bundle` now stores ALL observations via `store.complete_vitals` (JSON array); `build_observations_json` emits one item per observation |
| API silently drops records (`.ok()??`) | MAJOR | **Fixed**: `build_observations_json` now uses `?` with `anyhow::anyhow!` error propagation; errors surface as 500 responses |
| Fabricated queue state | MAJOR | **Fixed**: `Store` now has a `QUEUE` table with `QueueEntry`; `record_receiving` / `complete_bundle` / `mark_receipt_sent` drive the real `receiving→complete→receipt_sent` lifecycle with actual symbol counts |
| FHIR display omitted | MAJOR | **Fixed**: `loinc_coding` now includes `display` via `loinc_display()` lookup for 5 supported codes |
| Receipts not sent | MAJOR | **Fixed**: `run_udp_listener` now calls `build_receipt` and sends via `sock.send_to` on `Complete`; `mark_receipt_sent` updates queue state |
| Config not loaded (hardcoded CLI defaults) | MAJOR | **Fixed**: `main.rs` now loads `tgw_core::Config::load` from TOML with env overrides; listens on config-derived addresses |
| RNG skip during burst | MINOR | **Fixed**: `decide()` now always calls `gen_bool` first, then overrides with burst check: `self.in_burst_window(elapsed) || random_drop` |
| Zero-config unsafe (panic on `burst_every==0`) | MINOR | **Fixed**: `NetsimConfig::validate()` rejects invalid values; called at top of `run_proxy` |
| `unwrap_or_default()` hiding errors | MINOR | **Fixed**: `timestamp()` now uses `?` with `.context()`; `persist_bundle` propagates errors |
| `anyhow` in library code | MINOR | **Partial**: `Store` still uses `anyhow::Result` (library); `tgw-core` correctly uses `thiserror` |
| Public re-export lacks `///` | NIT | **Fixed**: `pub use store::Store` now has `///` doc comment |
| Import grouping | NIT | **Fixed**: imports follow std → external → crate grouping |
| Magic numbers in netsim defaults | NIT | **Fixed**: defaults extracted to named `const`s with doc comments |

### Remaining items (not yet addressed)

| Item | Severity | Status |
|---|---|---|
| `Store` uses `anyhow` instead of `thiserror` | MINOR | Spec says "thiserror in libs"; `Store` is library code but uses `anyhow::Result` throughout |
| Phase H (license audit) not started | — | `cargo license` / `cargo deny check licenses` not run; no license table in README |
| Phase D full e2e extension | — | The library-level lossy delivery test passes; the fuller gateway+proxy in-process e2e (with receipts, bounded timeout, 64 kbps) is not yet added |

---

## 8. Git State

```
Branch: twaha/gateway (clean working tree)
Commits (9):
  247e273 Harden gateway data handling
  d67b1fc Clean gateway style guardrails
  9829259 Complete lossy delivery evidence
  609f104 Track gateway queue lifecycle
  27358ce Load gateway runtime config
  7ff77ca Relay netsim reverse traffic
  0fbc633 Complete FHIR R5 coding details
  b743caf Implement gateway phases A through F
  32c44ab Commit scaffold baseline
  1bc0e94 Merge pull request #2 from MuazTPM-YT/muaz/core
```

The branch is clean (no uncommitted changes). It merged `muaz/core` (PR #2) which brought the fully implemented `tgw-core` and `tgw-field` crates. Twaha's commits on top implement the FHIR, gateway, and netsim tracks.

---

## 9. Next Steps

### What needs Muaz's core — RESOLVED

Muaz's `tgw-core` is now fully implemented and merged. All `todo!()` markers are gone. The `lossy_delivery` integration test passes (no longer `#[ignore]`d). No remaining work depends on Muaz.

### What Twaha can fix independently

1. **`Store` error type**: Replace `anyhow::Result` in `store.rs` with a `thiserror::Error` enum to comply with the "thiserror in libs" guardrail.
2. **Phase H — License audit**: Run `cargo license` or `cargo deny check licenses`; produce a license table for the README; flag any non-MIT/Apache dependencies.
3. **Phase D extension**: Extend `tests/lossy_delivery.rs` into the full gateway+proxy in-process e2e: spawn `run_udp_listener` + `run_proxy` in-process at 25% loss + burst + 64 kbps, send 5 vitals + one ~25 KB image, assert receipts issued and bounded `tokio::time::timeout`.
4. **`cargo doc --no-deps`**: Verify warning-free documentation builds for Twaha's crates (`tgw-fhir`, `tgw-gateway`, `tgw-netsim`).
