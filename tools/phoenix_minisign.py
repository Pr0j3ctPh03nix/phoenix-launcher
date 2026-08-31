#!/usr/bin/env python3
"""minisign signatures over release documents — the `Ed` (pure Ed25519) variant, both sides.

A manifest is trusted for every sha256 it carries, yet nothing proves who wrote the manifest: TLS
proves only that some server answered. Serving releases from third-party mirrors turns that into the
whole attack surface. A signature moves the trust onto a key we hold and leaves a mirror with the one
job it can be trusted with — moving bytes.

minisign's framing rather than something ad hoc: four lines of text, a second signature over the
human-readable comment (so a mirror cannot rewrite the part a person reads out of a signature it
copied verbatim), and existing verifiers in every language a reader might be written in.

THE FORMAT is a shared contract — the CI producer and the reader implement these same four lines:

    untrusted comment: <text>                    not signed; ignore it
    base64( "Ed" | key_id[8] | sig[64] )         sig  = Ed25519(sk, the signed file's exact bytes)
    trusted comment: <text>                      signed, and therefore quotable
    base64( gsig[64] )                           gsig = Ed25519(sk, sig | trusted comment text)

Base64 is RFC 4648, padded, unwrapped; every line ends LF; the trusted comment is signed as UTF-8
with no trailing newline. `sig` covers the file's bytes AS THEY ARRIVED — never a re-serialisation of
a document parsed out of them, or the signature stops saying anything about what was delivered.

Deliberate narrowings of upstream minisign, which any second implementation has to match:

  * ONE algorithm. `Ed` is pure Ed25519; `ED` prehashes with Blake2b-512 and is REFUSED. Accepting
    both means two code paths with an attacker choosing between them, to buy nothing: prehashing
    only helps for files too big to hold, and a manifest is kilobytes.
  * NOT a narrowing, but worth stating because a written spec got it wrong once: the trusted-comment
    prefix is `trusted comment: ` with a SPACE, matching upstream's own TRUSTED_COMMENT_PREFIX in
    src/minisign.h. Interoperability is the reason this format was chosen over an ad-hoc one, so a
    prefix that only we can read would have cost exactly what it was meant to buy.
  * The secret key at rest is NOT minisign's scrypt-encrypted .key: it is the same two-line shape as
    the public key, carrying the raw 32-byte seed. The release key lives in a CI secret, and a
    passphrase no human ever types is just a second secret stored beside the first.

Needs `cryptography` — the only non-stdlib dependency in this repo's tooling, and only here; the
document-only path (validate_manifest.validate) must never pull it in. The `minisign` CLI is not
used and does not have to exist.

    python tools/phoenix_minisign.py keygen --sec phoenix.key --pub phoenix.pub
    python tools/phoenix_minisign.py sign --sec phoenix.key manifest.json
    python tools/phoenix_minisign.py verify --pub phoenix.pub manifest.json
    python tools/phoenix_minisign.py selftest
"""
import argparse
import base64
import os
import sys
import time
from collections import namedtuple

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

ALGO = b"Ed"                  # pure Ed25519 — the only algorithm this contract has
ALGO_HASHED = b"ED"           # upstream's Blake2b-prehashed variant; named only to refuse it by name
UNTRUSTED_PREFIX = "untrusted comment: "
TRUSTED_PREFIX = "trusted comment: "
KEY_ID_LEN = 8
SIG_LEN = 64
KEY_LEN = 32                  # an Ed25519 public key, and the seed a private key is built from
DEFAULT_UNTRUSTED = "signature from the phoenix release key"

PublicKey = namedtuple("PublicKey", "key_id key")     # key: Ed25519PublicKey
SecretKey = namedtuple("SecretKey", "key_id key")     # key: Ed25519PrivateKey
Signature = namedtuple("Signature", "untrusted_comment algo key_id sig trusted_comment global_sig")


class MinisignError(Exception):
    """Every way producing or accepting a signature can fail, and the only exception `verify` lets
    out. One type on purpose: a caller that catches "malformed" separately from "did not verify"
    eventually treats one of them as benign."""


# --- encoding ------------------------------------------------------------------------------------

def _b64d(text, n, what):
    try:
        raw = base64.b64decode(text, validate=True)
    except Exception as e:
        raise MinisignError(f"{what}: not valid base64 ({e})") from None
    if len(raw) != n:
        raise MinisignError(f"{what}: {len(raw)} bytes, expected {n}")
    return raw


