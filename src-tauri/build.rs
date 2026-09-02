//! Tauri's codegen, plus the two things this crate generates from signed inputs: `trust::PINNED`
//! and `mirror::BAKED`.
//!
//! The release public keys used to be a hand-transcribed byte-array literal in `trust.rs`. That is
//! the wrong shape for a trust root — the bytes have a published, authoritative form (the minisign
//! `.pub` files in `Pr0j3ctPh03nix/release-tooling`, pinned by commit SHA in both workflows), and a
//! transcription of them can only ever agree with it or be silently wrong. So they are read and
//! decoded here, and the table is generated.
//!
//! **A missing or malformed key file fails the build.** There is no fallback and no empty table: a
//! launcher whose trust root is empty refuses every release, and refuses it as "no release is
//! available" (see `TrustError`), which is by design indistinguishable from an outage. Such a
//! binary must not exist, so the refusal happens here, where somebody is watching.
//!
//! The BAKED MIRROR LIST is that same idea applied to the bootstrap problem: mirrors are discovered
//! from `mirrors.json`, which lives on GitHub or on a mirror, so a client that cannot reach GitHub
//! can never learn that any mirror exists. A build may ship without one (the ordinary local build
//! does), but a build that ships one whose signature does not check out has shipped a bootstrap
//! nothing can use — and the runtime symptom is silence, not an error. So that is refused here too.

use std::path::{Path, PathBuf};

/// The runtime's own signature reader, compiled into this script.
///
/// Not a copy and not a reimplementation: `src/minisig.rs` is the file the binary uses, pulled in
/// here so that a disagreement between the build script and the binary about what a `.minisig` (or
/// its base64) is cannot be expressed.
///
/// `#[path]` rather than `include!`: an `include!`d file may not carry `//!` module docs (they are
/// inner attributes, which macro expansion cannot introduce), and the format's own documentation
/// is the most useful thing in that file. A build script is an ordinary crate root, so a plain
/// module declaration with a path works and keeps the file readable as a module.
///
/// It is the RUNTIME's module, so the parts this script has no use for are not dead code in any
/// meaningful sense — the point of sharing it is that there is one implementation, not that both
/// callers reach every line of it.
#[allow(dead_code)]
#[path = "src/minisig.rs"]
mod minisig;

/// Overrides where the `.pub` files are read from. A local build points it at any
/// `release-tooling` checkout's `keys/`; CI does not set it — see `keys_dir`.
const KEYS_DIR_ENV: &str = "PHOENIX_KEYS_DIR";

/// Where the signed mirror list to bake in is read from: a directory holding `mirrors.json` and
/// `mirrors.json.minisig`, exactly as the registry repo publishes them.
///
/// A PATH, not a secret, and deliberately with NO DEFAULT — unset means "this build bakes no list",
/// which is what an ordinary local build wants and what every build before this one did. The
/// launcher's release workflow downloads the registry's latest release assets into a directory and
/// exports this; nothing else sets it. A var that IS set and points at nothing is a CI step that
/// failed quietly, so that case is refused rather than silently degraded.
const MIRRORS_DIR_ENV: &str = "PHOENIX_MIRRORS_DIR";

/// The two files the baked list is made of, in the directory `MIRRORS_DIR_ENV` names.
const MIRRORS_DOC: &str = "mirrors.json";
const MIRRORS_SIG: &str = "mirrors.json.minisig";

/// The mirror list's own format number, and the payload it must identify itself as. Both are wire
/// values shared with `phoenix-mirror-registry` and re-checked at RUNTIME by `mirror::signed`; they
/// are asserted here so a document this build could never act on is caught while somebody is
/// watching, rather than becoming silence in the field.
const MIRRORS_FORMAT: u64 = 1;
const MIRRORS_PAYLOAD_ID: &str = "mirrors";

