//! What makes a downloaded document believable — WHOSE signature, and WHICH document.
//!
//! Every sha256 this launcher acts on comes out of a manifest, and the manifest itself used to be
//! covered by nothing but TLS — which authenticates a HOST, not a DOCUMENT. That is the wrong end
//! of the chain to rest on once releases can be served from third-party mirrors: the assets are
//! already verified against the manifest (install.rs), so signing the manifest is what closes it.
//! Everything downstream of here can go on being a hash comparison; this module is what makes the
//! hashes worth comparing against.
//!
//! The signature FORMAT is `minisig.rs` — its own module because `build.rs` includes it, so the
//! build script and the binary cannot disagree about what a `.minisig` is. What lives here is the
//! part a build script has no business knowing: the pinned keys, and what a document must say
//! about itself.
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
//!   highest serial this machine has accepted.

use crate::manifest::Manifest;
use crate::minisig::{self, SigError};
pub use crate::minisig::{KeyId, TrustedKey};

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

/// The keys a document may be signed by, in no particular order — a signature names its own key.
///
/// Compiled into the binary: there is no key distribution problem to solve here, because a key
/// fetched from the payload's own channel is not a trust root — it is one more thing whoever
/// serves the payload gets to choose. Only a key that arrived WITH the binary can say anything the
/// binary did not already have to take on faith.
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

/// Why a document is not the one that was asked for. The signature FILE's own refusals are
/// `minisig::SigError`; these are the questions a valid signature leaves open.
///
/// Typed (like `NetKind` and `UnsupportedSchema`) so the command layer can classify without
/// matching on message text — see `views::wire_kind`, which maps these and `SigError` alike to
/// `notFound`: an unverifiable release is a release we do not have, and that answer must never
/// stop somebody playing a game that is already installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// The release publishes the document but no signature for it. Not a `SigError`: there is no
    /// signature file to have a format complaint about, and the fact is about the RELEASE.
    Unsigned(String),
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
            // "document", not "manifest": the mirror list is neither, and these two refusals are
            // the ones it shares with a manifest (see `accept_ident`).
            Self::WrongPayload { expected, found } => match found {
                Some(got) => write!(f, "this is a {got:?} document, but {expected:?} was asked for"),
                None => write!(f, "the document does not say which payload it is ({expected:?} was asked for)"),
            },
            Self::StaleSerial { payload, found, floor } => match found {
                Some(n) => write!(
                    f,
                    "this {payload} release is serial {n}, older than {floor}, which this machine \
                     has already accepted"
                ),
                None => write!(f, "this {payload} document carries no serial, so it cannot be shown to be current"),
            },
        }
    }
}

impl std::error::Error for TrustError {}

/// Which signed payload a document is expected to describe.
///
/// NOT every one of these is a manifest, and `Mirrors` is why this enum is not named after one. The
/// published mirror list is its own small format — `format`, not `schema`, with no files and no
/// bundles (`phoenix-mirror-registry/generate_mirror_list.py` is the whole of it) — and what makes
/// it a payload HERE is only that it is sealed by the same key and ordered by the same per-payload
/// serial, which is exactly what `verify` and `accept_ident` are asked about. Nothing else in the
/// format is shared, and nothing here needs it to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// The dist repo — the shim and its game files.
    Mod,
    /// The launcher's own releases (self-update).
    Launcher,
    /// The base game.
    Game,
    /// The published list of download mirrors. It installs nothing; what it changes is where every
    /// FUTURE install comes from, and the launcher persists it — so a hostile answer is not a bad
    /// download, it is a permanent rewrite of this machine's sources. That is the whole reason it
    /// is signed and ratcheted like a payload. Read by `mirror`'s list machinery and nothing else.
    Mirrors,
}

impl Payload {
    /// The string a signed document's `payload_id` must carry to be this payload.
    ///
    /// These are WIRE values shared with every producer (release-tooling's `PAYLOAD_IDS`, and the
    /// mirror registry's `PAYLOAD_ID`); a typo here is not a bug that shows up as a bug, it is
    /// every document of that kind quietly becoming unreadable.
    pub fn id(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::Launcher => "launcher",
            Self::Game => "game",
            Self::Mirrors => "mirrors",
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

/// Verify `minisig` over `data` against THE PINNED RING, returning the id of the key that signed
/// it.
///
/// The format check is `minisig::verify`; what this adds is the one thing the format has no
/// opinion about — whose signatures this build believes. Keeping the two apart is what lets
/// `build.rs` run the identical verifier over a ring it decoded itself, without a second copy of
/// the parser existing anywhere.
pub fn verify(data: &[u8], minisig: &str) -> Result<KeyId, SigError> {
    minisig::verify(data, minisig, pinned())
}

/// The document names the payload the caller asked for and is not older than `floor`. Returns the
/// accepted serial, which is what the caller ratchets `floor` forward with.
///
/// Separate from `verify` because it asks a different question of a different input: `verify`
/// establishes that we produced the bytes, this establishes that they are the bytes we wanted.
/// Both have to pass, and neither implies the other.
pub fn accept(payload: Payload, manifest: &Manifest, floor: u64) -> Result<u64, TrustError> {
    accept_ident(payload, manifest.payload_id.as_deref(), manifest.serial, floor)
}

/// `accept` over the two fields it actually reads, for a signed document that is NOT a manifest.
///
/// The mirror list is the caller (`mirror::signed`): a different format that shares none of a
/// manifest's shape and all of its identity-and-freshness question. One function rather than two,
/// so "a document must name its payload, and must not be older than this machine has already
/// accepted" has exactly one implementation — a second copy would be free to disagree about the
/// case that decides everything, which is the one where a field is ABSENT. Both `None`s below are
/// refusals, and a reader that spelled either of them as a default would accept an unidentified or
/// unorderable document as current.
pub fn accept_ident(
    payload: Payload,
    payload_id: Option<&str>,
    serial: Option<u64>,
    floor: u64,
) -> Result<u64, TrustError> {
    if payload_id != Some(payload.id()) {
        return Err(TrustError::WrongPayload {
            expected: payload.id(),
            found: payload_id.map(str::to_string),
        });
    }
    // `>=`, not `>`: the same release re-fetched (every check does exactly that) is the common
    // case, and refusing it would make the second check of any release fail.
    match serial {
        Some(n) if n >= floor => Ok(n),
        found => Err(TrustError::StaleSerial { payload: payload.id(), found, floor }),
    }
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
                matches!(verify(DOC, &sig), Err(SigError::BadSignature)),
                "key {} accepted a signature it did not make",
                k.id.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
    }

    /// `verify` is the FORMAT verifier plus this module's ring, and the ring is the only thing it
    /// adds — so what is worth pinning here is that the ring is actually the one being consulted.
    /// A signature by a key nothing in this build holds is `UnknownKey`, and the suite's own key
    /// (appended to the table in test builds, and only there) is believed.
    #[test]
    fn verify_asks_the_pinned_ring_and_nothing_else() {
        assert_eq!(verify(DOC, &test_sig(DOC)).unwrap(), TEST_KEY_ID);
        let stranger = sign(&[9u8; 32], *b"OTHERKEY", *b"Ed", DOC, "signed by somebody else");
        assert_eq!(verify(DOC, &stranger), Err(SigError::UnknownKey(*b"OTHERKEY")));
    }
}
