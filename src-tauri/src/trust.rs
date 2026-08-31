//! What makes a downloaded document believable.
//!
//! Every sha256 this launcher acts on comes out of a manifest, and the manifest itself used to be
//! covered by nothing but TLS — which authenticates a HOST, not a DOCUMENT. That is the wrong end
//! of the chain to rest on once releases can be served from third-party mirrors: the assets are
//! already verified against the manifest (install.rs), so signing the manifest is what closes it.
//! Everything downstream of here can go on being a hash comparison; this module is what makes the
//! hashes worth comparing against.
//!
//! ## The signature format — a shared contract, not an implementation detail
//!
//! **minisign**, `Ed` (pure Ed25519) variant. A `<name>.minisig` is exactly four LF-terminated
//! lines:
//!
//! ```text
//! untrusted comment: <arbitrary text>
//! <base64 of: 2-byte algo || 8-byte key_id || 64-byte signature>
//! trusted comment: <arbitrary text>
//! <base64 of: 64-byte global signature>
//! ```
//!
//! * `algo` is ASCII `"Ed"` — the signature covers the FILE BYTES. `"ED"`, minisign's
//!   Blake2b-prehashed variant, is refused rather than supported: it is a second thing to get
//!   right for no gain here (documents are small enough to sign whole), and silently accepting a
//!   scheme we do not implement is how a verifier ends up verifying nothing.
//! * `signature` is `Ed25519(secret, <the document's exact bytes>)`.
//! * `global signature` is `Ed25519(secret, <the 64 signature bytes> || <trusted comment>)`, where
//!   the trusted comment is the line's text AFTER `trusted comment: `. It binds the one part of
//!   the file that is both attacker-visible and otherwise unauthenticated; skipping it is a small
//!   hole, but it is a hole in the only field a reader might one day be tempted to believe.
//!
//! Base64 is RFC 4648 with padding and no wrapping, decoded strictly (see `b64`).
//!
//! ## Identity and freshness
//!
//! A valid signature says "we produced this". It does not say WHICH document this is, and both
//! remaining questions have to be asked separately:
//!
//! * `payload_id` — a legitimately signed launcher manifest fed to the mod update path is signed
//!   by our key and describes the wrong thing entirely. The caller states what it asked for.
//! * `serial` — the SOLE ordering authority (`version` is a display string). A mirror can always
//!   serve an older release it once had a valid signature for, so a document is refused below the
//!   highest serial this machine has accepted, floored by a value baked at build time.

use crate::manifest::Manifest;

/// Bytes a signed document may occupy in memory. It has to be buffered whole to be verified, so
/// its size is a trust input like any other: without a ceiling a hostile host answers
/// `manifest.json` with an endless stream and the verifier never gets a chance to refuse it.
/// 16 MiB is an order of magnitude past the largest document we produce — the base game's
/// manifest, ~4.6k entries of roughly 200 bytes each.
pub const MAX_DOC_BYTES: u64 = 16 * 1024 * 1024;

/// A `.minisig` is ~300 bytes; the slack is for comments.
pub const MAX_SIG_BYTES: u64 = 8 * 1024;

/// The signature asset's name is the document's plus this.
pub const SIG_SUFFIX: &str = ".minisig";

/// minisign's 8-byte key identifier. Names WHICH key signed, and nothing else — it is not a
/// secret, not a checksum, and carries no authority of its own.
pub type KeyId = [u8; 8];

/// A key this build is willing to believe. Compiled into the binary: there is no key distribution
/// problem to solve here, because a key fetched from the payload's own channel is not a trust root
/// — it is one more thing whoever serves the payload gets to choose. Only a key that arrived WITH
/// the binary can say anything the binary did not already have to take on faith.
pub struct TrustedKey {
    pub id: KeyId,
    /// Raw 32-byte Ed25519 public key.
    pub key: [u8; 32],
}

