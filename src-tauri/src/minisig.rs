//! The `.minisig` signature FORMAT, and nothing else.
//!
//! Split out from `trust.rs` for one reason: `build.rs` `include!`s this file. The build script
//! decodes the published `.pub` keys and verifies the baked mirror list, and both of those are
//! exactly the parsing this module does — so a second copy there could only ever agree with this
//! one or be silently wrong, in the one place whose whole job is refusing things. What stays in
//! `trust.rs` is WHICH keys are believed (`PINNED`) and WHAT a document has to say about itself
//! (`Payload`, `accept`); this module knows neither, which is why `verify` takes its key ring as a
//! parameter rather than reaching for a table.
//!
//! Nothing here may reference the rest of the crate: an `include!`d file is compiled twice, once
//! inside this binary and once inside a build script that has no `crate::` to reach into.
//!
//! ## The format — a shared contract, not an implementation detail
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

/// minisign's 8-byte key identifier. Names WHICH key signed, and nothing else — it is not a
/// secret, not a checksum, and carries no authority of its own.
pub type KeyId = [u8; 8];

/// A key a caller is willing to believe. Which keys those are is `trust::PINNED`'s business, not
/// this module's: the format has no opinion about whose signature it is reading.
pub struct TrustedKey {
    pub id: KeyId,
    /// Raw 32-byte Ed25519 public key.
    pub key: [u8; 32],
}

/// Why a signature file is not believable. Every arm is a fact about the FILE — its shape, its
/// algorithm, its key, its two signatures. Whether the DOCUMENT it covers is the one that was
/// asked for is `trust::TrustError`'s question and is asked separately.
///
/// Typed (like `NetKind` and `UnsupportedSchema`) so the command layer can classify without
/// matching on message text — see `views::wire_kind`, which maps every one of these to `notFound`:
/// an unverifiable release is a release we do not have, and that answer must never stop somebody
/// playing a game that is already installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigError {
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
}

impl std::fmt::Display for SigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "the signature file is malformed: {why}"),
            Self::Algorithm(a) => write!(
                f,
                "the signature uses algorithm {:?}, and this launcher only accepts \"Ed\" \
                 (pure Ed25519 over the file bytes)",
                String::from_utf8_lossy(a)
            ),
            Self::UnknownKey(id) => {
                // Spelled out rather than reached for through `hex`: this file is compiled into a
                // build script too, and a build script that pulls a crate in to print eight bytes
                // has bought a supply chain for the one input whose whole job is to be trustworthy.
                write!(f, "the release is signed by an unknown key (")?;
                for b in id {
                    write!(f, "{b:02x}")?;
                }
                write!(f, ")")
            }
            Self::BadSignature => write!(f, "the signature does not match the file"),
            Self::BadComment => write!(f, "the signature's trusted comment has been tampered with"),
        }
    }
}

impl std::error::Error for SigError {}

/// Verify `minisig` over `data` against `keys`, returning the id of the key that signed it.
///
/// The order is the contract, and every step is a different refusal: parse, decode, algorithm,
/// KEY (distinct from a bad signature — see `SigError::UnknownKey`), the signature over the
/// document, then the global signature over `signature || trusted comment`.
///
/// `keys` is a parameter, not a table this module owns, because there are two callers with two
/// trust roots: the launcher verifies against `trust::PINNED`, and `build.rs` verifies the baked
/// mirror list against the very keys it has just decoded for that table. One verifier, two rings.
/// It is an ITERATOR rather than a slice because the launcher's ring is not one — a test build
/// appends the suite's own key to the generated table (`trust::pinned`), and materializing that
/// into a Vec per verification would be an allocation bought purely to satisfy a signature.
///
/// Nothing here can panic on hostile input: every length is checked before it is sliced, and the
/// base64 decoder rejects rather than guesses. A verifier that crashes on a malformed signature
/// file has handed the attacker a better outcome than the one they were reaching for.
pub fn verify<'k>(
    data: &[u8],
    minisig: &str,
    keys: impl IntoIterator<Item = &'k TrustedKey>,
) -> Result<KeyId, SigError> {
    let sig = parse(minisig)?;
    if sig.algo != *b"Ed" {
        return Err(SigError::Algorithm(sig.algo));
    }
    let key =
        keys.into_iter().find(|k| k.id == sig.key_id).ok_or(SigError::UnknownKey(sig.key_id))?;
    let ed = |msg: &[u8], signature: &[u8]| {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key.key)
            .verify(msg, signature)
    };
    ed(data, &sig.sig).map_err(|_| SigError::BadSignature)?;
    // binds the trusted comment to the signature it travels with, so the one attacker-visible
    // string in the file cannot be rewritten under a signature that still verifies
    let mut global = Vec::with_capacity(64 + sig.trusted.len());
    global.extend_from_slice(&sig.sig);
    global.extend_from_slice(sig.trusted.as_bytes());
    ed(&global, &sig.global).map_err(|_| SigError::BadComment)?;
    Ok(sig.key_id)
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

