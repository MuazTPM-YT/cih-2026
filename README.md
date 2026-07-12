# Low-Bandwidth Telemedicine Gateway (`cih-2026`)

Resilient clinical data delivery over lossy, bandwidth-constrained links. A field client
fountain-codes clinical bundles (RaptorQ FEC) over UDP, seals them with XChaCha20-Poly1305,
and stores-and-forwards them (redb) until an authenticated `DELIVERED` receipt arrives. The
gateway decodes, dedups, persists, emits **FHIR R5** JSON, and serves a dashboard.

Full design: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). Constraints (verbatim from the
problem statement): no centralized ML, works at **>20 % packet loss** and **<64 kbps**, and a
**lightweight binary** for low-power SBCs / basic mobile devices.

## Workspace

| Crate | Role |
|---|---|
| `tgw-core` | Bundle model, wire protocol, RaptorQ FEC, AEAD envelope, config |
| `tgw-field` | Field client: keygen, send-vitals, send-image, `status`, daemon |
| `tgw-gateway` | UDP receiver/decoder, redb store, receipts, axum API + dashboard |
| `tgw-fhir` | `VitalsObservation` → FHIR R5 `Observation`; image → FHIR `Media` |
| `tgw-netsim` | Deterministic seeded lossy UDP proxy (test instrument) |

## Quick start

### 0. Install dependencies

```sh
# Rust toolchain (rust-toolchain.toml pins 1.94.x automatically on first build)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# python3 + curl are used only by the test/demo scripts (usually preinstalled)
```

### 1. Build & test

```sh
./ci.sh                              # fmt --check + clippy -D warnings + test --workspace
cargo build --release --workspace    # release binaries → target/release/{tgw-field,tgw-gateway,tgw-netsim}
```

### 2. Run everything on one machine (full demo topology)

Four terminals, in this order:

```sh
# T1 — hospital gateway: UDP receiver :47000 + dashboard/API :8080
./target/release/tgw-field keygen --out keys/device-a.key        # once, shared PSK
./target/release/tgw-gateway --config config/gateway.toml --static-dir hospital-ui

# T2 — network degrader in the field→hospital path, with live slider control on :8088
./target/release/tgw-netsim --listen 127.0.0.1:47010 --forward 127.0.0.1:47000 \
    --loss 0.25 --rate 56000 --control-http 127.0.0.1:8088

# T3 — field agent: capture UI + REAL seal→RaptorQ→UDP send path, via the degraded link
# (TGW_LISTEN_ADDR=…:0 avoids colliding with the gateway's UDP port on one machine)
TGW_GATEWAY_ADDR=127.0.0.1:47010 TGW_LISTEN_ADDR=127.0.0.1:0 \
    ./target/release/tgw-field --config config/field.toml \
    serve --http 127.0.0.1:8091 --ui-dir field-ui

# T4 — static server for the metrics dashboard (any static server works)
python3 -m http.server 8090
```

Then open:

| URL | What you see |
|---|---|
| `http://localhost:8091/` | **Field capture UI** — submit vitals; offline-first outbox + true delivery states |
| `http://localhost:8080/` | **Hospital dashboard** — FHIR observations arriving over the lossy link |
| `http://localhost:8090/metrics-ui/` | **Link Console** — sliders drive the REAL netsim loss/bandwidth; live graphs of bytes attempted vs acknowledged |

Drag the *Packet loss* slider to 40% and watch: transmission keeps succeeding, the
attempted-vs-acked lines diverge (that gap **is** the FEC overhead paying for the loss),
and nothing is ever lost — captures made during a *Blackout* preset sit safely in the
queue and deliver the moment the link returns.

### 3. Two machines on one LAN

```sh
# Hospital laptop (prints its IP + the exact client command):
demo/run-hospital.sh
# Field laptop (after copying keys/device-a.key to the same path):
demo/run-client.sh <HOSPITAL_IP>
```

### 4. Different networks (WAN) — pair with a code, no key files

The hospital port-forwards one UDP port (e.g. 47000) and sets `net.public_addr` in
`config/gateway.toml`. Then:

