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

## Build & test

```sh
./ci.sh          # cargo fmt --check + clippy -D warnings + test --workspace
cargo build --release
```

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