/// The published keys, in the order they land in `PINNED`, each with the note it carries into the
/// generated file. Two of them, and the second is not redundant — see `trust::PINNED`.
const KEYS: [(&str, &str); 2] = [
    (
        "phoenix-active.pub",
        "ACTIVE — signs every release. Its private half is the CI signing secret, and release.yml \
         proves a freshly-made signature against this very file before it publishes anything.",
    ),
    (
        "phoenix-recovery.pub",
        "RECOVERY — the cold spare, kept offline and deliberately absent from CI, so a release it \
         signs is built and published BY HAND. A launcher accepts one only because of this entry, \
         which is why the entry exists years before it is ever used.",
    ),
];

fn main() {
    // the reader this script shares with the binary — a change to it has to re-run the generation
    println!("cargo:rerun-if-changed=src/minisig.rs");
    // before tauri's codegen: if this build is going to be refused, say so immediately rather than
    // after a round of work whose output is about to be thrown away
    let keys = generate_pinned_keys();
    generate_baked_mirrors(&keys);
    tauri_build::build()
}

/// Decode both `.pub` files and write `$OUT_DIR/pinned_keys.rs` — the array expression that
/// `trust::PINNED` includes. Returns the decoded ring, which is what the baked mirror list is then
/// verified against: the two have to be the same keys, and handing them along is how that is
/// guaranteed rather than merely arranged.
fn generate_pinned_keys() -> Vec<minisig::TrustedKey> {
    let dir = keys_dir();
    // emitted for both files up front, and whether or not either can be read: a build refused for
    // a missing key has to re-run by itself once that key appears
    for (file, _) in KEYS {
        println!("cargo:rerun-if-changed={}", dir.join(file).display());
    }

    let mut ring = Vec::with_capacity(KEYS.len());
    let mut out = String::from(
        "// @generated by src-tauri/build.rs — do not edit, and do not commit.\n\
         // The published release public keys, decoded from their minisign `.pub` files.\n[\n",
    );
    for (file, note) in KEYS {
        let path = dir.join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| refuse(&dir, &format!("{} cannot be read: {e}", path.display())));
        let (id, key) = parse_pub(&text).unwrap_or_else(|why| {
            refuse(&dir, &format!("{} is not a minisign public key: {why}", path.display()))
        });

        ring.push(minisig::TrustedKey { id, key });
        out.push_str(&comment(note));
        out.push_str(&format!("    // from {file}\n    TrustedKey {{\n"));
        out.push_str(&format!("        id: [{}],\n        key: [\n", hex_bytes(&id)));
        for row in key.chunks(8) {
            out.push_str(&format!("            {}, //\n", hex_bytes(row)));
        }
        out.push_str("        ],\n    },\n");
    }
    out.push_str("]\n");

    let dest = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let dest = dest.join("pinned_keys.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("cannot write {}: {e}", dest.display()));
    ring
}

