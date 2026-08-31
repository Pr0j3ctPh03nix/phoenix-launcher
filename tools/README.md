# Vendored release tooling — do not edit

These files are **mirrored from the dev superset** (`client-dist-staging`) by its `tools/sync.py`.
Edit them there and re-sync; an edit made here is reverted by the next sync, and
`python tools/sync.py --check` in the superset — the pre-tag gate — fails until it is.

| file | what it is |
|---|---|
| `phoenix_minisign.py` | the signature format, both directions. The shared contract with `src-tauri/src/trust.rs`, which is an independent implementation of the same four lines |
| `validate_manifest.py` | the reference reader-side validator: schema gate, signing envelope, `dest` traversal, codec, bundle invariants |
| `phx_release.py` | the seal step every payload goes through — validate, sign, prove — in that order |
| `gen_launcher_manifest.py` | produces this payload's manifest: the exe's sha256 as a signed `payload_id: "launcher"` document |
| `phoenix-release.pub` | the ACTIVE public half, used to prove a signature this workflow just made. Its private half is `secrets.PHOENIX_SIGNING_KEY` |

## Why vendored rather than fetched

The launcher signs its own payload, so its CI needs the same tooling the mod and the game are
sealed with. Fetching it at build time was the alternative and was rejected: a release of the
**reader** must not depend on the **producer's** repo being reachable, on a private repo, or on a
token issued for a different job. The cost of a copy is drift, and `sync.py --check` is the guard —
it compares these bytes against the superset's originals and refuses to pass if they differ.

## Why they are not the launcher's own implementation

`trust.rs` verifies signatures and is written independently of these on purpose — two
implementations of one wire format, so a disagreement surfaces as a test failure rather than as a
release nobody can install (`src-tauri/tests/interop/` pins that). These are the **producer** side
of the same format, and there is exactly one producer implementation for all three payloads.
