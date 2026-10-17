# 004 — History depth

**Goal:** a default herdr scrollback limit that holds ≈10k rows at a typical
wide pane, bounded at 20 MB.

**Prerequisites:** 001 (rows-per-MB math).

## Implementation

- `env.rs`: set `DEFAULT_HERDR_SCROLLBACK_LIMIT_BYTES` from the spike's table
  (smallest round MB value reaching ≈10k rows at ~200 columns, capped at 20 MB).
- Update its doc comment: what the number buys, that the limit is ghostty page
  memory (not text bytes), and that it applies to panes created after the herdr
  server restarts.
- Update the tests that hard-code `4000000` to use the constant or the new value.

## Validation

`cargo test -p keitora` (env tests).

**Commit:** `feat(herdr): raise default pane scrollback to about 10k lines`
