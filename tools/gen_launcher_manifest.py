#!/usr/bin/env python3
"""The LAUNCHER payload's manifest — the third producer, and by far the smallest.

WHY IT EXISTS. `selfupdate.rs` prefers a signed launcher manifest and falls back to the
`<exe>.sha256` sidecar when a release publishes none. Nothing emitted one, so the signed path was
unreachable and every self-update in existence has been decided by that sidecar — a plain hash
published by whoever served the release, which is exactly the party a mirror lets somebody else be.
Until a signed launcher manifest ships, the launcher is the one payload signing does not protect,
and it is the payload that executes.

WHAT IT EMITS. One `files[]` entry naming the exe asset, and nothing else. There are no bundles
(one file compresses to nothing useful and would cost every client a decode pass), no options and
no `tree`. `dest` is emitted because the format requires one and it must be a legal relative path;
the reader deliberately does not consult it — a launcher payload installs nothing into a game
folder, it replaces the running binary. `selfupdate.rs` matches on `name`, the release ASSET name.

SCHEMA. Declares 2, not 3: this document uses no schema-3 feature (bundles are the only one), and a
producer declares the format it actually wrote in. Readers support 1..3, so this is the widest
compatibility the honest number allows — see tools/manifest_schema.py for why the three version
numbers are kept apart.

Stdlib only, so it ships to dist through sync.py's DEV_TOOLS like the signer and the validator, and
CI reads exactly one copy of it.

    python tools/gen_launcher_manifest.py --version v1.5.2 --serial 2000042 \\
        --exe phoenix-launcher.exe --exe-path path/to/phoenix-launcher.exe \\
        [--notes-file body.md] --out manifest.json
"""
import argparse
import hashlib
import json
import os
import sys
import time

SCHEMA = 2
PAYLOAD_ID = "launcher"

# Same convention and the same reason as the mod producer's floor: the CI value is derived from a
# build counter that RESETS if the workflow file is renamed, and a serial that walks backwards
# silently kills the update channel for everyone who already checked. The launcher has published no
# serials at all yet (no manifest ever existed), so any floor would do — matching the mod's keeps
# one rule in the reader's head instead of two.
SERIAL_FLOOR = 2000000


def die(msg):
    sys.exit("gen_launcher_manifest: " + msg)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--version", required=True, help="release tag, e.g. v1.5.2")
    ap.add_argument("--serial", required=True, type=int,
                    help="this payload's place in its own order; RAISE it on every republish, "
                         "including a rebuild of the same --version")
    ap.add_argument("--exe", required=True,
                    help="the release ASSET name the reader matches on (e.g. phoenix-launcher.exe)")
    ap.add_argument("--exe-path", required=True, help="the built exe to hash")
    ap.add_argument("--notes-file", help="release notes to embed for the updater's What's new")
    ap.add_argument("--out", required=True, help="manifest.json to write")
    a = ap.parse_args()

    if a.serial < SERIAL_FLOOR:
        die("--serial {} is below the {} floor. If CI produced this, check whether the workflow "
            "was renamed (a build counter resets).".format(a.serial, SERIAL_FLOOR))
    if not os.path.isfile(a.exe_path):
        die("--exe-path {} does not exist".format(a.exe_path))
    # A `dest` has to be legal for the document to validate at all, and the asset name is the only
    # sensible one. Rejected here rather than in the reader, which would report it as a broken
    # release long after this could have said which input was wrong.
    if "/" in a.exe or "\\" in a.exe or a.exe in ("", ".", ".."):
        die("--exe {!r} must be a bare asset name".format(a.exe))

    ver = a.version[1:] if a.version.startswith("v") else a.version
    notes = None
    if a.notes_file:
        # utf-8-SIG: this producer runs under PowerShell, whose default `Out-File -Encoding utf8`
        # writes a BOM. Read as plain utf-8 that BOM survives into `notes` and the updater renders
        # a stray glyph at the top of every "What's new" panel. Harmless on a file without one.
        with open(a.notes_file, encoding="utf-8-sig") as fh:
            notes = fh.read().strip() or None

    manifest = {
        "schema": SCHEMA,
        "version": ver,
        "payload_id": PAYLOAD_ID,
        "serial": a.serial,
        # Advisory. `serial` orders releases; a clock can move backwards and a build machine's need
        # not agree with anyone else's.
        "signed_at": int(time.time()),
        "notes": notes,
        "files": [{
            "dest": a.exe,
            "name": a.exe,
            "sha256": sha256(a.exe_path),
            "size": os.path.getsize(a.exe_path),
        }],
        # Always empty, like every other producer's — a documented gap in the spec, not an oversight
        # to fix here alone.
        "remove": [],
    }

    out_dir = os.path.dirname(os.path.abspath(a.out))
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(a.out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(manifest, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print("gen_launcher_manifest: wrote {} (schema {}, version {}, serial {}, {} -> {})".format(
        a.out, SCHEMA, ver, a.serial, a.exe, manifest["files"][0]["sha256"][:12]))


if __name__ == "__main__":
    main()
