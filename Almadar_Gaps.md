<!-- Gap ledger for this repo: the source of truth for its open gaps. -->
# orbital-ffi — open gaps

Every open gap this repo owns lives here. This file is the source of truth; the monorepo's `docs/Almadar_Gaps.md` only rolls it up.

- **One entry per gap:** `- **<code>** — <what is wrong and where>. <owning package> [mechanical|architectural] — <evidence, prevention rung>`.
- **Codes:** new gaps use this repo's prefix `G-ORBITAL-FFI-`. Take the "Next code" below, then bump it in the same edit. Codes are never reused or renamed.
- **Close by deleting.** Remove the entry in the same commit as the fix.

Next code: `G-ORBITAL-FFI-002`

- **G-ORBITAL-FFI-001** — The embed fixtures exist twice with no guard keeping them equal: orbital-ffi `tests/fixtures/{resolved,embed}` (the C ABI golden's source, kept for the standalone CI) and orbital-core `tests/fixtures/embed` (read by the wasm twin). On 2026-10-25 three ffi `resolved/` programs still carried the retired `local: true` and lacked the P-009 `dispatch: client`, so a C ABI golden regenerated from them disagreed with the wasm twin on `entity-rerender`'s dispatch mode. Resynced from core. orbital-ffi + orbital-core fixtures [mechanical] — a monorepo-side test asserting the copies are byte-equal, or one fixture set both read from an installed package. Rung: (3) the twin tests. Root cause: H3.