/// The keys a document may be signed by, in no particular order — a signature names its own key.
///
/// TWO of them, and the second is not redundant: RECOVERY is a cold spare that never signs
/// anything while ACTIVE is intact. Its whole purpose is the day the active key is lost or
/// compromised, when the alternative is that every installed launcher becomes permanently
/// unable to accept a release — an unrecoverable state, since the fix would have to ship
/// through the very channel that broke.
///
/// **Generated, not transcribed.** `build.rs` decodes the published `.pub` files of
/// `Pr0j3ctPh03nix/release-tooling` — pinned by commit SHA in both workflows and checked out to
/// `.tooling/` — and emits this table; a build that cannot read them is refused outright, because
/// an empty trust root is a launcher that can never install anything. The hand-written literal
/// that used to sit here could only agree with those files or be silently wrong, and being wrong
/// has no runtime signal whatsoever: the failure looks exactly like "no release is available" by
/// design (see `TrustError` below).
pub const PINNED: &[TrustedKey] = &include!(concat!(env!("OUT_DIR"), "/pinned_keys.rs"));

/// Why a document is not believable. Typed (like `NetKind` and `UnsupportedSchema`) so the command
/// layer can classify without matching on message text — see `views::wire_kind`, which maps every
/// one of these to `notFound`: an unverifiable release is a release we do not have, and that
/// answer must never stop somebody playing a game that is already installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// The release publishes the document but no signature for it.
    Unsigned(String),
    /// The `.minisig` is not in the format at all — wrong line count, bad base64, wrong length.
    Malformed(&'static str),
    /// Well-formed, but names a signature algorithm this build does not implement.
    Algorithm([u8; 2]),
    /// Well-formed and complete, but signed by a key that is not ours. Deliberately DISTINCT from
    /// a bad signature: "somebody else signed this" and "these bytes were tampered with" are
    /// different events, and only the first one has an innocent explanation (a key rotation this
    /// build predates).
    UnknownKey(KeyId),
    /// The signature does not cover these bytes.
    BadSignature,
    /// The document verified, but the trusted comment is not the text that was signed with it.
    BadComment,
    /// The document does not identify itself as the payload that was asked for. `found: None`
    /// means it names no payload at all, which is the same refusal: our producers always state
    /// one, so a signed document without it is not a document of ours.
    WrongPayload { expected: &'static str, found: Option<String> },
    /// The document is not newer than the newest one this machine has already accepted.
    /// `found: None` means it carries no serial, which cannot be shown to be current either.
    StaleSerial { payload: &'static str, found: Option<u64>, floor: u64 },
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsigned(name) => write!(f, "the release publishes no signature for {name}"),
            Self::Malformed(why) => write!(f, "the signature file is malformed: {why}"),
            Self::Algorithm(a) => write!(
                f,
                "the signature uses algorithm {:?}, and this launcher only accepts \"Ed\" \
                 (pure Ed25519 over the file bytes)",
                String::from_utf8_lossy(a)
            ),
            Self::UnknownKey(id) => {
                write!(f, "the release is signed by an unknown key ({})", hex::encode(id))
            }
            Self::BadSignature => write!(f, "the signature does not match the file"),
            Self::BadComment => write!(f, "the signature's trusted comment has been tampered with"),
            Self::WrongPayload { expected, found } => match found {
                Some(got) => write!(f, "this is a {got:?} manifest, but {expected:?} was asked for"),
                None => write!(f, "the manifest does not say which payload it is ({expected:?} was asked for)"),
            },
            Self::StaleSerial { payload, found, floor } => match found {
                Some(n) => write!(
                    f,
                    "this {payload} release is serial {n}, older than {floor}, which this machine \
                     has already accepted"
                ),
                None => write!(f, "this {payload} manifest carries no serial, so it cannot be shown to be current"),
            },
        }
    }
}

impl std::error::Error for TrustError {}

/// Which signed payload a document is expected to describe.
///
/// The format also defines `"mirrors"` for the published mirror list. There is deliberately no
/// variant for it: nothing here reads one yet (that is the mirror phase's job), and an enum arm
/// no code can produce is a claim this module does not back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// The dist repo — the shim and its game files.
    Mod,
    /// The launcher's own releases (self-update).
    Launcher,
    /// The base game.
    Game,
}

impl Payload {
    /// The string a manifest's `payload_id` must carry to be this payload.
    pub fn id(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::Launcher => "launcher",
            Self::Game => "game",
        }
    }

}

