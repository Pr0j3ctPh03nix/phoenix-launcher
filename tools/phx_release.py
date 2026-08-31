#!/usr/bin/env python3
"""Seal a payload: validate the document, sign its exact bytes, prove the signature verifies.

ONE command, because there are three payloads and they must be sealed identically. Before this
existed each did it its own way — the mod in a bash block in dist's CI, the launcher in a near-copy
of that block in another repo, the game in a `--sign` flag inside its builder — and the three had
already drifted: only two validated the document, only two refused to publish unsigned, and each
handled the key differently. A payload is either sealed the way every other payload is sealed, or
it is a special case nobody remembers when the rules change.

    python tools/phx_release.py seal --manifest staging/manifest.json \\
        --pub tools/phoenix-release.pub --trusted-comment "phoenix mod v1.2.3"

THE ORDER IS THE POINT, and it is why this is one command rather than three steps a caller
sequences itself:

  1. VALIDATE first, with the reference reader-side validator, on a different code path than the
     producer that wrote the document. A signature over a broken manifest is a promise that the
     broken thing is genuinely ours — worse than no signature, because it is believed.
  2. SIGN the bytes ON DISK, never a re-serialisation. The signature covers a FILE; re-encoding a
     document parsed out of it would sign something the reader never sees.
  3. VERIFY what was just produced, against the PUBLIC half that ships to clients. This catches the
     failure nothing else can: a key rotated without its public half being republished signs
     perfectly and is refused by every client, and the only symptom is an update channel that has
     silently died.

THE KEY NEVER TOUCHES THE DISK in CI. `--key-env` (default PHOENIX_SIGNING_KEY) reads the secret
straight from the environment, so there is no keyfile to leak between the write and the delete, and
no `trap` to get wrong. `--sec` is for the by-hand game build, where the key is a file on an
offline machine and that is the whole point.

Stdlib plus `cryptography` (through phoenix_minisign), so it ships to dist via sync.py's DEV_TOOLS
and every CI reads exactly one copy of it.
"""
import argparse
import json
import os
import sys
from typing import NoReturn

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import phoenix_minisign  # noqa: E402
import validate_manifest  # noqa: E402


def die(msg) -> NoReturn:
    """Annotated NoReturn so a reader — and a type checker — can see that every check below is a
    hard gate: nothing after a failed one runs, and no half-sealed release exists."""
    sys.exit("phx_release: " + msg)


def seal(manifest_path, pub_path, trusted_comment, secret_text, sig_path=None):
    """Validate -> sign -> verify. Returns (signature path, key_id hex).

    Every failure exits non-zero with the reason named, because every one of them is a release that
    must not be published rather than a warning to read later.
    """
    if not os.path.isfile(manifest_path):
        die("no such manifest: {}".format(manifest_path))
    with open(manifest_path, "rb") as fh:
        data = fh.read()

    # 1. the document, from the reader's side
    try:
        doc = json.loads(data)
    except ValueError as e:
        die("{} is not JSON: {}".format(manifest_path, e))
    outcome, detail = validate_manifest.validate(doc)
    if outcome != "accept":
        die("the reference validator refuses this manifest — {}: {}\n"
            "  Nothing is signed. A signature over a document a reader rejects is a promise that "
            "the broken thing is ours.".format(outcome, detail))
    payload = doc.get("payload_id")
    serial = doc.get("serial")
    print("phx_release: document accepted (payload {!r}, serial {}, schema {})".format(
        payload, serial, doc.get("schema", 1)))

    # 2. the bytes as they sit on disk
    try:
        sig_text = phoenix_minisign.sign(data, secret_text, trusted_comment=trusted_comment)
    except phoenix_minisign.MinisignError as e:
        die("signing failed: {}".format(e))
    # The reader finds a signature by appending this to the document's asset name (trust.rs
    # SIG_SUFFIX); it is part of the contract, not a local naming choice.
    out = sig_path or manifest_path + ".minisig"
    with open(out, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(sig_text)

    # 3. and prove it, against the half that ships
    if not os.path.isfile(pub_path):
        die("no such public key: {}\n"
            "  It is synced from the dev superset (sync.py DEV_TOOLS). A checkout without it means "
            "the superset was not synced before this release.".format(pub_path))
    with open(pub_path, encoding="utf-8", newline="") as fh:
        pub = fh.read()
    try:
        key_id = phoenix_minisign.verify(data, sig_text, [pub])
    except phoenix_minisign.MinisignError as e:
        die("the signature just written does not verify against {}: {}\n"
            "  The signing key and the published public key disagree. Every client would refuse "
            "this release, and the only symptom would be that no update ever appears."
            .format(pub_path, e))

    print("phx_release: sealed {} ({} bytes) -> {}\n"
          "  signed by key {}\n"
          "  trusted comment: {}".format(
              manifest_path, len(data), out, key_id.hex(), trusted_comment))
    return out, key_id.hex()


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("seal", help="validate, sign and verify a payload manifest")
    s.add_argument("--manifest", required=True, help="the manifest.json to seal")
    s.add_argument("--pub", required=True, help="public key the signature is proven against")
    s.add_argument("--trusted-comment", required=True,
                   help="signed and therefore quotable, e.g. 'phoenix mod v1.2.3'")
    s.add_argument("--sec", help="secret key FILE (by-hand builds; prefer --key-env in CI)")
    s.add_argument("--key-env", default="PHOENIX_SIGNING_KEY",
                   help="env var holding the secret key text (default: %(default)s)")
    s.add_argument("--out", help="signature path (default: <manifest>.minisig)")
    a = ap.parse_args()

    if a.sec:
        if not os.path.isfile(a.sec):
            die("no such secret key file: {}".format(a.sec))
        with open(a.sec, encoding="utf-8", newline="") as fh:
            secret = fh.read()
    else:
        secret = os.environ.get(a.key_env)
        if not secret:
            die("{} is not set and no --sec was given — refusing to publish an unsigned payload.\n"
                "  A launcher installs nothing it cannot verify, so an unsigned release is one "
                "nobody can install.".format(a.key_env))

    seal(a.manifest, a.pub, a.trusted_comment, secret, a.out)


if __name__ == "__main__":
    main()
