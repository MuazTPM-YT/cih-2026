# Demo Cue Card — TGW Low-Bandwidth Telemedicine Gateway

> **Total time:** 4 minutes | **Laptops:** 2 | **Root required:** yes (for tc netem)

---

## Roles

| Role | Laptop | Runs |
|------|--------|------|
| **Operator A** (Laptop A — Field) | Field client + netem | `tgw-field`, `demo/knob.sh`, `demo/setup.sh` |
| **Operator B** (Laptop B — Gateway) | Gateway + dashboard | `tgw-gateway`, browser on `http://<B>:8080` |

---

## Pre-Demo Setup (before judges arrive)

### Laptop B — Gateway
```sh
tgw-gateway --config gateway.toml
# Dashboard is live at http://<B>:8080
```

### Laptop A — Network degradation + field client
```sh
# Set up degraded link ("bad rural cellular")
sudo demo/setup.sh <iface>
# → loss 25%, delay 120ms ±40ms, rate cap 64 kbit/s

# Start field daemon
tgw-field --config field.toml daemon
```

### Dashboard Configuration
The dashboard `API_BASE` setting in `app.js` must be:
- **For real demo:** `"http://<B>:8080"` (live gateway)
- **For mock fallback:** `""` (uses fixture files)

---

## The 4-Minute Script

### Step 1 — Establish the pain (30 s)

**Action (Laptop A):**
```sh
curl -T vitals.json http://<B>:8080/naive-upload --max-time 30
```

**What happens:** It stalls and times out.

**🎤 Say:**
> *"This is why standard telemedicine apps fail here — TCP treats loss as congestion."*

---

### Step 2 — Send vitals through TGW (60 s)

**Action (Laptop A):**
```sh
tgw-field send-vitals --bp 142/95 --spo2 91 --pulse 108 --patient P-1023
```

**What happens:**
- Laptop A shows: `queued → sending → delivered ✓`
- Laptop B dashboard: observation card pops with **FHIR R5 Observation**, LOINC codes, live

**Draw attention to:** The vitals card with patient ID, summary, and FHIR JSON expand button.

---

### Step 3 — Let a judge turn the dial (60 s)

**Action (invite a judge to Laptop A):**
```sh
sudo demo/knob.sh 40
```

Then send an image:
```sh
tgw-field send-image wound.jpg --patient P-1023
```

**What happens:** Progress slows in the transfer panel but completes. The photo renders on the dashboard.

**🎤 Say:**
> *"40% loss, still paced under 64 kbps — the fountain code doesn't care which packets die, only how many arrive."*

---

### Step 4 — Kill the gateway mid-transfer (60 s)

**Action:**
1. Start another image: `tgw-field send-image wound2.jpg --patient P-2048`
2. **Laptop B:** `Ctrl-C` the gateway process
3. Dashboard shows "Reconnecting…" banner (old data stays visible)
4. Wait 10 seconds
5. Restart: `tgw-gateway --config gateway.toml`

**What happens:** The bundle still lands after restart. Client queue state never lied.

**🎤 Say:**
> *"Store-and-forward with delivery receipts: the field worker knows — not hopes."*

---

### Step 5 — Close (30 s)

**Action (Laptop A or B):**
```sh
cargo test
```

**Draw attention to:** The seeded integration test output — 25% loss + burst + 64 kbps cap, repeatable.

**🎤 Say:**
> *"The demo isn't a lucky run; this is asserted in CI."*

---

## Fallback Plans

| Problem | Fallback |
|---------|----------|
| Venue LAN misbehaves | Single-laptop mode: `sudo demo/setup.sh lo` → dashboard on `localhost:8080` |
| No root access | `tgw-netsim` proxy provides the same loss/rate knobs in userspace |
| Projector trouble | `tgw-field status --watch` in a large-font terminal carries the story |

---

## Teardown

```sh
# Laptop A — remove network degradation
sudo demo/teardown.sh <iface>
```

---

## What's Tested vs. Simulated (say this proactively)

- **Genuinely tested:** delivery of vitals + images at 25% random loss with a burst episode,
  rate-capped to 64 kbps — both in the seeded integration test (`tgw-netsim`) and live
  through kernel netem.
- **Simulated for the demo:** the degraded link itself (netem stands in for a rural GSM
  link) and the vitals source (CLI-entered readings stand in for sensor hardware —
  software-only demo by team decision).
- **Documented but not demoed:** ARM cross-compile of the field binary; Android path untested.

---

## Demo Patient Identifiers

These are plausible but non-real identifiers used in the demo:

| Patient ID | Context |
|------------|---------|
| `P-1023` | Primary demo patient — vitals + wound image |
| `P-2048` | Second patient — used in gateway restart demo |
