"""Record actual devirt results per entry; compare against a saved census.

This mirror has no residual engine. Completion requires an emitted binary,
exit zero and zero unresolved blocks, not a zero-open experimental graph.
Protected samples are read by the analysis tool, never executed.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RECOVERED = re.compile(r"recovered (\d+) blocks \((\d+) unresolved\) in (\d+)")
ENTRY = re.compile(r"^\s*(0x[0-9a-fA-F]+)\s+\[")


def measure(exe: Path, sample: Path, entry: str, timeout: float) -> dict:
    began = time.monotonic()
    with tempfile.TemporaryDirectory() as tmp:
        binary = Path(tmp) / "function.bin"
        try:
            proc = subprocess.run([str(exe), "devirt", str(sample), entry,
                                   "--output", str(binary)], capture_output=True,
                                  text=True, encoding="utf-8", errors="replace", timeout=timeout)
        except subprocess.TimeoutExpired:
            return {"entry": entry, "complete": False, "status": "timeout",
                    "seconds": time.monotonic() - began}
        text = proc.stdout + proc.stderr
        summary = RECOVERED.search(text)
        payload = binary.read_bytes() if binary.is_file() else b""
        counts = tuple(map(int, summary.groups())) if summary else (None, None, None)
        return {"entry": entry, "exit": proc.returncode,
                "status": "ok" if proc.returncode == 0 and summary else "error",
                "blocks": counts[0], "unresolved": counts[1], "steps": counts[2],
                "complete": proc.returncode == 0 and counts[1] == 0 and bool(payload),
                "emitted_bytes": len(payload),
                "emitted_sha256": hashlib.sha256(payload).hexdigest() if payload else None,
                "seconds": time.monotonic() - began,
                "error_tail": text[-1000:] if proc.returncode != 0 or not summary else None}


def compare(rows: list[dict], baseline: dict) -> tuple[list[str], list[str], list[str]]:
    previous = {r["entry"]: r for r in baseline["per_entry"]}
    current = {r["entry"]: r for r in rows}
    lost = sorted(e for e, r in previous.items()
                  if r["complete"] and (e not in current or not current[e]["complete"]))
    gained = sorted(e for e, r in current.items()
                    if r["complete"] and e in previous and not previous[e]["complete"])
    missing = sorted(set(previous) - set(current))
    return lost, gained, missing


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sample", type=Path, required=True)
    parser.add_argument("--exe", type=Path, default=ROOT / "target/release" / (
        "tvm-devirt.exe" if sys.platform == "win32" else "tvm-devirt"))
    parser.add_argument("--entry", action="append", type=lambda v: hex(int(v, 0)))
    parser.add_argument("--timeout", type=float, default=300,
                        help="External process guard (seconds); recovery has two 120-second passes")
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    entries = args.entry
    if entries is None:
        proc = subprocess.run([str(args.exe), "entries", str(args.sample), "--all"],
                              capture_output=True, text=True, encoding="utf-8", errors="replace",
                              timeout=args.timeout, check=True)
        entries = [hex(int(m.group(1), 16)) for line in proc.stdout.splitlines()
                   if (m := ENTRY.match(line))]
    if not entries:
        parser.error("no entries found")
    rows = [measure(args.exe.resolve(), args.sample.resolve(), e, args.timeout)
            for e in dict.fromkeys(entries)]
    document = {"sample": str(args.sample.resolve()),
                "sample_sha256": hashlib.sha256(args.sample.read_bytes()).hexdigest(),
                "exe_sha256": hashlib.sha256(args.exe.read_bytes()).hexdigest(),
                "entries": len(rows), "resolved": sum(r["complete"] for r in rows),
                "incomplete_entries": [r["entry"] for r in rows if not r["complete"]],
                "per_entry": rows}
    if args.baseline:
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        if baseline["sample_sha256"] != document["sample_sha256"]:
            parser.error("baseline belongs to a different sample")
        lost, gained, missing = compare(rows, baseline)
        document.update(new_failure_entries=lost, newly_resolved_entries=gained,
                        missing_entries=missing)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({k: v for k, v in document.items() if k != "per_entry"}, indent=2))
    return int(bool(document.get("new_failure_entries") or document.get("missing_entries"))
               or any(r["status"] != "ok" for r in rows))


if __name__ == "__main__":
    raise SystemExit(main())
