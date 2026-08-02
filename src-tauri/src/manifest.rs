//! The release manifest, produced by the dist repo's tools/gen_manifest.py.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Markdown release notes ("What's new"), embedded by gen_manifest.py --notes-file. Optional so
    /// manifests written before this field are still accepted.
    #[serde(default)]
    pub notes: Option<String>,
    /// Oldest launcher version allowed to install this release (semver string, e.g. "1.2.0").
    /// Set it when a manifest change is NOT backward-compatible — older launchers then refuse
    /// with a clear "update the launcher" error instead of silently misinstalling.
    #[serde(default)]
    pub min_launcher: Option<String>,
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub remove: Vec<RemoveEntry>,
    /// User-selectable content: `choice` (one variant of a single dest) and `toggle` (an optional
    /// file set). Absent in older manifests.
    #[serde(default)]
    pub options: Vec<OptionEntry>,
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