// There is deliberately NO build-time serial floor. One existed (PHOENIX_MIN_SERIAL_*, baked per
// payload) as a backstop under the persisted ratchet, on the reasoning that settings.json is
// plaintext and anything able to edit it could hand the ratchet back. That reasoning does not
// survive contact: anything with write access to the user's profile can replace the launcher
// itself, which is strictly more powerful than rolling its ratchet back.
//
// The only window it genuinely covered was a FIRST install on a fresh machine, where nothing is
// persisted yet, behind a source that chooses to serve an old release. The worst outcome there is
// a genuine, signed, previously-published release — stale, not hostile — and the next check from
// any honest source corrects it. That is not worth a per-build variable that has to be set
// correctly forever and is silently useless when it is not.
//
// The launcher's own payload needs no floor at all: it knows its version from CARGO_PKG_VERSION
// and compares against the SIGNED manifest (see selfupdate::available), so a downgrade cannot be
// offered no matter what a source claims.

/// Verify `minisig` over `data`, returning the id of the key that signed it.
///
/// The order is the contract, and every step is a different refusal: parse, decode, algorithm,
/// KEY (distinct from a bad signature — see `TrustError::UnknownKey`), the signature over the
/// document, then the global signature over `signature || trusted comment`.
///
/// Nothing here can panic on hostile input: every length is checked before it is sliced, and the
/// base64 decoder rejects rather than guesses. A verifier that crashes on a malformed signature
/// file has handed the attacker a better outcome than the one they were reaching for.
pub fn verify(data: &[u8], minisig: &str) -> Result<KeyId, TrustError> {
    let sig = parse(minisig)?;
    if sig.algo != *b"Ed" {
        return Err(TrustError::Algorithm(sig.algo));
    }
    let key = pinned()
        .find(|k| k.id == sig.key_id)
        .ok_or(TrustError::UnknownKey(sig.key_id))?;
    let ed = |msg: &[u8], signature: &[u8]| {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key.key)
            .verify(msg, signature)
    };
    ed(data, &sig.sig).map_err(|_| TrustError::BadSignature)?;
    // binds the trusted comment to the signature it travels with, so the one attacker-visible
    // string in the file cannot be rewritten under a signature that still verifies
    let mut global = Vec::with_capacity(64 + sig.trusted.len());
    global.extend_from_slice(&sig.sig);
    global.extend_from_slice(sig.trusted.as_bytes());
    ed(&global, &sig.global).map_err(|_| TrustError::BadComment)?;
    Ok(sig.key_id)
}

/// The document names the payload the caller asked for and is not older than `floor`. Returns the
/// accepted serial, which is what the caller ratchets `floor` forward with.
///
/// Separate from `verify` because it asks a different question of a different input: `verify`
/// establishes that we produced the bytes, this establishes that they are the bytes we wanted.
/// Both have to pass, and neither implies the other.
pub fn accept(payload: Payload, manifest: &Manifest, floor: u64) -> Result<u64, TrustError> {
    if manifest.payload_id.as_deref() != Some(payload.id()) {
        return Err(TrustError::WrongPayload {
            expected: payload.id(),
            found: manifest.payload_id.clone(),
        });
    }
    // `>=`, not `>`: the same release re-fetched (every check does exactly that) is the common
    // case, and refusing it would make the second check of any release fail.
    match manifest.serial {
        Some(n) if n >= floor => Ok(n),
        found => Err(TrustError::StaleSerial { payload: payload.id(), found, floor }),
    }
}

/// A parsed `.minisig`. Borrows the trusted comment: it is signed text, and copying it would only
/// add a place for the copy to differ from what was verified.
struct Minisig<'a> {
    algo: [u8; 2],
    key_id: KeyId,
    sig: [u8; 64],
    trusted: &'a str,
    global: [u8; 64],
}

/// A producer on Windows writes CRLF without meaning to. The bytes the global signature covers are
/// the spec's (LF), so dropping a trailing CR can only let a CONFORMANT file verify — it can never
/// make a non-conformant one pass, because the comment it recovers is the same string either way.
fn cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

