# Plan: `.phxb` bundle format for game-dist (manifest schema 3)

Status: **proposed, not implemented** (designed 2026-08-07). When implementation starts, the
format spec's authoritative home is `docs/manifest-format.md` in the DIST repo (with conformance
fixtures vendored back here, as always); this file is the plan and the rationale.

## Problem

game-dist serves the base game as 4,635 raw per-file release assets (14.75 GB). Consequences,
worst first:

- **Request count**: a fresh install is ~4,600 HTTP requests. Failure odds compound (0.01%
  per-request failure ⇒ ~37% of runs hit ≥1 failure — observed in the wild as an HTTP 500 killing
  a run); the small-file tail is latency-bound (measured: 1,290 files = 0.37 GB), costing minutes
  of pure round trips on high-RTT links; ~9,200 AV scan events per install; thousands of rapid
  requests look like scraping to middleboxes and rate limiters; N installs ⇒ N×4,600 download
  events visible to GitHub.
- **No compression**: the cfg/lua/xml tail compresses 3–6×; VPKs maybe 1.1–1.3×.
- **Producer fragility**: the whole shard apparatus (~25 prereleases, ≤190 assets each, the
  probed-not-documented embedded-array bound) exists only because of the per-file choice.

Launcher-side mitigations already shipped (8 workers, pooled connections, largest-first,
transient retry) treat symptoms; the request count is the disease.

## Constraints

- GitHub Releases: ≤2 GiB per asset (single mega-archive impossible), public repo /
  `browser_download_url` / free CDN / Range supported.
- Must preserve: per-file verify + repair with BOUNDED cost, byte-level resume, the
  content-addressed-by-uncompressed-sha256 cache, per-chunk cancellation, honest progress/ETA,
  dumb data-driven reader.
- The game is FROZEN (1805): one-shot immutable distribution — delta-update efficiency is
  explicitly a non-goal.

## Format

**Central insight: the manifest already IS the index.** Every container format (tar/zip) exists
to carry member names/sizes/offsets — the manifest already carries dest, size, sha256 per file.
So a bundle has NO container:

    bundle = zstd( member_0 || member_1 || ... || member_n )    — one solid stream

Boundaries derive from the manifest: **a bundle's members appear in `files[]` in their exact
stream order**, and cumulative `size` yields every offset. Reader stream-decodes, counts bytes,
splits, hashes. Producer concatenates and compresses. No second source of truth, no per-member
header overhead, no tar determinism tax (mtimes/uids), no container dependency on either side.
(Dev inspection = `zstd -d` + a 10-line split-at-manifest-sizes script.)

## Manifest (schema 3)

```json
{ "schema": 3, "version": "1805",
  "bundles": [
    { "name": "b03-scripts.phxb", "codec": "zstd",
      "size": 412094464,  "sha256": "…",          // uncompressed stream (sanity + disk math)
      "psize": 96420117,  "psha256": "…" }        // the asset on the wire
  ],
  "files": [
    { "dest": "game/dota/scripts/npc/npc_units.txt", "sha256": "…", "size": 431104,
      "bundle": "b03-scripts.phxb" },
    { "name": "pak01_000.vpk.zst", "dest": "game/dota/pak01_000.vpk", "sha256": "…",
      "size": 1073741824, "pack": { "psize": 830000000, "psha256": "…" } },
    { "name": "logo.png", "dest": "game/dota/logo.png", "sha256": "…", "size": 51234 }
  ] }
```

Three file shapes, chosen by the PRODUCER per file (reader stays a dumb follower):

| shape | when | wire |
|---|---|---|
| `bundle` | small tail (< ~8 MB) | inside a solid bundle |
| `pack` | large files (VPKs) | standalone asset, whole-file zstd (a lone file is its own solid stream; keeps individual `.part` resume) |
| neither | ratio-poor files (producer measures; > ~0.97 ships raw) | identical to schema-2 — zero new reader code for this arm |