/// Read the signed mirror list `MIRRORS_DIR_ENV` names, check it, and write
/// `$OUT_DIR/baked_mirrors.rs` — `None`, or the pair `mirror::BAKED` includes.
///
/// PATHS, not transcribed bytes: the generated file is `include_bytes!`/`include_str!` over the
/// files themselves, so the document is never copied and `cargo:rerun-if-changed` stays honest
/// about it.
///
/// Everything checkable is checked, against the same reader the runtime uses and the same keys it
/// will use: the signature (`minisig::verify` over the ring just generated), the format number, the
/// payload id, and a serial of at least 1. `mirror::signed::verify` re-checks all four at runtime —
/// this is not a substitute for that gate, it is the difference between a bad list failing a build
/// and a bad list shipping as silence.
fn generate_baked_mirrors(keys: &[minisig::TrustedKey]) {
    println!("cargo:rerun-if-env-changed={MIRRORS_DIR_ENV}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("baked_mirrors.rs");
    let write = |body: String| {
        std::fs::write(&out, body).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()))
    };
    let generated = "// @generated by src-tauri/build.rs — do not edit, and do not commit.\n";

    let Some(dir) = std::env::var_os(MIRRORS_DIR_ENV).map(PathBuf::from) else {
        write(format!(
            "{generated}// No {MIRRORS_DIR_ENV} was set, so this build bakes no mirror list: it \
             discovers\n// every mirror it will ever use, or none.\nNone\n"
        ));
        return;
    };
    let (doc_path, sig_path) = (dir.join(MIRRORS_DOC), dir.join(MIRRORS_SIG));
    for path in [&doc_path, &sig_path] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let read = |path: &Path| {
        std::fs::read(path).unwrap_or_else(|e| {
            refuse_mirrors(&dir, &format!("{} cannot be read: {e}", path.display()))
        })
    };
    let doc = read(&doc_path);
    let sig = String::from_utf8(read(&sig_path)).unwrap_or_else(|_| {
        refuse_mirrors(&dir, &format!("{} is not UTF-8", sig_path.display()))
    });

    let complain = |what: &str| -> ! {
        refuse_mirrors(&dir, &format!("{}: {what}", doc_path.display()))
    };
    if let Err(e) = minisig::verify(&doc, &sig, keys) {
        complain(&format!("it does not verify against the pinned release keys ({e})"));
    }
    let parsed: serde_json::Value = serde_json::from_slice(&doc)
        .unwrap_or_else(|e| complain(&format!("it is not readable JSON ({e})")));
    if parsed.get("format").and_then(serde_json::Value::as_u64) != Some(MIRRORS_FORMAT) {
        complain(&format!(
            "this launcher reads mirror-list format {MIRRORS_FORMAT}, and that is not the format \
             this document declares"
        ));
    }
    if parsed.get("payload_id").and_then(serde_json::Value::as_str) != Some(MIRRORS_PAYLOAD_ID) {
        complain(&format!("it does not identify itself as the {MIRRORS_PAYLOAD_ID:?} payload"));
    }
    // At least 1, not merely present. Zero is the one serial that verifies at runtime and then
    // cannot ratchet — `Settings::advance_serial` moves only on a strict increase, so a list
    // accepted at 0 leaves the anti-rollback floor at 0 forever, and that same floor is what tells
    // `mirror::bootstrap` this machine has never accepted a list. `mirror::signed::verify` refuses
    // one; refusing it HERE as well is the difference between a bad list being inert in the field
    // and a bad list never being baked into a binary at all.
    match parsed.get("serial").and_then(serde_json::Value::as_u64) {
        None => complain("it carries no integer serial, so nothing can order it against a later list"),
        Some(0) => complain(
            "it carries serial 0, which no client can ratchet forward from — the registry never \
             mints one, and the runtime reader refuses one",
        ),
        Some(_) => {}
    }

    write(format!(
        "{generated}// The signed mirror list this build bootstraps from, by PATH — the bytes are \
         never\n// copied, so `cargo:rerun-if-changed` stays honest about them.\n\
         Some((include_bytes!({}), include_str!({})))\n",
        rust_path(&doc_path),
        rust_path(&sig_path),
    ));
}

/// An absolute path as a Rust string literal. `Debug` on the `String` is the escaping: a Windows
/// path is full of backslashes, and every one of them has to survive into the generated source.
fn rust_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string())
}