def _b64e(raw):
    return base64.b64encode(raw).decode("ascii")


def _lines(text, n, what):
    """The exact `n` lines of a minisign file, or a MinisignError naming what is wrong.

    Strict about CRLF rather than forgiving: the trusted comment is signed as the bytes on the line,
    so a file a text-mode copy has turned into CRLF is not "nearly right", it is a different signed
    message. Saying so beats a bare "does not verify" that sends someone hunting the wrong bug."""
    body = text.split("\n")
    if body and body[-1] == "":     # the trailing LF of the last line
        body.pop()
    if len(body) != n:
        raise MinisignError(f"{what}: {len(body)} lines, expected exactly {n}")
    if any(ln.endswith("\r") for ln in body):
        raise MinisignError(f"{what}: CRLF line endings — the signed form is LF")
    return body


def _field(line, prefix, what):
    if not line.startswith(prefix):
        raise MinisignError(f"{what}: line does not start with {prefix!r}")
    return line[len(prefix):]


# --- keys ----------------------------------------------------------------------------------------

def generate_keypair(comment):
    """-> (public key file text, secret key file text).

    The secret is returned and never written: where it comes to rest (a CI secret, a password
    manager) is a decision this module must not quietly make for its caller by leaving a file."""
    key_id = os.urandom(KEY_ID_LEN)
    sk = Ed25519PrivateKey.generate()
    pub = _two_line(comment, ALGO + key_id + sk.public_key().public_bytes_raw())
    sec = _two_line(comment + " — SECRET KEY, keep it in CI secrets and never in a repo",
                    ALGO + key_id + sk.private_bytes_raw())
    return pub, sec


def _two_line(comment, payload):
    return f"{UNTRUSTED_PREFIX}{comment}\n{_b64e(payload)}\n"


def _key_line(text, what):
    """The base64 payload line of a key file — tolerantly, unlike a signature.

    NOTHING in a key file is signed. The comment is `untrusted` in the literal sense: it is never
    read, never covered by anything, and exists to label a file for a human. The key is the base64
    on the last line, and its own length, algorithm prefix and base64 validity are all checked
    below. So the CRLF and exact-line-count rules `_lines` enforces for signatures — where the
    trusted comment IS signed bytes and a line ending genuinely changes the message — protect
    nothing here, while costing a real failure: the release key lives in a CI secret box, arrives
    with whatever line endings a browser chose, and a key refused for CRLF fails a release AFTER
    the build, citing a format the operator never picked.

    So: CRLF or LF, trailing newline or not, and the comment line optional — pasting just the
    base64 is a complete key. More than those two lines is refused, because that is not a key file
    and guessing which line to read would be worse than saying so.
    """
    lines = [ln.rstrip("\r") for ln in text.split("\n")]
    lines = [ln for ln in lines if ln.strip()]
    if len(lines) not in (1, 2):
        raise MinisignError(f"{what}: {len(lines)} non-blank lines, expected the base64 payload "
                            f"line, optionally preceded by a comment line")
    return lines[-1]


def _key_payload(text, what):
    b64 = _key_line(text, what)
    raw = _b64d(b64, 2 + KEY_ID_LEN + KEY_LEN, what)
    if raw[:2] != ALGO:
        raise MinisignError(f"{what}: algorithm {raw[:2]!r}, expected {ALGO!r}")
    return raw[2:2 + KEY_ID_LEN], raw[2 + KEY_ID_LEN:]


def parse_public_key(text):
    key_id, raw = _key_payload(text, "public key")
    return PublicKey(key_id, Ed25519PublicKey.from_public_bytes(raw))


def parse_secret_key(text):
    key_id, raw = _key_payload(text, "secret key")
    return SecretKey(key_id, Ed25519PrivateKey.from_private_bytes(raw))


def public_key_text(secret_key, comment):
    """The .pub that belongs to a secret key — so a lost public key is recoverable, and so a
    key file and its pub can be checked against each other rather than assumed to be a pair."""
    sk = _as_secret_key(secret_key)
    return _two_line(comment, ALGO + sk.key_id + sk.key.public_key().public_bytes_raw())


def _as_public_key(k):
    return k if isinstance(k, PublicKey) else parse_public_key(_as_text(k))