Zero-byte files keep today's rule: materialized by the reader, never on the wire, never bundled.
Reader validation: unknown `codec` → typed unsupported error; per bundle, Σ member `size` must
equal `bundle.size`; `check_dest` applies to bundled dests identically.

## Bundle sizing — two hard numbers

- zstd long-mode window is 128 MB (`--long=27`): solid ratio stops improving past it — bigger
  bundles buy nothing.
- Solid streams have no random access: repair cost = one whole bundle.

Both point to **~64–128 MB compressed** (~256–512 MB uncompressed at the tail's 3–5×), members
**sorted by extension-class then path** so similar content shares the window. ⇒ ~5–8 tail
bundles; a one-file repair costs ~100 MB, bounded by design.

## Reader flow (launcher, schema 3, `MAX_SCHEMA = 3`)

Plan/verify verdicts unchanged (per-dest, uncompressed sha). Obtain partitions the Write set:

- raw assets — exactly today's path;
- `pack` standalone — download `.part` → verify `psha256` → stream-decode → verify `sha256` →
  cache;
- bundle members — group needed dests by bundle; per needed bundle: download `.part` (Range
  resume + prefix re-hash vs `psha256`, transient retry loop applies unchanged) → verify
  `psha256` → single stream-decode pass splitting at manifest sizes, hashing every member,
  writing NEEDED members into the content-addressed cache (decode passes through unneeded
  members — zstd decodes >1 GB/s) → delete the bundle file.

Torn extraction is safe BY CONSTRUCTION: a member is committed to the cache only after its own
sha verifies, so a corrupt bundle tail loses nothing already extracted. A member sha mismatch
after a clean `psha256` = producer bug → loud typed failure, never a retry (source fact).
No decode-while-downloading in v1: saves ~1 s, complicates resume.

Progress/ETA/disk preflight switch to PACKED bytes for the wire math (bar and ETA stay honest);
uncompressed sizes drive the disk-space check (transient = bundle + extracted members).
Cache/`base_cached`/`pending_base_bytes`/warm logic: untouched.

## Producer (`gen_game_manifest.py` in the dist repo)

Classify (0-byte / large-or-ratio-poor / tail) → sort tail by (ext-class, path) → greedy-fill
bundles to the uncompressed target → `zstd -19 --long=27 -T0` (~minutes in CI for 15 GB) →
emit manifest with members in stream order → upload ~30–40 assets in ONE release. The shard
machinery (prereleases, ≤190/release, `merged_game_release`) retires for new releases; the
reader keeps it for old ones.

## Rollout (the schema gate was built for this)

1. Spec + conformance fixtures in the dist repo's `docs/manifest-format.md`; vendor fixtures here.
2. Launcher learns schema 3 (zstd decode — `zstd` crate ~0.5 MB, bundle/pack obtain arms,
   grouped repair, packed-byte progress). Release; let self-update propagate.
3. Cut the schema-3 game-dist release under a new tag. Old launchers get the typed
   "update the launcher" (`tooOld`); already-installed users are unaffected.

## Expected result

~4,600 requests → **~30–40**; ~14.75 GB → **~11–12 GB**; the latency-bound small-file phase
gone; middlebox/rate-limit/AV exposure cut ~100×; sharding deleted from the producer.

## Deliberately excluded (and why)

- **tar/zip container** — second source of truth + determinism tax, zero benefit given the
  manifest-as-index.
- **Per-member zstd frames with Range repair** — repair granularity bought at the worst price:
  tiny files compressed in isolation forfeit the solid ratio, which is the point of bundling.
- **Seekable-zstd frame table** — near-solid ratio AND Range-level member repair; genuinely good
  but ADDITIVE (an extra table field; old bundles stay valid), so it must not cost complexity
  now. The upgrade path if single-file repair traffic ever matters.
- **One chunked mega-archive** — 2 GiB asset cap forces blind slices; destroys repair.
- **Delta/chunk formats (casync/zsync), torrents** — solve update churn and hosting, neither of
  which is this problem (frozen game, GitHub CDN is the distribution decision).
