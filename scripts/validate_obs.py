#!/usr/bin/env python3
"""Validate what the gateway persisted against what the field client sent.

Args: <expect.tsv> <observations.json>
  expect.tsv lines: patient_id \t kind(pulse|spo2|bp) \t value \t expected_flag(may be empty)
  observations.json: the /api/observations array.

Prints: "<persisted> <flags_correct YES|NO> <corruption_leaks>"
  persisted        = obs rows whose patient_id was actually sent
  flags_correct    = YES iff every persisted row carries exactly its expected flag set
  corruption_leaks = persisted rows with a value we never sent, or a patient we never sent
                     (i.e. wrong/garbage data that slipped past AEAD + the integrity tag)
"""
import json
import sys


def main() -> None:
    expect_path, obs_path = sys.argv[1], sys.argv[2]
    expect = {}
    with open(expect_path) as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line:
                continue
            pid, kind, value, flag = (line.split("\t") + ["", "", "", ""])[:4]
            expect[pid] = {"kind": kind, "value": value, "flag": flag}

    with open(obs_path) as fh:
        try:
            obs = json.load(fh)
        except json.JSONDecodeError:
            obs = []

    persisted = 0
    leaks = 0
    flags_ok = True

    for item in obs:
        pid = item.get("patient_id", "")
        if pid not in expect:
            # A patient we never sent appearing at the gateway = corrupt/garbage data through.
            leaks += 1
            continue
        persisted += 1
        exp = expect[pid]

        # Value integrity (scalar readings only; bp is a component panel, checked by flag).
        if exp["kind"] in ("pulse", "spo2") and exp["value"]:
            got = (
                item.get("fhir", {})
                .get("valueQuantity", {})
                .get("value")
            )
            if got is None or abs(float(got) - float(exp["value"])) > 1e-9:
                leaks += 1  # persisted a value different from what was sent

        # Flag correctness: expected flag present; unexpected flags on clean rows are wrong.
        got_flags = set(item.get("flags", []) or [])
        want = {exp["flag"]} if exp["flag"] else set()
        if got_flags != want:
            flags_ok = False

    print(f"{persisted} {'YES' if flags_ok else 'NO'} {leaks}")


if __name__ == "__main__":
    main()
