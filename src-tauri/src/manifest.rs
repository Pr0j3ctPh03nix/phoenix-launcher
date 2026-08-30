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
///   3  adds bundles[] (many files as one solid zstd stream) and makes `name` OPTIONAL on every
///      file-bearing entry — a nameless entry's bytes come from the bundle carrying its sha256
///      (spec: docs/manifest-format-v3.md in the dist repo)
///
/// Raise `MAX_SCHEMA` in the same change that teaches the reader the new format, never before.
pub const MIN_SCHEMA: u32 = 1;
pub const MAX_SCHEMA: u32 = 3;

/// Bundle codecs this build can decode. Adding one is a `schema` bump by the spec's bump rule,
/// so the schema gate normally catches a new codec first — the explicit check (R2) is defence in
/// depth against a producer that broke that rule.
const CODECS: &[&str] = &["zstd"];

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

/// A bundle names a codec this build cannot decode, under a schema it CAN read. Same user-facing
/// answer as `UnsupportedSchema` — "update the launcher", never "your download is corrupt" — and
/// the shell maps it to the same `tooOld` wire kind; only the detection point differs (R2).
#[derive(Debug, Clone)]
pub struct UnsupportedCodec {
    pub bundle: String,
    pub codec: String,
}

impl std::fmt::Display for UnsupportedCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this release's bundle {} uses codec {:?}, which this launcher cannot decode — update the launcher",
            self.bundle, self.codec
        )
    }
}