def _as_secret_key(k):
    return k if isinstance(k, SecretKey) else parse_secret_key(_as_text(k))


def _as_text(k):
    return k.decode("utf-8") if isinstance(k, (bytes, bytearray)) else k


# --- signatures ----------------------------------------------------------------------------------

def parse_signature(text):
    """-> Signature, structure only: line shape, prefixes, base64 and field lengths. Applies NO
    policy — `verify` owns which algorithms and keys are acceptable, and the fixture generator needs
    to be able to reassemble a signature this parser accepted into one it must not."""
    untrusted, sig_b64, trusted, gsig_b64 = _lines(text, 4, "signature")
    raw = _b64d(sig_b64, 2 + KEY_ID_LEN + SIG_LEN, "signature line")
    return Signature(
        _field(untrusted, UNTRUSTED_PREFIX, "signature"),
        raw[:2], raw[2:2 + KEY_ID_LEN], raw[2 + KEY_ID_LEN:],
        _field(trusted, TRUSTED_PREFIX, "signature"),
        _b64d(gsig_b64, SIG_LEN, "global signature line"),
    )


def format_signature(s):
    return (f"{UNTRUSTED_PREFIX}{s.untrusted_comment}\n"
            f"{_b64e(s.algo + s.key_id + s.sig)}\n"
            f"{TRUSTED_PREFIX}{s.trusted_comment}\n"
            f"{_b64e(s.global_sig)}\n")


def sign(data, secret_key, untrusted_comment=DEFAULT_UNTRUSTED, trusted_comment=None):
    """-> the .minisig text for `data`. `trusted_comment` defaults to a timestamp, which is all
    upstream puts there; it is signed but ADVISORY — a reader must take payload identity, ordering
    and everything else from the signed document, never by parsing this line."""
    sk = _as_secret_key(secret_key)
    if trusted_comment is None:
        trusted_comment = f"timestamp:{int(time.time())}"
    for what, comment in (("untrusted", untrusted_comment), ("trusted", trusted_comment)):
        if "\n" in comment or "\r" in comment:
            raise MinisignError(f"the {what} comment spans lines; a .minisig is exactly four")
    sig = sk.key.sign(data)
    gsig = sk.key.sign(sig + trusted_comment.encode("utf-8"))
    return format_signature(
        Signature(untrusted_comment, ALGO, sk.key_id, sig, trusted_comment, gsig))


def verify(data, minisig_text, public_keys):
    """-> the 8-byte key_id that signed `data`, or MinisignError. Nothing is returned on a partial
    result: both signatures are checked, and the algorithm is checked before either.

    `public_keys` is the trust root — file texts or PublicKeys. The key_id selects among them; it is
    a hint, not a credential, so an attacker gains nothing by naming a key that did not sign."""
    s = parse_signature(minisig_text)
    if s.algo == ALGO_HASHED:
        raise MinisignError("algorithm 'ED' (Blake2b-prehashed) — this contract signs 'Ed' only")
    if s.algo != ALGO:
        raise MinisignError(f"algorithm {s.algo!r}, expected {ALGO!r}")

    ring = [_as_public_key(k) for k in public_keys]
    if not ring:
        raise MinisignError("no public keys given; a signature with no trust root proves nothing")
    candidates = [k for k in ring if k.key_id == s.key_id]
    if not candidates:
        raise MinisignError(f"signed by key {s.key_id.hex()}, which is not in the trust root "
                            f"({', '.join(k.key_id.hex() for k in ring)})")

    for k in candidates:
        try:
            k.key.verify(s.sig, data)
        except InvalidSignature:
            continue
        # The primary signature verified, so this IS the signing key: a bad global signature here is
        # a rewritten trusted comment, not a wrong key, and must not be retried against another.
        try:
            k.key.verify(s.global_sig, s.sig + s.trusted_comment.encode("utf-8"))
        except InvalidSignature:
            raise MinisignError("the trusted comment and its global signature disagree — one of "
                                "the two was replaced after signing") from None
        return k.key_id
    raise MinisignError(f"the signature by key {s.key_id.hex()} does not match the signed bytes "
                        f"({len(data)} of them)")


# --- selftest -------------------------------------------------------------------------------------

