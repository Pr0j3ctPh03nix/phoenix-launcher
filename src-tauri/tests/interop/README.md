# Cross-implementation signature fixtures

A real release manifest signed by the **Python** producer
(`client-dist-staging/tools/phoenix_minisign.py`), kept here so `trust.rs` can prove it reads what
that producer actually writes.

Two independent implementations of one wire format disagree silently: each one's own tests pass,
because each verifies what it produced. The disagreement only surfaces when a release is already
tagged and no client can install it. This is the fixture that makes it surface at `cargo test`.

- `manifest.json` — a real schema-3 bundled mod manifest (from `gen_manifest.py`, not hand-written)
- `manifest.json.minisig` — its signature, produced by `phoenix_minisign.py sign`
- `test.pub` — the matching public key

Not a private key, and not a shipped key: the pair was generated for this fixture and discarded.
The test supplies `test.pub` explicitly rather than going through `PINNED`, so these bytes carry no
authority in a real build.

To regenerate after a deliberate format change:

```sh
cd <client-dist-staging worktree>
python tools/phoenix_minisign.py keygen --sec /tmp/t.key --pub src-tauri/tests/interop/test.pub
cp <a real manifest.json> src-tauri/tests/interop/manifest.json
python tools/phoenix_minisign.py sign --sec /tmp/t.key src-tauri/tests/interop/manifest.json
```