fn parse(text: &str) -> Result<Minisig<'_>, TrustError> {
    use TrustError::Malformed;
    let mut lines: Vec<&str> = text.split('\n').collect();
    // the file's own terminating LF, not a fifth line
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let [untrusted, sig_line, trusted, global_line] = lines[..] else {
        return Err(Malformed("expected exactly four lines"));
    };
    if !cr(untrusted).starts_with("untrusted comment:") {
        return Err(Malformed("the first line is not an untrusted comment"));
    }
    // The prefix INCLUDES the space, and the space is not optional. Upstream's
    // TRUSTED_COMMENT_PREFIX is "trusted comment: " and our producer refuses to parse a line
    // without it, so accepting both spellings meant one signature could be written two ways that
    // verify identically under one global signature — the exact malleability `b64` below is
    // intolerant to avoid, and a place where this reader silently accepted what the producer
    // would reject.
    let trusted = cr(trusted)
        .strip_prefix("trusted comment: ")
        .ok_or(Malformed("the third line is not a trusted comment"))?;

    let head = b64(cr(sig_line)).ok_or(Malformed("the signature line is not base64"))?;
    let head: [u8; 74] = head.try_into().map_err(|_| Malformed("the signature line is not 74 bytes"))?;
    let global = b64(cr(global_line)).ok_or(Malformed("the global signature line is not base64"))?;
    let global: [u8; 64] = global
        .try_into()
        .map_err(|_| Malformed("the global signature is not 64 bytes"))?;

    Ok(Minisig {
        algo: [head[0], head[1]],
        key_id: head[2..10].try_into().expect("10 - 2 == 8"),
        sig: head[10..74].try_into().expect("74 - 10 == 64"),
        trusted,
        global,
    })
}

/// Standard base64 (RFC 4648), decoded STRICTLY: canonical padding, no whitespace, no wrapping,
/// no alternate alphabet, and the bits the final character contributes beyond the last byte must
/// be zero.
///
/// Hand-written rather than declared as a dependency, and that is a decision rather than an
/// oversight. `base64` is already in the lock TWICE (0.21 and 0.22, pulled in by unrelated
/// crates), so declaring one of them re-creates exactly the coupling that declaring `ring`
/// removes: a resolver picking our plumbing for us. And what a signature file wants is not a
/// general decoder but an intolerant one — every lenient feature (skipped whitespace, optional
/// padding, ignored trailing bits) lets one signature file be spelled several ways, which is
/// malleability in the one place we are trying to remove it. Twenty lines that reject everything
/// they do not recognise are easier to be sure of than a general decoder configured to.
fn b64(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() % 4 != 0 {
        return None;
    }
    let pad = b.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in &b[..b.len() - pad] {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None, // including a '=' anywhere but the very end
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// Every key a signature may name. In a test build the suite's own key is appended, so the tests
/// exercise the REAL verifier against real signatures instead of a stand-in — the production table
/// stays untouched, and no private key exists outside `#[cfg(test)]` code.
fn pinned() -> impl Iterator<Item = &'static TrustedKey> {
    PINNED.iter().chain(test_keys())
}

#[cfg(not(test))]
fn test_keys() -> std::slice::Iter<'static, TrustedKey> {
    const NONE: &[TrustedKey] = &[];
    NONE.iter()
}

#[cfg(test)]
fn test_keys() -> std::slice::Iter<'static, TrustedKey> {
    std::slice::from_ref(testing::key()).iter()
}

