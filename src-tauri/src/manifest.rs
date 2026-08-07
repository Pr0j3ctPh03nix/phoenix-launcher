//! The release manifest, produced by the dist repo's tools/gen_manifest.py.
//!
//! Spec: `docs/manifest-format.md` in the dist repo. Its conformance fixtures are vendored at
//! `src-tauri/manifest-fixtures/` and asserted by the tests at the bottom of this file.
//!
//! Compatibility rests on ONE field, `schema`: the manifest declares the format it is written in,
//! and deciding whether that can be read is entirely the reader's job. Note the direction — the
//! producer never names a launcher version, since a dist repo that has to know "app 1.3.0 will
//! understand this" is a forward reference to a build that does not exist yet. (This replaced an
//! earlier `min_launcher` field, which pointed the dependency the wrong way.)
//!
//! Unknown keys are ignored everywhere, by design: the producer treats additive keys as
//! backward-compatible and will NOT bump `schema` for them, so erroring on an unrecognised field
//! would force a needless bump for every addition — locking out readers that would have coped.
//! serde ignores unknown fields by default; do not add `deny_unknown_fields` to anything here.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// Manifest format versions this build understands.
///
///   1  the original flat {files, remove} document — predates the `schema` key entirely
///   2  adds options[] (choice/toggle); an unknown `kind` is fatal to a v1 reader
///
/// Raise `MAX_SCHEMA` in the same change that teaches the reader the new format, never before.
pub const MIN_SCHEMA: u32 = 1;
pub const MAX_SCHEMA: u32 = 2;

/// What an ABSENT `schema` means. Schema 1 predates the key, so requiring it would reject every
/// manifest published before it existed.
const LEGACY_SCHEMA: u32 = 1;

/// The manifest is written in a format this build does not understand. Rooted in the error chain
/// so the shell can put a `tooOld` kind on the wire and the UI can say "update the launcher".
///
/// Distinct from a parse failure on purpose: a manifest from the future is not malformed, and
/// telling the user their file is corrupt when they simply need a newer build is the single
/// worst diagnosis this reader could give.
#[derive(Debug, Clone)]
pub struct UnsupportedSchema {
    pub found: u32,
    pub supported: u32,
}

impl std::fmt::Display for UnsupportedSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this release uses manifest schema {} but this launcher reads up to {} — update the launcher",
            self.found, self.supported
        )
    }
}

impl std::error::Error for UnsupportedSchema {}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Format version of this document, as decided by `read_schema` during `parse`.
    ///
    /// NOT deserialized: serde's `default` only covers a MISSING key, so an explicit
    /// `"schema": null` — which the permissive pass correctly reads as legacy — would still die
    /// here with "invalid type: null, expected u32". The permissive pass is the single source of
    /// truth for this field; `parse` assigns it afterwards.
    #[serde(skip, default = "default_schema")]
    pub schema: u32,
    pub version: String,
    /// Markdown release notes ("What's new"), embedded by gen_manifest.py --notes-file. Optional so
    /// manifests written before this field are still accepted.
    #[serde(default)]
    pub notes: Option<String>,
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub remove: Vec<RemoveEntry>,
    /// User-selectable content: `choice` (one variant of a single dest) and `toggle` (an optional
    /// file set). Absent in older manifests.
    #[serde(default)]
    pub options: Vec<OptionEntry>,
}

fn default_schema() -> u32 {
    LEGACY_SCHEMA
}

impl Manifest {
    /// Read a manifest, refusing a format this build does not understand.
    ///
    /// TWO PASSES, ON PURPOSE — this ordering is the whole point of the compatibility design.
    /// `schema` is read from a permissive `Value` BEFORE the document is deserialized into
    /// `Manifest`, because a manifest from the future can carry an option `kind` (or a required
    /// field) we have no representation for. Parsing first would turn "update the launcher" into
    /// an unintelligible syntax error — exactly what the `future-unknown-option-kind` fixture
    /// exists to catch. The `Value` is reused for the real parse, so this costs one JSON parse.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let doc: serde_json::Value =
            serde_json::from_slice(bytes).context("parsing manifest.json")?;
        let schema = read_schema(&doc)?;
        if !(MIN_SCHEMA..=MAX_SCHEMA).contains(&schema) {
            return Err(anyhow!(UnsupportedSchema { found: schema, supported: MAX_SCHEMA }));
        }
        let mut manifest: Self = serde_json::from_value(doc).context("parsing manifest.json")?;
        manifest.schema = schema;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Check every install destination in the document before anything can act on one.
    fn validate(&self) -> Result<()> {
        for f in &self.files {
            check_dest(&f.dest)?;
        }
        for r in &self.remove {
            check_dest(&r.dest)?;
        }
        for o in &self.options {
            if let Some(d) = &o.dest {
                check_dest(d)?;
            }
            for f in &o.files {
                check_dest(&f.dest)?;
            }
        }
        Ok(())
    }
}