def _selftest():
    """The properties that make a signature worth checking at all. Keys are minted here and thrown
    away — a fixed test key in the repo is a private key in the repo."""
    results = []

    def ok(name, fn):
        try:
            fn()
        except Exception as e:                      # noqa: BLE001 — any escape is the failure
            results.append((False, name, f"{type(e).__name__}: {e}"))
        else:
            results.append((True, name, ""))

    def refused(name, fn):
        try:
            fn()
        except MinisignError as e:
            results.append((True, name, str(e)))
        except Exception as e:                      # noqa: BLE001
            results.append((False, name, f"raised {type(e).__name__}, not MinisignError: {e}"))
        else:
            results.append((False, name, "ACCEPTED — the check does not exist"))

    pub_text, sec_text = generate_keypair("phoenix selftest")
    other_pub, other_sec = generate_keypair("phoenix selftest, an unrelated key")
    key_id = parse_public_key(pub_text).key_id
    data = b'{"payload_id":"mod","serial":7,"files":[]}\n'
    sig = sign(data, sec_text, "selftest", "serial 7")
    other_sig = sign(data, other_sec, "selftest", "serial 7")

    def assert_(cond, why):
        if not cond:
            raise AssertionError(why)

    ok("keygen -> sign -> verify, and verify names the signing key",
       lambda: assert_(verify(data, sig, [pub_text]) == key_id, "wrong key_id returned"))
    ok("a key ring: the key_id selects, order does not",
       lambda: assert_(verify(data, sig, [other_pub, pub_text]) == key_id, "not selected by id"))
    ok("the public key file round-trips",
       lambda: assert_(parse_public_key(public_key_text(sec_text, "x")).key_id == key_id,
                       "pub derived from the secret disagrees"))

    # A key file is not signed content, so it is read tolerantly — see _key_line. These three are
    # exactly what a secret pasted into a CI secret box can arrive as, and each of them failing
    # would surface as a broken release rather than as a bad paste.
    ok("a key file with CRLF line endings",
       lambda: assert_(parse_secret_key(sec_text.replace("\n", "\r\n")).key_id == key_id,
                       "CRLF key rejected"))
    ok("a key file with no comment line — just the base64",
       lambda: assert_(parse_secret_key(sec_text.split("\n")[1]).key_id == key_id,
                       "bare payload line rejected"))
    ok("a key file with no trailing newline",
       lambda: assert_(parse_secret_key(sec_text.rstrip("\n")).key_id == key_id,
                       "missing trailing newline rejected"))
    refused("a key file with a smuggled extra line",
            lambda: parse_secret_key(sec_text + "AAAA\n"))
    ok("a signature verifies under the key that made it",
       lambda: verify(data, other_sig, [other_pub]))

    refused("one flipped byte of the signed data",
            lambda: verify(data.replace(b'"serial":7', b'"serial":8'), sig, [pub_text]))
    refused("a byte APPENDED to the signed data",
            lambda: verify(data + b" ", sig, [pub_text]))
    refused("a real signature by a key the ring does not hold",
            lambda: verify(data, other_sig, [pub_text]))
    refused("an empty trust root",
            lambda: verify(data, sig, []))

    refused("a .minisig truncated to three lines",
            lambda: verify(data, "\n".join(sig.split("\n")[:3]) + "\n", [pub_text]))
    refused("a .minisig cut in half mid-base64",
            lambda: verify(data, sig[:len(sig) // 2], [pub_text]))
    refused("an empty .minisig", lambda: verify(data, "", [pub_text]))
    lines = sig.split("\n")
    refused("a .minisig whose signature line is not base64",
            lambda: verify(data, "\n".join([lines[0], "@@ not base64 @@", lines[2], lines[3], ""]),
                           [pub_text]))
    refused("a .minisig with a mislabelled comment line",
            lambda: verify(data, sig.replace(TRUSTED_PREFIX, "trusted: "), [pub_text]))
    refused("a .minisig converted to CRLF",
            lambda: verify(data, sig.replace("\n", "\r\n"), [pub_text]))

    # The prehashed variant is the one substitution that survives every OTHER check: the signature
    # below is a genuine, verifying pure-Ed25519 signature by the real key, relabelled. A verifier
    # that reads the algorithm and ignores it accepts it, and has silently gained a second
    # algorithm it never implemented.
    parsed = parse_signature(sig)
    refused("algorithm 'ED' over an otherwise valid signature",
            lambda: verify(data, format_signature(parsed._replace(algo=ALGO_HASHED)), [pub_text]))
    refused("an unknown algorithm",
            lambda: verify(data, format_signature(parsed._replace(algo=b"xx")), [pub_text]))

    # Both of these leave the PRIMARY signature valid. They fail only if the global signature is
    # actually checked, which is the whole reason the trusted comment is worth reading.
    refused("a rewritten trusted comment under a valid primary signature",
            lambda: verify(data, format_signature(parsed._replace(trusted_comment="serial 9999")),
                           [pub_text]))
    refused("a replaced global signature",
            lambda: verify(data, format_signature(
                parsed._replace(global_sig=parse_signature(other_sig).global_sig)), [pub_text]))

    refused("a comment that would smuggle in a fifth line",
            lambda: sign(data, sec_text, "selftest", "serial 7\ntrusted comment: serial 8"))

    for good, name, detail in results:
        print(f"  {'ok  ' if good else 'FAIL'} {name}" + (f"\n         {detail}" if detail else ""))
    bad = sum(not good for good, _, _ in results)
    print(f"selftest: {len(results)} checks, all pass" if not bad
          else f"selftest: {bad} of {len(results)} checks FAILED")
    return bad


# --- CLI -------------------------------------------------------------------------------------------

def _read_text(path):
    with open(path, encoding="utf-8", newline="") as f:   # newline="": CRLF must not be hidden
        return f.read()


def _write_new(path, text, what):
    if os.path.exists(path):
        sys.exit(f"error: {path} exists; refusing to overwrite a {what}")
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("keygen", help="mint a keypair; the secret is written, never printed")
    g.add_argument("--sec", required=True, help="secret key file to create (NEVER commit it)")
    g.add_argument("--pub", required=True, help="public key file to create")
    g.add_argument("-c", "--comment", default="phoenix release key")

    s = sub.add_parser("sign", help="sign a file's exact bytes")
    s.add_argument("file")
    s.add_argument("--sec", required=True)
    s.add_argument("--out", help="default: <file>.minisig")
    s.add_argument("-t", "--trusted-comment", help="signed; default: timestamp:<now>")
    s.add_argument("-c", "--untrusted-comment", default=DEFAULT_UNTRUSTED)

    v = sub.add_parser("verify", help="verify a file against a trust root")
    v.add_argument("file")
    v.add_argument("--pub", required=True, action="append", metavar="PATH",
                   help="a trusted public key; repeat for a key ring")
    v.add_argument("--sig", help="default: <file>.minisig")

    sub.add_parser("selftest", help="check the signing and verification rules against each other")
    a = ap.parse_args()

    if a.cmd == "selftest":
        sys.exit(1 if _selftest() else 0)

    if a.cmd == "keygen":
        # BOTH destinations are checked before EITHER is written. Writing the secret first and then
        # refusing on the public path left an orphaned secret key on disk under a message that says
        # nothing was written — the worst failure mode available to the one tool whose whole job is
        # handling a secret carefully.
        for path, what in ((a.sec, "secret key"), (a.pub, "public key")):
            if os.path.exists(path):
                sys.exit(f"error: {path} exists; refusing to overwrite a {what}")
        pub, sec = generate_keypair(a.comment)
        _write_new(a.sec, sec, "secret key")
        _write_new(a.pub, pub, "public key")
        print(f"key {parse_public_key(pub).key_id.hex()}\n"
              f"  secret {a.sec}  — move it into a CI secret and delete the file\n"
              f"  public {a.pub}  — commit this one")
        return

    with open(a.file, "rb") as f:
        data = f.read()

    if a.cmd == "sign":
        out = a.out or a.file + ".minisig"
        text = sign(data, _read_text(a.sec), a.untrusted_comment, a.trusted_comment)
        with open(out, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        print(f"signed {a.file} ({len(data)} bytes) -> {out}")
        return

    minisig = _read_text(a.sig or a.file + ".minisig")
    try:
        key_id = verify(data, minisig, [_read_text(p) for p in a.pub])
    except MinisignError as e:
        sys.exit(f"REFUSED {a.file}\n  {e}")
    print(f"ok {a.file} — signed by key {key_id.hex()}\n"
          f"  trusted comment: {parse_signature(minisig).trusted_comment}")


if __name__ == "__main__":
    main()