/// Where the `.pub` files are read from: `PHOENIX_KEYS_DIR`, else `.tooling/keys` at the repo root.
///
/// Both workflows check `release-tooling` out to exactly that path, so the default resolves in CI
/// with no per-step configuration — which matters because EVERY cargo invocation needs the keys
/// (`cargo test` compiles this crate too), not just the one that packages a release.
fn keys_dir() -> PathBuf {
    println!("cargo:rerun-if-env-changed={KEYS_DIR_ENV}");
    if let Some(dir) = std::env::var_os(KEYS_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    manifest
        .parent()
        .expect("src-tauri/ has a parent — this crate does not live at a filesystem root")
        .join(".tooling")
        .join("keys")
}

/// An entry's note as wrapped `//` lines. Generated or not, this file is the readable statement of
/// what the two keys are for, and a 180-column line is not one.
fn comment(note: &str) -> String {
    let (mut out, mut line) = (String::new(), String::new());
    for word in note.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > 92 {
            out.push_str(&format!("    // {line}\n"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    out + &format!("    // {line}\n")
}

/// `0xd7, 0x11, …` — the bytes only, so the caller decides the brackets.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", ")
}

/// A minisign public key: an untrusted comment line, then base64 of `"Ed" || key_id || key`.
///
/// The comment is optional here and CRLF is tolerated — neither is signed, neither is an input to
/// anything, and a `.pub` that has been through a Windows editor is the ordinary case. Everything
/// the bytes actually claim IS checked: the length, the algorithm, and base64 that is exactly
/// base64.
fn parse_pub(text: &str) -> Result<([u8; 8], [u8; 32]), String> {
    let mut body = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("untrusted comment:"));
    let line = body.next().ok_or("it has no key line at all")?;
    if body.next().is_some() {
        return Err("it has more than one key line".into());
    }
    let blob = minisig::b64(line).ok_or("the key line is not standard base64")?;
    if blob.len() != 42 {
        return Err(format!(
            "the key line decodes to {} bytes, not 42 (2-byte algorithm + 8-byte key id + 32-byte key)",
            blob.len()
        ));
    }
    if blob[..2] != *b"Ed" {
        return Err(format!(
            "it declares algorithm {:?}, and only \"Ed\" (pure Ed25519) is a key this launcher can verify with",
            String::from_utf8_lossy(&blob[..2])
        ));
    }
    Ok((
        blob[2..10].try_into().expect("10 - 2 == 8"),
        blob[10..].try_into().expect("42 - 10 == 32"),
    ))
}

/// Stop the build over the BAKED MIRROR LIST, saying what is wrong and what would fix it.
///
/// A separate voice from `refuse` because the fix is a different one: an unreadable key file means
/// the build has no trust root at all, while a bad mirror list means the bootstrap it would ship is
/// one no client could act on — and the way out of the second is to unset the variable, which is a
/// perfectly good build.
fn refuse_mirrors(dir: &Path, problem: &str) -> ! {
    panic!(
        "\n\
         the mirror list to bake in could not be used, so this launcher will not be built.\n\
         \n\
         {problem}\n\
         \n\
         `mirror::BAKED` is the list a client bootstraps from when it cannot reach GitHub at all —\n\
         which is the entire audience for mirrors. A list that does not verify ships a bootstrap\n\
         nothing can use, and the runtime symptom is SILENCE: such a client simply never learns\n\
         that a mirror exists. It is refused here instead.\n\
         \n\
         mirror list directory: {}\n\
         expected in it: {MIRRORS_DOC}, {MIRRORS_SIG}\n\
         \n\
         Both are release assets of Pr0j3ctPh03nix/phoenix-mirror-registry. To build WITHOUT a\n\
         baked list — which is an ordinary, supported build — leave {MIRRORS_DIR_ENV} unset.\n",
        dir.display(),
    )
}

/// Stop the build, saying what is wrong and what would fix it.
fn refuse(dir: &Path, problem: &str) -> ! {
    panic!(
        "\n\
         the pinned release keys could not be read, so this launcher will not be built.\n\
         \n\
         {problem}\n\
         \n\
         `trust::PINNED` is GENERATED from the published minisign public keys of\n\
         Pr0j3ctPh03nix/release-tooling. Without them there is no trust root, and a launcher with\n\
         an empty trust root refuses every release as \"no release is available\" — a silent,\n\
         permanent failure indistinguishable from an outage. It is refused here instead.\n\
         \n\
         keys directory: {}\n\
         expected in it: {}\n\
         \n\
         Both workflows check release-tooling out to `.tooling/` at the repo root, which is what\n\
         the default path points at. For a local build, point {KEYS_DIR_ENV} at any checkout:\n\
         \n\
         \x20    PHOENIX_KEYS_DIR=/path/to/release-tooling/keys cargo build\n",
        dir.display(),
        KEYS.map(|(file, _)| file).join(", "),
    )
}