/// The declared format version. Absent (or null) means `LEGACY_SCHEMA`. Present but not a whole
/// number is a MALFORMED document, not a legacy one — quietly reading it as 1 would install a
/// format that was never actually checked.
fn read_schema(doc: &serde_json::Value) -> Result<u32> {
    match doc.get("schema") {
        None | Some(serde_json::Value::Null) => Ok(LEGACY_SCHEMA),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .with_context(|| format!("manifest `schema` is not a whole number: {v}")),
    }
}

/// Reject a `dest` that could escape the game root. The spec puts this on the reader deliberately:
/// `dest` is joined straight onto the game directory, so it is the one field that turns a
/// compromised — or merely buggy — manifest into an arbitrary file write.
///
/// BOTH separators are checked, not just the forward slash the producer emits. Windows treats
/// `..\` as a parent traversal too, so splitting on '/' alone would wave `game\..\..\Windows`
/// straight through.
fn check_dest(dest: &str) -> Result<()> {
    let reject = |why: &str| Err(anyhow!("refusing manifest dest {dest:?}: {why}"));
    if dest.is_empty() {
        return reject("empty");
    }
    if dest.starts_with('/') || dest.starts_with('\\') {
        return reject("absolute path");
    }
    // "C:/…" (drive-relative or absolute) and NTFS alternate data streams ("file:stream") both
    // hide behind a colon, and neither can appear in a legitimate relative install path
    if dest.contains(':') {
        return reject("drive letter or alternate data stream");
    }
    for raw in dest.split(['/', '\\']) {
        if raw == ".." {
            return reject("escapes the game root");
        }
        if raw.is_empty() {
            return reject("empty path component");
        }
        // Win32 strips trailing spaces and dots from a path component BEFORE resolving it, so the
        // device-name comparison below has to see what the filesystem will see, not what the
        // manifest wrote: `GetFullPathName("C:\a\NUL ")` resolves to `\\.\NUL` (verified).
        let part = raw.trim_end_matches([' ', '.']);
        if part.is_empty() || part == ".." {
            return reject("empty path component");
        }
        // "NUL", "CON.txt", "com1" … — Windows resolves reserved device names (with or without
        // an extension, any case) to the device itself, not a file; writing there hangs or
        // vanishes bytes rather than installing anything
        // byte-wise on purpose: str slicing (`stem[..3]`) panics on a multibyte character
        // boundary, and file names here can legitimately be Cyrillic
        let stem = part.split('.').next().unwrap_or(part).as_bytes();
        let reserved = stem.eq_ignore_ascii_case(b"CON")
            || stem.eq_ignore_ascii_case(b"PRN")
            || stem.eq_ignore_ascii_case(b"AUX")
            || stem.eq_ignore_ascii_case(b"NUL")
            || (stem.len() == 4
                && (stem[..3].eq_ignore_ascii_case(b"COM") || stem[..3].eq_ignore_ascii_case(b"LPT"))
                && stem[3].is_ascii_digit());
        if reserved {
            return reject("Windows reserved device name");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// Asset name in the release.
    pub name: String,
    /// Install destination, relative to the game root (the folder containing `game/`).
    pub dest: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoveEntry {
    /// Path (relative to the game root) to delete from a client carrying an earlier release.
    pub dest: String,
}

/// A display string, either plain or per-language (`{"en": ..., "ru": ...}`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Label {
    Plain(String),
    Localized(HashMap<String, String>),
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum OptionKind {
    /// Exactly one of `variants` is installed at `dest`.
    Choice,
    /// `files` are installed when enabled, absent when disabled.
    Toggle,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptionEntry {
    pub id: String,
    pub kind: OptionKind,
    pub label: Label,
    #[serde(default)]
    pub description: Option<Label>,
    /// choice: the default variant id (JSON string); toggle: default enabled (JSON bool).
    pub default: serde_json::Value,
    /// choice only: the shared install destination of every variant.
    #[serde(default)]
    pub dest: Option<String>,
    #[serde(default)]
    pub variants: Vec<Variant>,
    /// toggle only.
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// One selectable asset of a `choice` option; installs at the option's `dest`.
#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    pub id: String,
    pub label: Label,
    /// Asset name in the release.
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The dist repo's conformance fixtures, vendored so this suite is hermetic (the launcher
    /// repo builds and tests without a sibling checkout). Refresh by re-copying
    /// `client-dist-staging/docs/manifest-fixtures/` over `src-tauri/manifest-fixtures/`.
    fn fixtures() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/manifest-fixtures"))
    }

    /// Was this refused SPECIFICALLY for its schema, rather than as a parse/validation failure?
    fn refused_for_schema(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.downcast_ref::<UnsupportedSchema>().is_some())
    }

    /// Walks `index.json` and asserts every documented expectation against the real reader.
    #[test]
    fn dist_repo_conformance_suite() {
        let dir = fixtures();
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();

        // Tripwire: the suite states the producer's current schema. If a fixture refresh brings a
        // higher one, fail with the actual instruction instead of a baffling `accept` failure.
        let producer = index["schema"].as_u64().unwrap() as u32;
        assert_eq!(
            producer, MAX_SCHEMA,
            "fixtures are schema {producer}, this reader reads up to {MAX_SCHEMA} — teach it the \
             new format first, then raise MAX_SCHEMA in the same change"
        );
        let future = index["future_schema"].as_u64().unwrap() as u32;
        assert!(future > MAX_SCHEMA, "the 'future' fixtures ({future}) are not actually ahead of us");

        let cases = index["cases"].as_array().expect("index.json carries cases[]");
        assert!(!cases.is_empty(), "the conformance suite is empty");
        for case in cases {
            let file = case["file"].as_str().expect("case.file");
            let expect = case["expect"].as_str().expect("case.expect");
            let why = case["why"].as_str().unwrap_or("");
            let bytes = std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            let got = Manifest::parse(&bytes);
            match expect {
                "accept" => {
                    if let Err(e) = got {
                        panic!("{file} must be ACCEPTED but failed: {e:#}\n  why: {why}");
                    }
                }
                "refuse:schema" => {
                    let Err(e) = got else {
                        panic!("{file} must be REFUSED but parsed clean\n  why: {why}");
                    };
                    assert!(
                        refused_for_schema(&e),
                        "{file} was refused, but NOT for its schema — it failed as: {e:#}\n  why: {why}"
                    );
                }
                other => panic!("{file}: unknown expectation {other:?} in index.json"),
            }
        }
    }

    /// The accepted fixtures must not merely parse — they must yield what the spec describes.
    /// "Accept" that silently drops the options list would pass the suite and install nothing.
    #[test]
    fn current_fixture_parses_into_the_documented_shape() {
        let m = Manifest::parse(&std::fs::read(fixtures().join("current.json")).unwrap()).unwrap();
        assert_eq!(m.schema, 2);
        assert_eq!(m.version, "1.0.0");
        assert!(m.notes.is_some_and(|n| n.contains("Added")));
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].dest, "game/bin/win64/winmm.dll");
        assert_eq!(m.files[0].size, 1560);
        assert_eq!(m.remove.len(), 1);
        assert_eq!(m.remove[0].dest, "game/dota/scripts/regions.txt");

        assert_eq!(m.options.len(), 2);
        let choice = &m.options[0];
        assert_eq!(choice.id, "lighting");
        assert_eq!(choice.kind, OptionKind::Choice);
        assert_eq!(choice.dest.as_deref(), Some("game/dota_phoenix/maps/dota.vpk"));
        assert_eq!(choice.default, serde_json::json!("original")); // variant id for a choice
        assert_eq!(choice.variants.len(), 2);
        assert_eq!(choice.variants[0].name, "opt__lighting__mod.vpk");
        assert!(matches!(&choice.label, Label::Localized(m) if m["ru"] == "Освещение"));

        let toggle = &m.options[1];
        assert_eq!(toggle.kind, OptionKind::Toggle);
        assert_eq!(toggle.default, serde_json::json!(false)); // bool for a toggle
        assert_eq!(toggle.files.len(), 1);
        assert_eq!(toggle.files[0].dest, "game/dota_phoenix/pak01_dir.vpk");
    }

    #[test]
    fn absent_schema_reads_as_1() {
        let m =
            Manifest::parse(&std::fs::read(fixtures().join("legacy-no-schema.json")).unwrap()).unwrap();
        assert_eq!(m.schema, LEGACY_SCHEMA);
        assert_eq!(m.files.len(), 1);
        assert!(m.options.is_empty(), "a v1 manifest simply has no options");
    }

    /// Unknown keys are ignored, not rejected — and the recognised ones around them still land.
    #[test]
    fn additive_unknown_keys_are_ignored() {
        let m = Manifest::parse(&std::fs::read(fixtures().join("additive-unknown-keys.json")).unwrap())
            .unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.options.len(), 2);
        // the unknown top-level/entry keys sit beside real ones; nothing may be lost to them
        assert_eq!(m.files[0].sha256.len(), 64);
        assert!(m.options[0].description.is_some(), "a known optional key next to unknown ones");
    }

    #[test]
    fn schema_is_read_without_deserializing_the_document() {
        // an option kind we have no representation for, under a schema we DO support, is a
        // genuine parse failure — a new kind is required to bump `schema`, so this is malformed
        let same_schema_bad_kind =
            format!(r#"{{"schema": {MAX_SCHEMA}, "version": "1.0.0", "files": [],
                        "options": [{{"id":"x","kind":"sequence","label":"x","default":null}}]}}"#);
        let e = Manifest::parse(same_schema_bad_kind.as_bytes()).unwrap_err();
        assert!(!refused_for_schema(&e), "malformed, not a schema refusal");

        // the SAME unknown kind one schema higher is a clean schema refusal: the version is read
        // first, so the document is never deserialized at all
        let future_bad_kind = same_schema_bad_kind.replace(
            &format!(r#""schema": {MAX_SCHEMA}"#),
            &format!(r#""schema": {}"#, MAX_SCHEMA + 1),
        );
        assert!(refused_for_schema(&Manifest::parse(future_bad_kind.as_bytes()).unwrap_err()));
    }

    #[test]
    fn a_malformed_schema_value_is_not_silently_legacy() {
        // reading "2.0"/"two"/-1 as 1 would install a format that was never checked
        for bad in [r#""2""#, "2.5", "-1", "true"] {
            let src = format!(r#"{{"schema": {bad}, "version": "1.0.0", "files": []}}"#);
            let e = Manifest::parse(src.as_bytes()).unwrap_err();
            assert!(format!("{e:#}").contains("whole number"), "schema {bad} -> {e:#}");
        }
        // an explicit null, however, is simply absent
        let null = Manifest::parse(br#"{"schema": null, "version": "1.0.0", "files": []}"#).unwrap();
        assert_eq!(null.schema, LEGACY_SCHEMA);
    }

    #[test]
    fn dests_that_escape_the_game_root_are_refused() {
        for bad in [
            "../outside.dll",
            "game/../../Windows/System32/evil.dll",
            r"game\..\..\Windows\System32\evil.dll", // backslashes traverse on Windows too
            "/etc/passwd",
            r"\Windows\evil.dll",
            "C:/Windows/System32/evil.dll",
            r"C:\Windows\evil.dll",
            "game/file.txt:stream",
            "game//file.txt",
            "game/NUL",
            "game/con.txt",     // devices resolve with an extension too
            "game/COM1/x.txt",  // and as a directory component
            "",
        ] {
            let src = format!(
                r#"{{"schema":2,"version":"1.0.0","files":[
                     {{"name":"a","dest":{},"sha256":"aa","size":1}}]}}"#,
                serde_json::to_string(bad).unwrap()
            );
            assert!(Manifest::parse(src.as_bytes()).is_err(), "dest {bad:?} must be refused");
        }
        // near-misses of the reserved-name rule stay allowed — and the 4-byte Cyrillic stem
        // ("яя" = 4 bytes) proves the check can't panic on a multibyte boundary
        for ok in ["game/CONFIG.txt", "game/null.txt", "game/COM.txt", "game/COMX.txt", "game/яя.txt"] {
            let src = format!(
                r#"{{"schema":2,"version":"1.0.0","files":[
                     {{"name":"a","dest":"{ok}","sha256":"aa","size":1}}]}}"#
            );
            assert!(Manifest::parse(src.as_bytes()).is_ok(), "dest {ok:?} must be allowed");
        }
        // and the legitimate shapes still pass, including a `remove` and an option dest
        let good = br#"{"schema":2,"version":"1.0.0",
            "files":[{"name":"a","dest":"game/bin/win64/winmm.dll","sha256":"aa","size":1}],
            "remove":[{"dest":"game/dota/old.txt"}],
            "options":[{"id":"o","kind":"choice","label":"L","default":"v",
                        "dest":"game/dota_phoenix/maps/dota.vpk",
                        "variants":[{"id":"v","label":"V","name":"n","sha256":"bb","size":2}]}]}"#;
        assert!(Manifest::parse(good).is_ok());
    }

    /// The traversal check must cover EVERY dest in the document, not just `files[]`.
    #[test]
    fn every_dest_in_the_document_is_checked() {
        let cases = [
            r#""remove":[{"dest":"../evil"}]"#,
            r#""options":[{"id":"o","kind":"choice","label":"L","default":"v","dest":"../evil",
                           "variants":[{"id":"v","label":"V","name":"n","sha256":"bb","size":2}]}]"#,
            r#""options":[{"id":"o","kind":"toggle","label":"L","default":true,
                           "files":[{"name":"n","dest":"../evil","sha256":"bb","size":2}]}]"#,
        ];
        for tail in cases {
            let src = format!(r#"{{"schema":2,"version":"1.0.0","files":[],{tail}}}"#);
            assert!(Manifest::parse(src.as_bytes()).is_err(), "unchecked dest in: {tail}");
        }
    }
}