impl std::error::Error for UnsupportedCodec {}

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
    /// Which payload this document describes: "mod" | "launcher" | "game" | "mirrors".
    ///
    /// OPTIONAL HERE, REQUIRED THERE — and the split is the point. This type is a format reader:
    /// it answers "is this a well-formed manifest", and a schema-1 document from 2024 is, without
    /// ever having heard of payload ids. Making the field mandatory would refuse every existing
    /// release (and every conformance fixture) over a key that did not exist when they were
    /// written. The requirement belongs to the TRUST boundary instead — `trust::accept`, reached
    /// only from the signed path — where it costs nothing and buys everything: a document that
    /// carries a valid signature from a pinned key was produced by the current tooling, which
    /// always states both fields, so demanding them there can only ever reject a substitution.
    /// See `engine::manifest_of`.
    #[serde(default)]
    pub payload_id: Option<String>,
    /// Monotonic per payload, and the SOLE ordering authority — `version` is a display string
    /// ("1.0.0", "1805") and is never compared. Same optionality rationale as `payload_id`.
    #[serde(default)]
    pub serial: Option<u64>,
    /// When the producer signed this document, unix seconds. ADVISORY ONLY: a clock is not an
    /// authority, and nothing in the reader may fail on it — a producer whose machine is a day
    /// off must not be able to lock every client out. Parsed so the field is DECLARED where the
    /// format is declared; deliberately unread, since `serial` answers the only question
    /// ("is this current") that a timestamp looks like it could.
    #[serde(default)]
    #[allow(dead_code)]
    pub signed_at: Option<i64>,
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
    /// Many files' bytes as one solid compressed release asset (schema 3). Absent means none —
    /// a schema-3 document with no bundles is exactly a schema-2 document (R1).
    #[serde(default)]
    pub bundles: Vec<Bundle>,
    /// The producer's display hierarchy over `files[]` — how the launcher groups the
    /// always-installed content on screen ("Phoenix Core", "Hero Demo Plus", …). PRESENTATIONAL
    /// by contract: ignoring it wholesale is conforming (a flat list is plainer, not wrong, which
    /// is why it carries no schema number), and a ref to a dest `files[]` does not carry must be
    /// SKIPPED at render — never refused. That is why `validate` deliberately does not look at it:
    /// refusing a release over presentation would turn a display slip into a client that cannot
    /// update.
    #[serde(default)]
    pub tree: Vec<TreeNode>,
    /// Paths the files view must not report as FOREIGN even though the manifest does not ship
    /// them — the game's own runtime droppings (configs it rewrites, logs, replays, crash dumps).
    /// Nothing here is ever written, deleted or verified; the list only decides what is worth
    /// showing a user who asked "what is in my game folder that isn't the game".
    ///
    /// Data-driven on purpose. Which files a build scribbles is knowledge about Dota 2, not about
    /// updaters, and baking it into the launcher would mean a release every time that changed —
    /// the same reason the file list itself lives here. See `install::ignores_extra` for the three
    /// match rules (exact / `dir/` prefix / `*.ext` suffix); absent means "quiet nothing", which
    /// only ever costs noise in a view that is off by default.
    #[serde(default)]
    pub ignore: Vec<String>,
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

    /// Check every install destination in the document before anything can act on one, then the
    /// option shapes and the bundle invariants — all at parse time (R9): a lazy check would report
    /// a broken release partway through a multi-gigabyte install, and could not be
    /// conformance-tested at all.
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
        self.validate_hashes()?;
        self.validate_dests()?;
        self.validate_options()?;
        self.validate_bundles()
    }

    /// Every option must resolve to something. An option `resolve` would SKIP is a broken release.
    ///
    /// `resolve` materializes a choice from the effective selection — the user's, or the
    /// manifest's `default` when theirs names no variant (`effective_selection` already discards
    /// an unknown id, so that half is guarded). Nothing guards the default: if IT names no
    /// variant, or the option carries no `dest` to install one at, the branch falls through and
    /// the option contributes no file at all. A toggle whose `default` is not a bool reads as
    /// disabled by the same silence, taking its files with it.
    ///
    /// The symptom never mentions the option. The dest simply never enters the resolved set, so
    /// the check reports "up to date" about a release that ships one more file than is on disk —
    /// and a client that already holds the previous variant sees that dest as an orphan and
    /// DELETES it on the next apply. That is a content regression with no row to show for it,
    /// which is worse than the dest collision `validate_dests` refuses: that one at least leaves
    /// a change on screen forever.
    ///
    /// Deliberately narrow: only shapes that make the option a guaranteed no-op are refused.
    /// Fields a kind does not read (a `dest` on a toggle) are inert, and refusing the whole
    /// manifest — the launcher's only failure mode here, felt by every client at once — has to be
    /// reserved for defects that actually cost the user files.
    ///
    /// No B-number — see `validate_hashes`. Like that check and `validate_dests`, this is stricter
    /// than the dist repo's reference validator.
    fn validate_options(&self) -> Result<()> {
        for o in &self.options {
            match o.kind {
                OptionKind::Choice => {
                    let Some(dest) = &o.dest else {
                        return bail_invalid(format!(
                            "choice {} has no dest — its variants have nowhere to install",
                            o.id
                        ));
                    };
                    let Some(id) = o.default.as_str() else {
                        return bail_invalid(format!(
                            "choice {} default {} is not a variant id",
                            o.id, o.default
                        ));
                    };
                    if !o.variants.iter().any(|v| v.id == id) {
                        return bail_invalid(format!(
                            "choice {} default {id:?} names no variant, so {dest} is never written",
                            o.id
                        ));
                    }
                }
                OptionKind::Toggle => {
                    if !o.default.is_boolean() {
                        return bail_invalid(format!(
                            "toggle {} default {} is not true or false",
                            o.id, o.default
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Every content hash must be 64 LOWERCASE hex — the form `hex::encode` produces and the only
    /// form the reader compares against.
    ///
    /// Not cosmetic, and not fixable by being lenient here. The verification that matters compares
    /// a manifest hash against hex the reader computed itself (`obtain_to_cache`, `plan_one`), so a
    /// manifest written with UPPERCASE digests — what PowerShell's `Get-FileHash` emits, the
    /// likeliest way a Windows-side producer would compute these — makes every entry mismatch
    /// forever: the check view reports the whole payload as needing an update, and the install
    /// downloads correct bytes and then refuses them as "verification failed". Accepting the
    /// casing here instead would only move the lie: `build_local_check_view` compares
    /// case-insensitively, so the offline verdict would say "ok" while the online one said
    /// "update", about the same bytes. Name the defect at parse time, once.
    ///
    /// No B-number: this is a reader-side invariant the format spec leaves implicit, not one of
    /// the producer's enumerated bundle guarantees.
    fn validate_hashes(&self) -> Result<()> {
        let malformed = |h: &str| {
            h.len() != 64 || !h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        // `.get`, not `[..12]`: a malformed hash can hold multibyte characters, and slicing on a
        // non-boundary panics — the same trap check_dest and the B1 check document
        let short = |h: &str| h.get(..12).unwrap_or(h).to_string();
        for (_, sha, _) in self.payload_entries() {
            if malformed(sha) {
                bail_invalid(format!("entry sha256 {} is not 64 lowercase hex", short(sha)))?;
            }
        }
        for b in &self.bundles {
            if malformed(&b.psha256) {
                bail_invalid(format!(
                    "bundle {} psha256 {} is not 64 lowercase hex",
                    b.name,
                    short(&b.psha256)
                ))?;
            }
            for m in &b.members {
                if malformed(m) {
                    bail_invalid(format!(
                        "bundle {} member {} is not 64 lowercase hex",
                        b.name,
                        short(m)
                    ))?;
                }
            }
        }
        Ok(())
    }

    /// No two entries that can be installed AT THE SAME TIME may target one dest.
    ///
    /// Choice variants are exempt by construction — they share the option's single `dest` and
    /// `resolve` emits exactly one of them — so the set that must be unique is files[] plus each
    /// choice's dest plus every toggle's files.
    ///
    /// A collision is unrecoverable rather than untidy: `resolve` would emit the dest twice with
    /// two different hashes, `plan` would score it twice, the install would write both and leave
    /// whichever landed last, and every later check would report the loser as one file still to
    /// change — an update no amount of applying can ever clear.
    ///
    /// `remove[]` is deliberately NOT folded in: `plan` already resolves a dest that is both
    /// shipped and removed in favour of shipping it (`!managed.contains`), deterministically and
    /// in the only sensible direction.
    ///
    /// No B-number — see `validate_hashes`.
    fn validate_dests(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let installable = self.files.iter().map(|f| f.dest.as_str()).chain(
            self.options.iter().flat_map(|o| {
                o.dest.as_deref().into_iter().chain(o.files.iter().map(|f| f.dest.as_str()))
            }),
        );
        for dest in installable {
            if !seen.insert(dest) {
                bail_invalid(format!("two entries install to the same dest {dest}"))?;
            }
        }
        Ok(())
    }

    /// Every file-bearing entry in the document — files[], toggle files, and choice variants —
    /// as (asset name, content hash, size). Variants are included deliberately: a variant can be
    /// a bundle member, lives outside files[], and shares its dest with its siblings — which is
    /// exactly why bundle membership is keyed by content hash rather than by path or position.
    pub fn payload_entries(&self) -> impl Iterator<Item = (Option<&str>, &str, u64)> {
        let files = self.files.iter().map(|f| (f.name.as_deref(), f.sha256.as_str(), f.size));
        let opts = self.options.iter().flat_map(|o| {
            let vs = o.variants.iter().map(|v| (v.name.as_deref(), v.sha256.as_str(), v.size));
            let fs = o.files.iter().map(|f| (f.name.as_deref(), f.sha256.as_str(), f.size));
            vs.chain(fs)
        });
        files.chain(opts)
    }

    /// The producer guarantees B1–B8 (docs/manifest-format-v3.md) plus the codec gate (R2),
    /// validated rather than trusted: a violation is a producer defect — a broken release — and
    /// deserves to be named as one up front, not discovered as a mid-download hash mismatch.
    ///
    /// Message prefixes ("B2: …") match the reference validator (tools/validate_manifest.py) so
    /// a launcher-side refusal and a dist-side selftest describe the same defect the same way.
    ///
    /// B8 is checked the way the reference does: bundle names against each other and against
    /// entry names — NOT entry names against each other, which schema 2 never promised (two
    /// dests may legitimately share one asset).
    fn validate_bundles(&self) -> Result<()> {
        let size_of: HashMap<&str, u64> =
            self.payload_entries().map(|(_, sha, size)| (sha, size)).collect();
        let entry_names: std::collections::HashSet<&str> =
            self.payload_entries().filter_map(|(name, _, _)| name).collect();

        let mut names = std::collections::HashSet::new();
        let mut all_members: HashMap<&str, &str> = HashMap::new(); // member -> first bundle
        for b in &self.bundles {
            if !CODECS.contains(&b.codec.as_str()) {
                return Err(anyhow!(UnsupportedCodec {
                    bundle: b.name.clone(),
                    codec: b.codec.clone(),
                }));
            }
            if !names.insert(b.name.as_str()) || entry_names.contains(b.name.as_str()) {
                bail_invalid(format!("B8: duplicate asset name {}", b.name))?;
            }
            if b.members.is_empty() {
                bail_invalid(format!("B7: {} has no members", b.name))?;
            }
            let mut total: u64 = 0;
            for m in &b.members {
                let Some(&size) = size_of.get(m.as_str()) else {
                    // .get, not [..12]: a malformed hash can hold multibyte characters, and
                    // slicing on a non-boundary panics — same trap check_dest documents
                    return bail_invalid(format!(
                        "B1: {} member {} matches no entry",
                        b.name,
                        m.get(..12).unwrap_or(m)
                    ));
                };
                if size == 0 {
                    bail_invalid(format!("B6: {} carries a zero-size member", b.name))?;
                }
                if all_members.insert(m.as_str(), b.name.as_str()).is_some() {
                    bail_invalid("B5: a hash appears in members more than once".to_string())?;
                }
                total += size;
            }
            if total != b.size {
                bail_invalid(format!(
                    "B2: {} members sum to {total}, size says {}",
                    b.name, b.size
                ))?;
            }
        }

        // B3: a non-empty entry with no `name` must be claimed by exactly one bundle — nothing
        // else in the document says where its bytes would come from. ("Exactly one" is already
        // half-guaranteed by the B5 check above.)
        for (name, sha, size) in self.payload_entries() {
            if size > 0 && name.is_none() && !all_members.contains_key(sha) {
                bail_invalid(format!(
                    "B3: entry {} has no `name` and is in no bundle",
                    sha.get(..12).unwrap_or(sha)
                ))?;
            }
        }
        Ok(())
    }
}

/// A bundle-invariant violation: a supported schema whose document breaks a producer guarantee.
/// A broken release — NOT an "update the launcher" answer, and not a schema refusal.
fn bail_invalid(why: String) -> Result<()> {
    Err(anyhow!("refusing a broken release manifest — {why}"))
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
        if is_reserved_device(part) {
            return reject("Windows reserved device name");
        }
    }
    Ok(())
}

/// "NUL", "CON.txt", "com1" … — Windows resolves reserved device names (with or without an
/// extension, any case) to the device itself, not a file; writing there hangs or vanishes bytes
/// rather than creating anything.
///
/// Byte-wise on purpose: str slicing (`stem[..3]`) panics on a multibyte character boundary, and
/// the names reaching this can legitimately be Cyrillic — a manifest dest, or a folder name the
/// user typed (`install::subdir_issue`, which is why this is not private). Callers that can be
/// handed a raw component must trim trailing spaces and dots first: Win32 strips them BEFORE it
/// resolves the path, so `NUL ` is the device too.
pub fn is_reserved_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).as_bytes();
    stem.eq_ignore_ascii_case(b"CON")
        || stem.eq_ignore_ascii_case(b"PRN")
        || stem.eq_ignore_ascii_case(b"AUX")
        || stem.eq_ignore_ascii_case(b"NUL")
        || (stem.len() == 4
            && (stem[..3].eq_ignore_ascii_case(b"COM") || stem[..3].eq_ignore_ascii_case(b"LPT"))
            && stem[3].is_ascii_digit())
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// Asset name in the release. OPTIONAL since schema 3 — an entry resolves to bytes by exactly
    /// one route, checked in this order (the spec's resolution table):
    ///
    /// 1. `size == 0` — the reader materializes an empty file; no asset exists (a leftover
    ///    `name` on such an entry is historical and meaningless — GitHub refuses to host
    ///    zero-byte assets, so none was ever uploaded);
    /// 2. `name` present — that release asset, verbatim (schema-2 behaviour);
    /// 3. `name` absent — the one bundle whose `members` carries this entry's `sha256`
    ///    (existence and uniqueness guaranteed by B3/B5, validated at parse).
    #[serde(default)]
    pub name: Option<String>,
    /// Install destination, relative to the game root (the folder containing `game/`).
    pub dest: String,
    pub sha256: String,
    pub size: u64,
}

/// Many files' bytes concatenated and compressed as ONE release asset (schema 3). There is no
/// container inside the file — no tar, no member table: the manifest already states every
/// member's `sha256` and `size`, so the decoded stream is split by counting bytes against the
/// members' sizes, in `members` order (B4: nothing between members, nothing after the last).
#[derive(Debug, Clone, Deserialize)]
pub struct Bundle {
    /// Release asset name. Content-addressed by the producer (embeds a psha256 prefix), but
    /// readers use it verbatim and never parse the hash back out — `psha256` is stated below.
    pub name: String,
    /// Enumerated; only "zstd" is defined. Unknown -> `UnsupportedCodec` at parse time.
    pub codec: String,
    /// Bytes of the PACKED asset on the wire. Progress bars, ETAs and "downloaded so far" speak
    /// this number; it is not interchangeable with `size` (R7).
    pub psize: u64,
    /// sha256 of the packed asset's bytes — verified BEFORE anything is decoded (R3).
    pub psha256: String,
    /// Bytes of the DECODED stream (== the members' sizes summed, B2). Free-space / installed-
    /// footprint math speaks this number.
    pub size: u64,
    /// Content hashes of the carried entries, in stream order. Keyed by hash, not dest or
    /// position: choice variants share one dest and live outside files[], and duplicate content
    /// is carried once while landing at several dests.
    pub members: Vec<String>,
}

/// One node of the display `tree`: dest refs into `files[]` plus child groups, to any depth.
/// A node WITHOUT a `label` just splices its content into its parent — the producer emits one for
/// files declared outside any named group.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeNode {
    #[serde(default)]
    pub label: Option<Label>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub groups: Vec<TreeNode>,
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
    /// Asset name in the release; optional since schema 3 (same resolution routes as
    /// `FileEntry::name` — a nameless variant's bytes come from the bundle carrying its hash).
    #[serde(default)]
    pub name: Option<String>,
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

    /// Was this refused SPECIFICALLY for an undecodable bundle codec (R2)?
    fn refused_for_codec(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.downcast_ref::<UnsupportedCodec>().is_some())
    }

    /// A well-formed stand-in content hash: 64 lowercase hex from a repeated byte pair. Every hash
    /// in a manifest under test has to be one (`validate_hashes`), so a case that is about
    /// something ELSE — a dest, a schema, a bundle sum — says so by using this instead of a short
    /// placeholder that would now be refused before its own subject was ever reached.
    fn hash(pair: &str) -> String {
        pair.repeat(32)
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
                // supported schema, unrecognised codec: the same user-facing "update the app"
                // as refuse:schema, but detected at the bundle (typed UnsupportedCodec)
                "refuse:codec" => {
                    let Err(e) = got else {
                        panic!("{file} must be REFUSED but parsed clean\n  why: {why}");
                    };
                    assert!(
                        refused_for_codec(&e),
                        "{file} was refused, but NOT for its codec — it failed as: {e:#}\n  why: {why}"
                    );
                }
                // supported schema, broken producer guarantee (B1–B8): a broken release —
                // refused, but neither as a schema nor as a codec problem ("update the
                // launcher" would be a lie; there is no launcher that reads it)
                "refuse:invalid" => {
                    let Err(e) = got else {
                        panic!("{file} must be REFUSED but parsed clean\n  why: {why}");
                    };
                    assert!(
                        !refused_for_schema(&e) && !refused_for_codec(&e),
                        "{file} must be refused as a BROKEN RELEASE, not as a schema/codec \
                         refusal — it failed as: {e:#}\n  why: {why}"
                    );
                }
                other => panic!("{file}: unknown expectation {other:?} in index.json"),
            }
        }
    }

    /// The accepted fixtures must not merely parse — they must yield what the spec describes.
    /// "Accept" that silently drops the options list would pass the suite and install nothing.
    /// current.json exercises every schema-3 feature at once: a raw entry, a multi-member bundle,
    /// a one-member bundle, a zero-byte entry and a BUNDLED option variant.
    #[test]
    fn current_fixture_parses_into_the_documented_shape() {
        let m = Manifest::parse(&std::fs::read(fixtures().join("current.json")).unwrap()).unwrap();
        assert_eq!(m.schema, 3);
        assert_eq!(m.version, "1.0.0");
        assert!(m.notes.is_some_and(|n| n.contains("Added")));

        assert_eq!(m.bundles.len(), 2);
        let multi = &m.bundles[0];
        assert_eq!(multi.name, "b000-txt-4f3a91c2e5d8.phxb");
        assert_eq!(multi.codec, "zstd");
        assert_eq!((multi.psize, multi.size), (7633, 22899));
        assert_eq!(multi.members.len(), 4);
        assert_eq!(m.bundles[1].members.len(), 1, "a one-member bundle is legal (B7)");

        assert_eq!(m.files.len(), 5);
        assert_eq!(m.files[0].name.as_deref(), Some("pak01_000.vpk"), "a raw entry keeps its name");
        assert_eq!(m.files[1].name, None, "a bundled entry has none");
        assert!(multi.members.contains(&m.files[1].sha256), "…and its hash is in the bundle");
        let empty = &m.files[4];
        assert_eq!(empty.size, 0, "a zero-byte entry is materialized, never fetched");
        assert_eq!(m.remove.len(), 1);

        let choice = &m.options[0];
        assert_eq!(choice.kind, OptionKind::Choice);
        assert_eq!(choice.default, serde_json::json!("original"));
        assert_eq!(choice.variants[0].name, None, "the bundled variant — what forces hash keying");
        assert!(multi.members.contains(&choice.variants[0].sha256));
        assert_eq!(choice.variants[1].name.as_deref(), Some("opt__lighting__original.vpk"));
        let toggle = &m.options[1];
        assert_eq!(toggle.kind, OptionKind::Toggle);
        assert_eq!(toggle.files[0].name, None);
        assert!(multi.members.contains(&toggle.files[0].sha256));

        // the display tree: a labeled node with a nested group, and an UNLABELED node whose
        // content splices into its parent — both shapes the renderer has to know
        assert_eq!(m.tree.len(), 2);
        let core = &m.tree[0];
        assert!(matches!(&core.label, Some(Label::Localized(l)) if l["en"] == "Phoenix Core"));
        assert!(core.files.contains(&"game/dota/pak01_000.vpk".to_string()));
        assert_eq!(core.groups.len(), 1, "Hero Demo Plus nests under Phoenix Core here");
        assert!(m.tree[1].label.is_none(), "an unlabeled node is legal and splices inline");
    }

    /// Exactly what the shim producer emits today — and will keep emitting: the shim never cuts
    /// over to 3, and it is the routine update path hit on every launch. A reader that only
    /// handles bundled documents would break the repo it updates from.
    #[test]
    fn schema2_fixture_parses_into_the_shim_shape() {
        let m =
            Manifest::parse(&std::fs::read(fixtures().join("schema2-options.json")).unwrap()).unwrap();
        assert_eq!(m.schema, 2);
        assert!(m.bundles.is_empty(), "absent bundles mean none (R1)");
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.files[0].name.as_deref(), Some("winmm.dll"));
        assert_eq!(m.files[0].dest, "game/bin/win64/winmm.dll");
        assert_eq!(m.files[0].size, 1560);
        assert_eq!(
            m.files[1].dest,
            "game/dota_addons_phoenix/hero_demo/scripts/vscripts/events.lua",
            "an always-installed file that the tree files under a heading"
        );
        assert_eq!(m.remove.len(), 1);
        assert_eq!(m.remove[0].dest, "game/dota/scripts/regions.txt");
        assert_eq!(m.tree.len(), 1);
        assert!(m.tree[0].groups[0].files.contains(&m.files[1].dest));

        assert_eq!(m.options.len(), 2);
        let choice = &m.options[0];
        assert_eq!(choice.id, "lighting");
        assert_eq!(choice.kind, OptionKind::Choice);
        assert_eq!(choice.dest.as_deref(), Some("game/dota_phoenix/maps/dota.vpk"));
        assert_eq!(choice.default, serde_json::json!("original")); // variant id for a choice
        assert_eq!(choice.variants.len(), 2);
        assert_eq!(choice.variants[0].name.as_deref(), Some("opt__lighting__mod.vpk"));
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
    /// The fixture plants them at the top level, inside a file entry, inside a BUNDLE and inside
    /// an option.
    #[test]
    fn additive_unknown_keys_are_ignored() {
        let m = Manifest::parse(&std::fs::read(fixtures().join("additive-unknown-keys.json")).unwrap())
            .unwrap();
        assert_eq!(m.files.len(), 5);
        assert_eq!(m.options.len(), 2);
        assert_eq!(m.bundles.len(), 2);
        // the unknown top-level/entry/bundle keys sit beside real ones; nothing may be lost
        assert_eq!(m.files[0].sha256.len(), 64);
        assert_eq!(m.bundles[0].members.len(), 4);
        assert!(m.options[0].description.is_some(), "a known optional key next to unknown ones");
    }

    /// The signing fields are additive in both directions: a document that carries them keeps
    /// them, and one written before they existed still parses. Whether a document is ALLOWED to
    /// omit them is not a format question — it is `trust::accept`'s, on the signed path only, and
    /// that split is what keeps every fixture here valid.
    #[test]
    fn the_signing_fields_are_read_and_never_required() {
        let signed = Manifest::parse(
            br#"{"schema":2,"payload_id":"mod","serial":42,"signed_at":1756500000,
                 "version":"1.0.0","files":[]}"#,
        )
        .unwrap();
        assert_eq!(signed.payload_id.as_deref(), Some("mod"));
        assert_eq!(signed.serial, Some(42));
        assert_eq!(signed.signed_at, Some(1_756_500_000));

        // every fixture in the suite predates all three, and must keep parsing
        let legacy = Manifest::parse(br#"{"schema":2,"version":"1.0.0","files":[]}"#).unwrap();
        assert_eq!((legacy.payload_id, legacy.serial, legacy.signed_at), (None, None, None));
        // MAX_SCHEMA does NOT move for an additive key — that is the whole compatibility rule
        assert_eq!(MAX_SCHEMA, 3);
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
                     {{"name":"a","dest":{},"sha256":"{}","size":1}}]}}"#,
                serde_json::to_string(bad).unwrap(),
                hash("aa")
            );
            assert!(Manifest::parse(src.as_bytes()).is_err(), "dest {bad:?} must be refused");
        }
        // near-misses of the reserved-name rule stay allowed — and the 4-byte Cyrillic stem
        // ("яя" = 4 bytes) proves the check can't panic on a multibyte boundary
        for ok in ["game/CONFIG.txt", "game/null.txt", "game/COM.txt", "game/COMX.txt", "game/яя.txt"] {
            let src = format!(
                r#"{{"schema":2,"version":"1.0.0","files":[
                     {{"name":"a","dest":"{ok}","sha256":"{}","size":1}}]}}"#,
                hash("aa")
            );
            assert!(Manifest::parse(src.as_bytes()).is_ok(), "dest {ok:?} must be allowed");
        }
        // and the legitimate shapes still pass, including a `remove` and an option dest
        let good = format!(
            r#"{{"schema":2,"version":"1.0.0",
            "files":[{{"name":"a","dest":"game/bin/win64/winmm.dll","sha256":"{}","size":1}}],
            "remove":[{{"dest":"game/dota/old.txt"}}],
            "options":[{{"id":"o","kind":"choice","label":"L","default":"v",
                        "dest":"game/dota_phoenix/maps/dota.vpk",
                        "variants":[{{"id":"v","label":"V","name":"n","sha256":"{}","size":2}}]}}]}}"#,
            hash("aa"),
            hash("bb")
        );
        assert!(Manifest::parse(good.as_bytes()).is_ok());
    }

    /// Every message that names a hash truncates it for display; a broken manifest can put
    /// multibyte garbage where a hash belongs, and a byte-index slice there panics mid-character.
    /// Must be a clean refusal — a hostile document crashing the parser is strictly worse than the
    /// arbitrary-write it failed to achieve. ("aяяяяяя": byte 12 lands mid-я.)
    ///
    /// The hash-format check sees such a value first, so it is the one that has to survive it —
    /// in an entry, in `members`, and in a bundle's `psha256`.
    #[test]
    fn multibyte_garbage_in_hashes_is_refused_without_panicking() {
        let good = hash("aa");
        let cases = [
            format!(
                r#"{{"schema":3,"version":"1","files":[],
                     "bundles":[{{"name":"b","codec":"zstd","psize":1,"psha256":"{good}","size":1,
                                  "members":["aяяяяяя"]}}]}}"#
            ),
            format!(
                r#"{{"schema":3,"version":"1","files":[],
                     "bundles":[{{"name":"b","codec":"zstd","psize":1,"psha256":"aяяяяяя","size":1,
                                  "members":["{good}"]}}]}}"#
            ),
            r#"{"schema":3,"version":"1",
                "files":[{"dest":"game/x","sha256":"aяяяяяя","size":4}]}"#
                .to_string(),
        ];
        for src in cases {
            let e = Manifest::parse(src.as_bytes()).unwrap_err();
            assert!(format!("{e:#}").contains("64 lowercase hex"), "got: {e:#}");
        }
    }

    /// A WELL-FORMED hash that resolves to nothing is a different defect from a malformed one, and
    /// keeps its own (B-numbered) message — the hash-format check must not swallow B1/B3.
    #[test]
    fn well_formed_but_unresolvable_hashes_still_fail_as_b1_and_b3() {
        let orphan = format!(
            r#"{{"schema":3,"version":"1","files":[],
                 "bundles":[{{"name":"b","codec":"zstd","psize":1,"psha256":"{}","size":1,
                              "members":["{}"]}}]}}"#,
            hash("aa"),
            hash("cc")
        );
        let e = Manifest::parse(orphan.as_bytes()).unwrap_err();
        assert!(format!("{e:#}").contains("B1"), "got: {e:#}");

        // no `name`, in no bundle: nothing in the document says where its bytes come from
        let unbundled = format!(
            r#"{{"schema":3,"version":"1",
                 "files":[{{"dest":"game/x","sha256":"{}","size":4}}]}}"#,
            hash("cc")
        );
        let e = Manifest::parse(unbundled.as_bytes()).unwrap_err();
        assert!(format!("{e:#}").contains("B3"), "got: {e:#}");
    }

    /// An UPPERCASE digest is the realistic version of this defect: PowerShell's `Get-FileHash`
    /// emits one, and every comparison the reader makes is against lowercase hex it computed
    /// itself — so accepting it would mean a payload that is permanently "to update" and an
    /// install that downloads correct bytes and then calls them corrupt.
    #[test]
    fn uppercase_and_short_hashes_are_refused_as_a_broken_release() {
        for bad in ["AA".repeat(32), hash("aa")[..63].to_string(), format!("{}z", &hash("aa")[..63])] {
            let src = format!(
                r#"{{"schema":2,"version":"1.0.0",
                     "files":[{{"name":"a","dest":"game/x","sha256":"{bad}","size":1}}]}}"#
            );
            let e = Manifest::parse(src.as_bytes()).unwrap_err();
            assert!(format!("{e:#}").contains("64 lowercase hex"), "{bad} -> {e:#}");
            assert!(!refused_for_schema(&e) && !refused_for_codec(&e), "a broken release, not a version gap");
        }
    }

    /// Two entries that would be installed at once may not share a dest: `resolve` emits both,
    /// the install writes both, one survives, and every later check reports the other as a change
    /// that applying can never clear. Choice VARIANTS sharing the option's one dest is the legal
    /// case and must keep parsing.
    #[test]
    fn two_entries_installing_to_one_dest_are_refused() {
        let (a, b) = (hash("aa"), hash("bb"));
        let collisions = [
            // files[] against itself
            format!(
                r#""files":[{{"name":"a","dest":"game/dota/x.vpk","sha256":"{a}","size":1}},
                            {{"name":"b","dest":"game/dota/x.vpk","sha256":"{b}","size":2}}]"#
            ),
            // a toggle's file against a core file
            format!(
                r#""files":[{{"name":"a","dest":"game/dota/x.vpk","sha256":"{a}","size":1}}],
                   "options":[{{"id":"t","kind":"toggle","label":"L","default":false,
                                "files":[{{"name":"b","dest":"game/dota/x.vpk","sha256":"{b}","size":2}}]}}]"#
            ),
            // a choice's dest against a core file
            format!(
                r#""files":[{{"name":"a","dest":"game/dota/x.vpk","sha256":"{a}","size":1}}],
                   "options":[{{"id":"c","kind":"choice","label":"L","default":"v",
                                "dest":"game/dota/x.vpk",
                                "variants":[{{"id":"v","label":"V","name":"b","sha256":"{b}","size":2}}]}}]"#
            ),
        ];
        for tail in collisions {
            let src = format!(r#"{{"schema":2,"version":"1.0.0",{tail}}}"#);
            let e = Manifest::parse(src.as_bytes()).unwrap_err();
            assert!(format!("{e:#}").contains("same dest"), "got: {e:#}");
        }

        // the legal shape: two variants of ONE choice, sharing the option's dest
        let legal = format!(
            r#"{{"schema":2,"version":"1.0.0","files":[],
                 "options":[{{"id":"c","kind":"choice","label":"L","default":"v1",
                              "dest":"game/dota/x.vpk",
                              "variants":[{{"id":"v1","label":"A","name":"a","sha256":"{a}","size":1}},
                                          {{"id":"v2","label":"B","name":"b","sha256":"{b}","size":2}}]}}]}}"#
        );
        assert!(Manifest::parse(legal.as_bytes()).is_ok());
    }

    /// An option `resolve` would silently skip is refused up front. Left to run, it contributes no
    /// file: the check then calls a release "up to date" while one of its dests is unwritten, and
    /// a client holding the previous variant deletes that dest as an orphan on the next apply.
    #[test]
    fn an_option_the_reader_would_skip_is_refused() {
        let (a, b) = (hash("aa"), hash("bb"));
        let variants = format!(
            r#"[{{"id":"v1","label":"A","name":"a","sha256":"{a}","size":1}},
                {{"id":"v2","label":"B","name":"b","sha256":"{b}","size":2}}]"#
        );
        let broken = [
            // a default naming no variant — `find` misses and the dest is never written
            format!(
                r#"{{"id":"c","kind":"choice","label":"L","default":"v3",
                     "dest":"game/dota/x.vpk","variants":{variants}}}"#
            ),
            // ...including when it is not even a variant id in shape
            format!(
                r#"{{"id":"c","kind":"choice","label":"L","default":true,
                     "dest":"game/dota/x.vpk","variants":{variants}}}"#
            ),
            // no dest: nothing in the document says where the chosen variant would land
            format!(
                r#"{{"id":"c","kind":"choice","label":"L","default":"v1","variants":{variants}}}"#
            ),
            // a toggle whose default is not a bool reads as OFF, and its files vanish with it
            format!(
                r#"{{"id":"t","kind":"toggle","label":"L","default":"yes",
                     "files":[{{"name":"a","dest":"game/dota/fx.vpk","sha256":"{a}","size":1}}]}}"#
            ),
        ];
        for opt in broken {
            let src = format!(r#"{{"schema":2,"version":"1.0.0","files":[],"options":[{opt}]}}"#);
            let e = Manifest::parse(src.as_bytes()).unwrap_err();
            assert!(format!("{e:#}").contains("broken release"), "got: {e:#}");
            assert!(!refused_for_schema(&e), "a broken release, not a version gap");
        }

        // both kinds in their legal shape still parse
        let legal = format!(
            r#"{{"schema":2,"version":"1.0.0","files":[],"options":[
                 {{"id":"c","kind":"choice","label":"L","default":"v2",
                   "dest":"game/dota/x.vpk","variants":{variants}}},
                 {{"id":"t","kind":"toggle","label":"L","default":false,
                   "files":[{{"name":"a","dest":"game/dota/fx.vpk","sha256":"{a}","size":1}}]}}]}}"#
        );
        assert!(Manifest::parse(legal.as_bytes()).is_ok());
    }

    /// R8: arriving via a bundle relaxes nothing about `dest` — a nameless entry is checked
    /// exactly like a named one, and refused as a traversal, not as some bundle defect.
    #[test]
    fn bundled_entries_get_the_same_dest_validation() {
        let sha = "aa".repeat(32);
        let src = format!(
            r#"{{"schema":3,"version":"1.0.0",
                 "files":[{{"dest":"game/../../evil.dll","sha256":"{sha}","size":4}}],
                 "bundles":[{{"name":"b.phxb","codec":"zstd","psize":2,"psha256":"bb",
                              "size":4,"members":["{sha}"]}}]}}"#
        );
        let e = Manifest::parse(src.as_bytes()).unwrap_err();
        assert!(format!("{e:#}").contains("refusing manifest dest"), "failed as: {e:#}");
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