fn parse(text: &str) -> Result<Minisig<'_>, SigError> {
    use SigError::Malformed;
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
    let head: [u8; 74] =
        head.try_into().map_err(|_| Malformed("the signature line is not 74 bytes"))?;
    let global = b64(cr(global_line)).ok_or(Malformed("the global signature line is not base64"))?;
    let global: [u8; 64] =
        global.try_into().map_err(|_| Malformed("the global signature is not 64 bytes"))?;

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
///
/// `pub` because `build.rs` reads a minisign `.pub` key line with it — the same alphabet, the same
/// intolerance, one implementation.
pub fn b64(s: &str) -> Option<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    //! The FORMAT, against a ring holding only the suite's own key — nothing here is about which
    //! keys ship (`trust.rs` owns that and tests it there).
    use super::*;
    use crate::trust::testing::*;

    const DOC: &[u8] = br#"{"payload_id":"mod","serial":7,"version":"1.0.0","files":[]}"#;

    /// The suite's signing key as a one-entry ring.
    fn ring() -> &'static [TrustedKey] {
        std::slice::from_ref(key())
    }

    fn check(data: &[u8], sig: &str) -> Result<KeyId, SigError> {
        verify(data, sig, ring())
    }

    #[test]
    fn a_good_signature_verifies_and_names_its_key() {
        assert_eq!(check(DOC, &test_sig(DOC)).unwrap(), TEST_KEY_ID);
    }

    /// The whole point: the signature is over the document's bytes, so one changed byte anywhere
    /// in it must fail — including in a place a careless verifier might not cover (the last one).
    #[test]
    fn one_flipped_byte_fails() {
        let sig = test_sig(DOC);
        for i in [0, DOC.len() / 2, DOC.len() - 1] {
            let mut doc = DOC.to_vec();
            doc[i] ^= 0x01;
            assert_eq!(check(&doc, &sig), Err(SigError::BadSignature), "byte {i}");
        }
        // and a truncated document is not a shorter valid one
        assert_eq!(check(&DOC[..DOC.len() - 1], &sig), Err(SigError::BadSignature));
    }

    /// A perfectly-formed signature from a key the ring does not hold is its OWN answer.
    /// Collapsing it into "bad signature" would describe an attack where the honest explanation is
    /// usually a key rotation this build predates — and the two want different responses.
    #[test]
    fn an_unpinned_key_is_reported_as_unknown_not_as_a_bad_signature() {
        let sig = sign(&[9u8; 32], *b"OTHERKEY", *b"Ed", DOC, "signed by somebody else");
        assert_eq!(check(DOC, &sig), Err(SigError::UnknownKey(*b"OTHERKEY")));
        // the same key id with the right SEED is still unknown — the id is a label, not authority
        let sig = sign(&TEST_SEED, *b"OTHERKEY", *b"Ed", DOC, "right key, wrong id");
        assert_eq!(check(DOC, &sig), Err(SigError::UnknownKey(*b"OTHERKEY")));
    }

    /// `ED` is minisign's Blake2b-prehashed variant. We do not implement it, so it must be refused
    /// by NAME — reading it as `Ed` would verify a hash-of-a-hash and call the file signed.
    #[test]
    fn the_prehashed_algorithm_is_refused() {
        let sig = sign(&TEST_SEED, TEST_KEY_ID, *b"ED", DOC, "prehashed");
        assert_eq!(check(DOC, &sig), Err(SigError::Algorithm(*b"ED")));
        // ...and so is anything else in that field
        let sig = sign(&TEST_SEED, TEST_KEY_ID, *b"xx", DOC, "nonsense");
        assert_eq!(check(DOC, &sig), Err(SigError::Algorithm(*b"xx")));
    }

    /// The global signature is what makes the trusted comment trusted. Rewriting it while keeping
    /// everything else must fail — otherwise the field is a lie in a file that claims to be signed.
    #[test]
    fn a_rewritten_trusted_comment_fails_the_global_signature() {
        let sig = test_sig(DOC);
        let tampered =
            sig.replace("trusted comment: test signature", "trusted comment: something else");
        assert_ne!(tampered, sig);
        assert_eq!(check(DOC, &tampered), Err(SigError::BadComment));
        // an empty comment is legal, but only if it is the one that was signed
        let empty = sign(&TEST_SEED, TEST_KEY_ID, *b"Ed", DOC, "");
        assert!(check(DOC, &empty).is_ok());
        assert_eq!(
            check(DOC, &empty.replace("trusted comment: \n", "trusted comment: x\n")),
            Err(SigError::BadComment)
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
            lines[..3].join("\n"),                        // three lines
            format!("{good}extra line\n"),                // five
            lines.join("\r\n"),                           // CRLF is tolerated — see below
            good.replace("untrusted comment:", "hello:"), // wrong first line
            good.replace("trusted comment:", "comment:"), // wrong third line
            format!("{}\n!!!!{}\n{}\n{}\n", lines[0], &lines[1][4..], lines[2], lines[3]), // not base64
            format!("{}\n{}\n{}\n{}\n", lines[0], &lines[1][..96], lines[2], lines[3]), // short sig
            format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], &lines[3][..84]), // short global
            format!("{}\n{}\n{}\n\n", lines[0], lines[1], lines[2]),                    // empty global
        ];
        for (i, text) in bad.iter().enumerate() {
            match check(DOC, text) {
                // CRLF is the one entry here that is deliberately NOT a failure: a producer on
                // Windows writes it by accident, and the bytes the signature covers are the
                // spec's either way.
                Ok(_) => assert_eq!(i, 4, "case {i} must not verify: {text:?}"),
                Err(SigError::Malformed(_)) => {}
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

    /// A signature produced by the PYTHON producer, parsed and checked by this reader.
    ///
    /// The two implementations share no code, so nothing else catches a disagreement about the
    /// wire format — and such a disagreement is not a test failure in either repo, it is a release
    /// nobody can install. Fixtures and provenance: `tests/interop/README.md`.
    ///
    /// The key comes from the fixture rather than from `trust::PINNED`, so these bytes never carry
    /// authority in a real build; what is under test is the FORMAT, not the trust root — which is
    /// exactly why `verify` takes its ring as a parameter and this test can hand it one.
    #[test]
    fn a_python_produced_signature_verifies_here() {
        let doc = include_bytes!("../tests/interop/manifest.json");
        let sig_text = include_str!("../tests/interop/manifest.json.minisig");
        let pub_text = include_str!("../tests/interop/test.pub");

        // second line of the .pub: base64(algo || key_id || 32-byte key)
        let blob = b64(pub_text.lines().nth(1).unwrap()).expect("pubkey line is base64");
        assert_eq!(&blob[..2], b"Ed", "the producer wrote a non-Ed algorithm");
        let fixture = TrustedKey {
            id: blob[2..10].try_into().unwrap(),
            key: blob[10..].try_into().expect("32-byte public key"),
        };

        let parsed = parse(sig_text).expect("the producer's .minisig must parse here");
        assert_eq!(parsed.algo, *b"Ed");
        assert_eq!(parsed.key_id, fixture.id, "signature key id != pubkey key id");
        assert_eq!(
            verify(doc, sig_text, std::slice::from_ref(&fixture)).unwrap(),
            fixture.id,
            "the producer's signature must verify against the producer's key"
        );

        // and it still fails closed on a tampered document
        let mut bad = doc.to_vec();
        bad[0] ^= 1;
        assert_eq!(
            verify(&bad, sig_text, std::slice::from_ref(&fixture)),
            Err(SigError::BadSignature),
            "a tampered document must not verify"
        );
    }
}