/// Signing, for tests only. Keys are generated here at RUNTIME from a fixed seed — a private key
/// never enters the repository, and the verifier under test is the shipped one.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::sync::OnceLock;

    /// The suite's signing seed. Not a secret and not a production key: it exists only inside
    /// `#[cfg(test)]`, which is compiled for `cargo test` and for nothing else.
    pub(crate) const TEST_SEED: [u8; 32] = [7u8; 32];
    pub(crate) const TEST_KEY_ID: KeyId = *b"TESTKEY0";

    pub(crate) fn pair(seed: &[u8; 32]) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(seed).expect("a 32-byte seed is a valid Ed25519 seed")
    }

    /// The pinned test key, derived once.
    pub(crate) fn key() -> &'static TrustedKey {
        static KEY: OnceLock<TrustedKey> = OnceLock::new();
        KEY.get_or_init(|| TrustedKey {
            id: TEST_KEY_ID,
            key: pair(&TEST_SEED).public_key().as_ref().try_into().expect("Ed25519 keys are 32 bytes"),
        })
    }

    /// A complete `.minisig` over `data`. Every knob the format has is a parameter, so a test can
    /// produce the wrong algorithm or the wrong key without hand-assembling base64.
    pub(crate) fn sign(
        seed: &[u8; 32],
        key_id: KeyId,
        algo: [u8; 2],
        data: &[u8],
        trusted: &str,
    ) -> String {
        let kp = pair(seed);
        let sig = kp.sign(data);
        let mut head = Vec::with_capacity(74);
        head.extend_from_slice(&algo);
        head.extend_from_slice(&key_id);
        head.extend_from_slice(sig.as_ref());
        let mut global = Vec::new();
        global.extend_from_slice(sig.as_ref());
        global.extend_from_slice(trusted.as_bytes());
        format!(
            "untrusted comment: signature from the test suite\n{}\ntrusted comment: {trusted}\n{}\n",
            b64_encode(&head),
            b64_encode(kp.sign(&global).as_ref())
        )
    }

    /// The ordinary case: signed by the pinned test key, in the supported algorithm.
    pub(crate) fn test_sig(data: &[u8]) -> String {
        sign(&TEST_SEED, TEST_KEY_ID, *b"Ed", data, "test signature")
    }

    /// Encode side of `b64`. Test-only: the launcher never produces base64, it only reads it.
    pub(crate) fn b64_encode(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const DOC: &[u8] = br#"{"payload_id":"mod","serial":7,"version":"1.0.0","files":[]}"#;

    fn manifest(json: &str) -> Manifest {
        Manifest::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn a_good_signature_verifies_and_names_its_key() {
        assert_eq!(verify(DOC, &test_sig(DOC)).unwrap(), TEST_KEY_ID);
    }

    /// The whole point: the signature is over the document's bytes, so one changed byte anywhere
    /// in it must fail — including in a place a careless verifier might not cover (the last one).
    #[test]
    fn one_flipped_byte_fails() {
        let sig = test_sig(DOC);
        for i in [0, DOC.len() / 2, DOC.len() - 1] {
            let mut doc = DOC.to_vec();
            doc[i] ^= 0x01;
            assert_eq!(verify(&doc, &sig), Err(TrustError::BadSignature), "byte {i}");
        }
        // and a truncated document is not a shorter valid one
        assert_eq!(verify(&DOC[..DOC.len() - 1], &sig), Err(TrustError::BadSignature));
    }

    /// A perfectly-formed signature from a key we do not pin is its OWN answer. Collapsing it into
    /// "bad signature" would describe an attack where the honest explanation is usually a key
    /// rotation this build predates — and the two want different responses.
    #[test]
    fn an_unpinned_key_is_reported_as_unknown_not_as_a_bad_signature() {
        let sig = sign(&[9u8; 32], *b"OTHERKEY", *b"Ed", DOC, "signed by somebody else");
        assert_eq!(verify(DOC, &sig), Err(TrustError::UnknownKey(*b"OTHERKEY")));
        // the same key id with the right SEED is still unknown — the id is a label, not authority
        let sig = sign(&TEST_SEED, *b"OTHERKEY", *b"Ed", DOC, "right key, wrong id");
        assert_eq!(verify(DOC, &sig), Err(TrustError::UnknownKey(*b"OTHERKEY")));
    }

    /// `ED` is minisign's Blake2b-prehashed variant. We do not implement it, so it must be refused
    /// by NAME — reading it as `Ed` would verify a hash-of-a-hash and call the file signed.
    #[test]
    fn the_prehashed_algorithm_is_refused() {
        let sig = sign(&TEST_SEED, TEST_KEY_ID, *b"ED", DOC, "prehashed");
        assert_eq!(verify(DOC, &sig), Err(TrustError::Algorithm(*b"ED")));
        // ...and so is anything else in that field
        let sig = sign(&TEST_SEED, TEST_KEY_ID, *b"xx", DOC, "nonsense");
        assert_eq!(verify(DOC, &sig), Err(TrustError::Algorithm(*b"xx")));
    }

    /// The global signature is what makes the trusted comment trusted. Rewriting it while keeping
    /// everything else must fail — otherwise the field is a lie in a file that claims to be signed.
    #[test]
    fn a_rewritten_trusted_comment_fails_the_global_signature() {
        let sig = test_sig(DOC);
        let tampered = sig.replace("trusted comment: test signature", "trusted comment: something else");
        assert_ne!(tampered, sig);
        assert_eq!(verify(DOC, &tampered), Err(TrustError::BadComment));
        // an empty comment is legal, but only if it is the one that was signed
        let empty = sign(&TEST_SEED, TEST_KEY_ID, *b"Ed", DOC, "");
        assert!(verify(DOC, &empty).is_ok());
        assert_eq!(
            verify(DOC, &empty.replace("trusted comment: \n", "trusted comment: x\n")),
            Err(TrustError::BadComment)
        );
    }

    /// Malformed input must be REFUSED — never panic, never pass. Each case is a different way a
    /// parser that assumed its input could go wrong.
    #[test]
    fn malformed_signature_files_are_refused_cleanly() {
        let good = test_sig(DOC);
        let lines: Vec<&str> = good.trim_end().split('\n').collect();
        let bad = [
            String::new(),
            "\n".to_string(),
            lines[..3].join("\n"),                            // three lines
            format!("{good}extra line\n"),                    // five
            lines.join("\r\n"),                               // CRLF is tolerated — see below
            good.replace("untrusted comment:", "hello:"),     // wrong first line
            good.replace("trusted comment:", "comment:"),     // wrong third line
            format!("{}\n!!!!{}\n{}\n{}\n", lines[0], &lines[1][4..], lines[2], lines[3]), // not base64
            format!("{}\n{}\n{}\n{}\n", lines[0], &lines[1][..96], lines[2], lines[3]), // short sig
            format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], &lines[3][..84]), // short global
            format!("{}\n{}\n{}\n\n", lines[0], lines[1], lines[2]),                    // empty global
        ];
        for (i, text) in bad.iter().enumerate() {
            match verify(DOC, text) {
                // CRLF is the one entry here that is deliberately NOT a failure: a producer on
                // Windows writes it by accident, and the bytes the signature covers are the
                // spec's either way.
                Ok(_) => assert_eq!(i, 4, "case {i} must not verify: {text:?}"),
                Err(TrustError::Malformed(_)) => {}
                Err(e) => panic!("case {i} failed as {e} — expected a malformed-file refusal"),
            }
        }
    }

    /// Base64 with anything clever in it is not base64 here. Each of these decodes "fine" under a
    /// lenient decoder and would let one signature be spelled several ways.
    #[test]
    fn the_base64_decoder_is_strict() {
        assert_eq!(b64("TWFu").unwrap(), b"Man");
        assert_eq!(b64("TWE=").unwrap(), b"Ma");
        assert_eq!(b64("TQ==").unwrap(), b"M");
        assert_eq!(b64(""), None, "empty");
        assert_eq!(b64("TWFu\n"), None, "trailing newline");
        assert_eq!(b64("TW Fu"), None, "embedded space");
        assert_eq!(b64("TWFuTWE"), None, "missing padding");
        assert_eq!(b64("TW=u"), None, "padding in the middle");
        assert_eq!(b64("T==="), None, "over-padded");
        assert_eq!(b64("TW-u"), None, "url-safe alphabet");
        // non-canonical: "TWE=" and "TWF=" differ only in bits the output cannot carry, so a
        // lenient decoder maps two different files onto the same bytes
        assert_eq!(b64("TWF="), None, "non-zero trailing bits");
    }

    #[test]
    fn a_payload_must_say_which_payload_it_is() {
        let m = manifest(r#"{"payload_id":"mod","serial":7,"version":"1","files":[]}"#);
        assert_eq!(accept(Payload::Mod, &m, 0).unwrap(), 7);
        assert_eq!(
            accept(Payload::Game, &m, 0),
            Err(TrustError::WrongPayload { expected: "game", found: Some("mod".into()) })
        );
        // a signed document of ours always names one; silence is refused, not assumed
        let anon = manifest(r#"{"serial":7,"version":"1","files":[]}"#);
        assert_eq!(
            accept(Payload::Mod, &anon, 0),
            Err(TrustError::WrongPayload { expected: "mod", found: None })
        );
    }

    #[test]
    fn the_serial_floor_refuses_a_rollback_and_admits_a_re_fetch() {
        let m = manifest(r#"{"payload_id":"mod","serial":7,"version":"1","files":[]}"#);
        assert_eq!(accept(Payload::Mod, &m, 6).unwrap(), 7);
        assert_eq!(accept(Payload::Mod, &m, 7).unwrap(), 7, "the same release, checked again");
        assert_eq!(
            accept(Payload::Mod, &m, 8),
            Err(TrustError::StaleSerial { payload: "mod", found: Some(7), floor: 8 })
        );
        // no serial at all: nothing to order it by, so it cannot be shown to be current
        let m = manifest(r#"{"payload_id":"mod","version":"1","files":[]}"#);
        assert_eq!(
            accept(Payload::Mod, &m, 0),
            Err(TrustError::StaleSerial { payload: "mod", found: None, floor: 0 })
        );
    }

    /// The generated table is the two published release keys, and nothing else.
    ///
    /// `PINNED` is emitted by `build.rs` from whatever `.pub` files it was pointed at, so the thing
    /// worth asserting is no longer "these bytes were typed correctly" — it is that the generation
    /// ran against the RIGHT keys and sliced them correctly. A build against a stale checkout, an
    /// unrelated `keys/` directory, or a parser off by a byte lands here instead of in the field,
    /// where the only symptom would be every release quietly becoming uninstallable.
    #[test]
    fn the_pinned_table_is_exactly_the_two_published_release_keys() {
        let ids: Vec<String> = PINNED.iter().map(|k| hex::encode(k.id)).collect();
        assert_eq!(PINNED.len(), 2, "expected ACTIVE + RECOVERY, got {ids:?}");
        // order is not part of the contract (a signature names its own key), membership is
        for want in ["d71158eefddd649c", "275e5c3a137be35a"] {
            assert!(ids.iter().any(|id| id == want), "{want} is not in the pinned table: {ids:?}");
        }
    }

    /// Naming a pinned key must not be enough to be believed — only signing as one is.
    ///
    /// Every pinned entry is forged in turn: a signature made by a key we hold the secret for,
    /// stamped with the REAL key's id. The id is untrusted routing metadata, so the only thing
    /// standing between this and acceptance is the signature check itself. If a refactor ever
    /// reduced verification to "is this id in PINNED", nothing else in the suite would notice.
    #[test]
    fn naming_a_pinned_key_does_not_forge_its_signature() {
        for k in PINNED {
            let sig = sign(&TEST_SEED, k.id, *b"Ed", DOC, "as if");
            assert!(
                matches!(verify(DOC, &sig), Err(TrustError::BadSignature)),
                "key {} accepted a signature it did not make",
                k.id.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
    }
    /// A signature produced by the PYTHON producer, parsed and checked by this reader.
    ///
    /// The two implementations share no code, so nothing else catches a disagreement about the
    /// wire format — and such a disagreement is not a test failure in either repo, it is a release
    /// nobody can install. Fixtures and provenance: `tests/interop/README.md`.
    ///
    /// The key comes from the fixture rather than from `PINNED`, so these bytes never carry
    /// authority in a real build; what is under test is the FORMAT, not the trust root.
    #[test]
    fn a_python_produced_signature_verifies_here() {
        let doc = include_bytes!("../tests/interop/manifest.json");
        let sig_text = include_str!("../tests/interop/manifest.json.minisig");
        let pub_text = include_str!("../tests/interop/test.pub");

        // second line of the .pub: base64(algo || key_id || 32-byte key)
        let blob = b64(pub_text.lines().nth(1).unwrap()).expect("pubkey line is base64");
        assert_eq!(&blob[..2], b"Ed", "the producer wrote a non-Ed algorithm");
        let key_id: KeyId = blob[2..10].try_into().unwrap();
        let key: [u8; 32] = blob[10..].try_into().expect("32-byte public key");

        let parsed = parse(sig_text).expect("the producer's .minisig must parse here");
        assert_eq!(parsed.algo, *b"Ed");
        assert_eq!(parsed.key_id, key_id, "signature key id != pubkey key id");

        let ed = |msg: &[u8], s: &[u8]| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key).verify(msg, s)
        };
        ed(doc, &parsed.sig).expect("the primary signature must verify");
        let mut global = Vec::new();
        global.extend_from_slice(&parsed.sig);
        global.extend_from_slice(parsed.trusted.as_bytes());
        ed(&global, &parsed.global).expect("the global signature must verify");

        // and it still fails closed on a tampered document
        let mut bad = doc.to_vec();
        bad[0] ^= 1;
        assert!(ed(&bad, &parsed.sig).is_err(), "a tampered document must not verify");
    }

}
