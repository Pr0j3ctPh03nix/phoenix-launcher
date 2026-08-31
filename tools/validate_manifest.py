#!/usr/bin/env python3
"""Reference validation of a manifest document — the reader's parse-time obligations, executable.

Encodes exactly what a reader must check BEFORE acting on a document (R9): the schema gate, the
signing envelope, the dest-traversal refusal, the codec gate, and the DOCUMENT half of the B1-B8
bundle invariants. B4 (nothing between members, nothing after the last) is absent here on purpose:
it is a property of the DECODED stream, so only a reader that decodes can check it. It
reads only the document — never the assets — so it is fast and needs nothing but the JSON.

Two jobs:

  * cross-check what a producer emitted, on a DIFFERENT code path than the producer's own
    self-verification, so a shared bug cannot hide in both;
  * back `gen_fixtures.py --selftest`, which asserts that every conformance fixture actually
    produces the outcome index.json claims. A fixture matrix that has quietly stopped testing what
    it says it tests is worse than none at all.

    python tools/validate_manifest.py manifest.json [manifest.json ...]
    python tools/validate_manifest.py --pubkey phoenix.pub manifest.json      # + the signature

`validate` is stdlib only and stays that way; `verify_signature` is the separate, optional step and
is the only thing here that needs `cryptography`.

Exit 0 if every document is acceptable, 1 otherwise.
"""
import argparse
import json
import os
import sys

# What a CURRENT reader is expected to support. `validate` reports a schema outside this as a
# refusal rather than a defect — a manifest from the future is not malformed.
MIN_SCHEMA = 1
MAX_SCHEMA = 3
CODECS = {"zstd"}
# The payloads this build knows how to act on. Duplicated from manifest_schema.PAYLOAD_IDS rather
# than imported, for the same reason MAX_SCHEMA is not FORMAT_SCHEMA: this file is meant to be
# PORTED into a reader, and it is worth more as one page with no imports. (gen_fixtures --selftest
# asserts the two lists have not drifted apart.)
PAYLOAD_IDS = {"mod", "launcher", "game", "mirrors"}


def entries(doc):
    """Every file-bearing entry in the document — files[], toggle files, and choice variants.

    Options are included deliberately: a choice variant can be a bundle member, and variants live
    outside files[] and share a dest with their siblings. That is precisely why membership is keyed
    by content hash rather than by position or path."""
    out = list(doc.get("files", []))
    for o in doc.get("options", []):
        out += o.get("files", [])
        out += o.get("variants", [])
    return out


def unsafe_dest(d):
    """Why `d` is an unsafe install path, or None if it is safe. `dest` must be a relative,
    forward-slashed path under the game root; anything that could escape it turns a compromised
    manifest into an arbitrary file write, so a reader must REFUSE it rather than trust the producer."""
    if not isinstance(d, str) or not d:
        return "empty or non-string dest"
    if "\\" in d:
        return "backslash (dest must be forward-slashed)"
    if d.startswith("/") or (len(d) > 1 and d[1] == ":"):
        return "absolute path"
    if ".." in d.split("/"):
        return "'..' escapes the game root"
    return None


def dests(doc):
    """Every install destination in the document: files[], choice option dests, toggle files.
    (Variants have no dest of their own — they share their choice's.)"""
    out = [e["dest"] for e in doc.get("files", []) if "dest" in e]
    for o in doc.get("options", []):
        if "dest" in o:
            out.append(o["dest"])
        out += [f["dest"] for f in o.get("files", []) if "dest" in f]
    return out


def validate(doc):
    """-> (outcome, detail) where outcome is one of the conformance expectations:
    'accept' | 'refuse:schema' | 'refuse:invalid' | 'refuse:codec'.

    The vocabulary is CLOSED, so a malformed document has to come back as a verdict and never as an
    exception. That matters twice over: this file is meant to be PORTED into a reader, where a
    traceback is not one of the four things a caller can act on; and its first job is to be told
    about producer bugs, so a manifest missing a required key is the ordinary case it exists to
    catch, not an unexpected one. `_validate` below indexes required keys directly — which keeps it
    readable as a spec — and this wrapper is what makes that safe.
    """
    if not isinstance(doc, dict):
        return "refuse:invalid", "the document is not a JSON object"
    try:
        return _validate(doc)
    except (KeyError, TypeError, AttributeError, IndexError) as e:
        return "refuse:invalid", "malformed document: {} {}".format(type(e).__name__, e)


