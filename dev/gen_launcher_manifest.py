#!/usr/bin/env python3
"""Discovery step for the launcher's release manifest: hashes the built exe and hands ONE Entry to
build_manifest.write() (release-tooling, checked out to --tooling by CI). Carries no format knowledge
of its own -- no schema number, no envelope keys -- all of that lives in phoenix_tooling.

NO SERIAL, and that is the point: what this writes is a SEAL REQUEST, which carries serial 0 and so
names no release. The signing authority assigns the real number, writes it into the document and
signs those bytes; what gets published is the document that comes back, never this one.

    python dev/gen_launcher_manifest.py --version v1.5.2 \\
        --exe phoenix-launcher.exe --exe-path path/to/phoenix-launcher.exe \\
        [--notes-file body.md] --out manifest.json [--tooling .tooling]
"""
import argparse
import hashlib
import os
import sys


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
    ap.add_argument("--exe", required=True,
                     help="the release ASSET name the reader matches on (e.g. phoenix-launcher.exe)")
    ap.add_argument("--exe-path", required=True, help="the built exe to hash")
    ap.add_argument("--notes-file", help="release notes to embed for the updater's What's new")
    ap.add_argument("--out", required=True, help="manifest.json to write")
    ap.add_argument("--tooling", default=".tooling",
                     help="the release-tooling checkout ROOT, which `phoenix_tooling` is imported "
                          "from (default: %(default)s)")
    a = ap.parse_args()

    if not os.path.isfile(a.exe_path):
        die("--exe-path {} does not exist".format(a.exe_path))
    # A `dest` has to be a legal relative path for the document to build at all, and the asset name
    # is the only sensible one -- rejected here, rather than deep inside the builder, so the failure
    # names which input was wrong.
    if "/" in a.exe or "\\" in a.exe or a.exe in ("", ".", ".."):
        die("--exe {!r} must be a bare asset name".format(a.exe))

    # The checkout ROOT, not a directory inside it: `phoenix_tooling` is a package there, and the
    # module names under it are the surface that repo promises not to move.
    sys.path.insert(0, os.path.abspath(a.tooling))
    from phoenix_tooling import build_manifest  # noqa: E402

    notes = None
    if a.notes_file:
        # utf-8-SIG: this runs after a PowerShell step, whose default `Out-File -Encoding utf8`
        # writes a BOM. Reading plain utf-8 would let that BOM survive into `notes`, and the updater
        # would render a stray glyph atop every "What's new" panel. Harmless on a file without one.
        with open(a.notes_file, encoding="utf-8-sig") as fh:
            notes = fh.read().strip() or None

    ver = a.version[1:] if a.version.startswith("v") else a.version
    exe_entry = build_manifest.Entry(a.exe, sha256(a.exe_path), os.path.getsize(a.exe_path), name=a.exe)
    # `serial` is left at write()'s default of 0 -- see the module docstring.
    doc = build_manifest.write(a.out, "launcher", ver, entries=[exe_entry], notes=notes)
    print("gen_launcher_manifest: wrote {} (schema {}, version {}, serial {} -- a seal request, "
          "{} -> {})".format(a.out, doc["schema"], ver, doc["serial"], a.exe,
                             doc["files"][0]["sha256"][:12]))


if __name__ == "__main__":
    main()