```sh
# Hospital:
./target/release/tgw-gateway --config config/gateway.toml pair
#   → Pairing code: 4-otter-cobalt
#   → Field runs:   tgw-field pair "tgw1:203.0.113.5:47000:4-otter-cobalt"

# Field (any network, e.g. cellular):
./target/release/tgw-field pair "tgw1:203.0.113.5:47000:4-otter-cobalt"
./target/release/tgw-field daemon            # delivers via the paired session key
```

The SPAKE2 handshake derives the session key from the code without ever transmitting
it; a wrong code fails key confirmation and no data moves. See §Cross-LAN pairing below.

### 5. Verify the extreme-network claims locally

```sh
scripts/preflight.sh          # build + tests + 25% loss/56 kbps cell + blackout recovery
scripts/stress_e2e.sh         # full sweep: loss 0→100%, corruption, bursts, live peer relay
LOSS=0.4 scripts/stress_e2e.sh single    # one ad-hoc cell at 40% loss
```

`preflight.sh` exits non-zero if any check fails — run it before the demo.

## Offline-first: what happens when connectivity dies

Three independent layers hold data until an **authenticated receipt** proves delivery:

1. **Browser outbox** (`field-ui`, localStorage) — if the local field agent itself is
   unreachable, captures persist in the browser and auto-flush with exponential backoff
   and on the `online` event. The card honestly shows *Offline — saved locally*.
2. **Device queue** (`tgw-field`, redb) — every bundle survives crash/reboot and is
   drained by the daemon; a bundle that exhausts retries is flagged `STUCK` and kept,
   never dropped. The `wait=false` capture API acknowledges as soon as this durable
   write lands, then the UI polls `/api/status` for the true state.
3. **Transport receipts** — a bundle is only cleared by an AEAD-authenticated
   `DELIVERED` receipt from the gateway; the UI never shows *Delivered ✓* without one.

## Lightweight-binary metrics

Measured on x86-64 Linux from the `[profile.release]` build (`lto = true`,
`codegen-units = 1`, `strip = true`):

| Binary | Release size (stripped) | Idle RSS |
|---|---|---|
| `tgw-field` | ~2.9 MB | ~4.4 MB |
| `tgw-gateway` | ~3.8 MB | — |

Idle RSS is the resident set of `tgw-field daemon` sitting idle (drained queue, no transfer),
read from `/proc/<pid>/status` `VmRSS`.

### Cross-compiling for aarch64 SBCs

Pure-Rust dependencies were chosen deliberately (`lz4_flex` over zstd C bindings, RustCrypto
over `ring`) so a static musl build is a one-liner:

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl -p tgw-field
```

## Cross-LAN pairing (no key files)

The field and hospital can connect across **different networks** (the open internet), not just
the same LAN — over the same custom UDP/RaptorQ transport, with no hand-typed keys.

1. The hospital operator **port-forwards one UDP port** (e.g. 47000) on their router to the
   hospital machine, and sets `net.public_addr` in `config/gateway.toml`.
2. Hospital: `tgw-gateway pair` — prints a short code and the exact command to run:
   ```
   Pairing code: 4-otter-cobalt
   Field runs:   tgw-field pair "tgw1:203.0.113.5:47000:4-otter-cobalt"
   ```
3. Field: `tgw-field pair "tgw1:<host:port>:<code>"` — runs a **SPAKE2** handshake over UDP and
   stores a fresh per-session key locally. Then `tgw-field daemon` delivers as usual.

The pairing code derives the session key on both ends without ever transmitting it; a wrong
code fails key-confirmation, so no data moves under an unconfirmed key. The gateway's public
UDP port is hardened: unauthenticated datagrams are dropped before any decoder state is
allocated (session-keyed MAC gate + in-flight caps), and the pairing responder uses a stateless
anti-spoof cookie plus a lockout after repeated bad codes. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) §6.

**Limitation:** if the hospital's ISP uses CGNAT (no forwardable public port), direct dialing
cannot work; that case would need hole-punching or a relay, which is out of scope.

## Resilience evidence

`tests/lossy_delivery.rs` proves delivery under degradation without root or a live network:

- **Library level:** clinical bundles reconstruct byte-for-byte after 25 % deterministic loss.
- **Full end-to-end:** field `deliver` → `tgw-netsim` proxy (25 % loss + burst + 64 kbps +
  jitter) → real gateway `run_udp_listener` → AEAD `DELIVERED` receipts, asserting every bundle
  lands in the gateway store within a bounded timeout, at the default 1.4× repair overhead.