def _validate(doc):
    schema = doc.get("schema", 1)
    if isinstance(schema, bool) or not isinstance(schema, int):
        return "refuse:invalid", "`schema` is not a whole number"
    if not (MIN_SCHEMA <= schema <= MAX_SCHEMA):
        return "refuse:schema", f"schema {schema}, this build reads up to {MAX_SCHEMA}"

    # The signing envelope. Both of these are 'refuse:invalid' and NOT 'refuse:schema': a document
    # whose payload cannot be identified, or whose place in the payload's order cannot be read, is a
    # broken release, not a newer format — the reader is not missing a feature and no update fixes
    # it. (An unrecognised `payload_id` is the same answer: the set is closed, so an id we cannot
    # dispatch means the document was mis-served or rewritten, and guessing is exactly the mistake
    # signing exists to prevent.) `signed_at` is advisory and deliberately unchecked: refusing on a
    # timestamp hands anyone with a wrong clock a client that cannot update.
    payload_id = doc.get("payload_id")
    if payload_id not in PAYLOAD_IDS:
        return "refuse:invalid", (f"`payload_id` {payload_id!r} is none of {sorted(PAYLOAD_IDS)}"
                                  if payload_id is not None else "no `payload_id`")
    serial = doc.get("serial")
    if isinstance(serial, bool) or not isinstance(serial, int) or serial < 0:
        return "refuse:invalid", f"`serial` {serial!r} is not a non-negative whole number"

    for d in dests(doc):
        why = unsafe_dest(d)
        if why:
            return "refuse:invalid", f"unsafe dest {d!r}: {why}"

    size_of = {}
    for e in entries(doc):
        size_of[e["sha256"]] = e["size"]

    all_members, names = [], set()
    for b in doc.get("bundles", []):
        if b["codec"] not in CODECS:
            return "refuse:codec", f"{b['name']} uses codec {b['codec']!r}"
        if b["name"] in names:
            return "refuse:invalid", f"B8: duplicate asset name {b['name']}"
        names.add(b["name"])
        if not b["members"]:
            return "refuse:invalid", f"B7: {b['name']} has no members"
        total = 0
        for m in b["members"]:
            if m not in size_of:
                return "refuse:invalid", f"B1: {b['name']} member {m[:12]} matches no entry"
            if size_of[m] == 0:
                return "refuse:invalid", f"B6: {b['name']} carries a zero-size member"
            total += size_of[m]
        if total != b["size"]:
            return "refuse:invalid", f"B2: {b['name']} members sum to {total}, size says {b['size']}"
        all_members += b["members"]

    if len(all_members) != len(set(all_members)):
        return "refuse:invalid", "B5: a hash appears in members more than once"

    member_set = set(all_members)
    for e in entries(doc):
        if e["size"] > 0 and "name" not in e and e["sha256"] not in member_set:
            where = e.get("dest", e["sha256"][:12])
            return "refuse:invalid", f"B3: {where} has no `name` and is in no bundle"
    return "accept", ""


def verify_signature(data, minisig_text, public_keys):
    """-> (outcome, detail) where outcome is 'accept' | 'refuse:signature'.

    Kept out of `validate` on purpose, and not merely because it needs a key: the two take different
    inputs. `validate` reads a parsed DOCUMENT, while a signature covers the exact BYTES that
    arrived — re-serialising a parsed document produces a different file and would fail against its
    own signature. Which is also the order at install time: verify these bytes, then parse them.

    Only this function needs `cryptography`, so it is imported here — porting or running the
    document checks must not require a crypto library to be installed."""
    here = os.path.dirname(os.path.abspath(__file__))
    if here not in sys.path:
        sys.path.insert(0, here)
    from phoenix_minisign import MinisignError, verify
    try:
        return "accept", "signed by key " + verify(data, minisig_text, public_keys).hex()
    except MinisignError as e:
        return "refuse:signature", str(e)


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("manifest", nargs="+")
    ap.add_argument("--pubkey", action="append", metavar="PATH",
                    help="verify the signature against this key first; repeat for a key ring")
    ap.add_argument("--sig", help="default: <manifest>.minisig")
    a = ap.parse_args()
    if a.pubkey and len(a.manifest) > 1:
        sys.exit("error: --pubkey verifies one manifest at a time (each has its own .minisig)")
    if a.sig and not a.pubkey:
        sys.exit("error: --sig without --pubkey would check nothing")

    bad = 0
    for p in a.manifest:
        with open(p, "rb") as f:
            raw = f.read()
        if a.pubkey:
            keys = []
            for kp in a.pubkey:
                with open(kp, encoding="utf-8", newline="") as f:
                    keys.append(f.read())
            with open(a.sig or p + ".minisig", encoding="utf-8", newline="") as f:
                outcome, detail = verify_signature(raw, f.read(), keys)
            print(f"{outcome:<15} {p}  ({len(raw)} bytes)"
                  + (f"\n                {detail}" if detail else ""))
            if outcome != "accept":
                # Nothing below is worth doing: an unverified document has no author, so its
                # contents are whatever the last hop decided they should be.
                sys.exit(1)
        # A file that is not JSON at all is a refusal like any other — same reason validate() does
        # not raise: "unreadable" is one of the things this tool is pointed at documents to find.
        try:
            doc = json.loads(raw)
        except ValueError as e:
            bad += 1
            print(f"{'refuse:invalid':<15} {p}  ({len(raw)} bytes)\n                not JSON: {e}")
            continue
        outcome, detail = validate(doc)
        bad += outcome != "accept"
        shape = ""
        if isinstance(doc, dict):
            shape = (f"  (schema {doc.get('schema', 1)}, {len(doc.get('files', []))} files, "
                     f"{len(doc.get('bundles', []))} bundles)")
        print(f"{outcome:<15} {p}{shape}"
              + (f"\n                {detail}" if detail else ""))
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
