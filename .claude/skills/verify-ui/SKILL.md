---
name: verify-ui
description: Render the changed FastProcMan Slint pages with canned data on a private Xvfb and read the screenshots before calling a UI change done. Use after any change to crates/app/ui or the Rust that feeds it.
---

# Verify a FastProcMan UI change

Do not commit, push or create branches. Never claim a UI change works from a passing build alone.

1. **Checks:** `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test && cargo audit`. Fix any failure first.
2. **Snapshots:** `scripts/ui-snapshot.sh <page> [page ...]` (names are the keys of `PAGES` in the script; none = all; `MODES="many"` narrows). It builds, starts a private Xvfb, opens each page with `FASTPROCMAN_FIXTURES=empty|single|many` and writes `target/ui-snapshots/<page>-<mode>.png`. The real system is never scanned or changed.
3. **Read every PNG** of the pages you touched, in all three modes, and look for: stretched rows (rows spread over the window with one item; a `VerticalLayout` inside a `ScrollView` needs `alignment: start`), misaligned badges, truncated or overflowing text at the window edge, wrong units or float noise (`3.600000`), blank lists with no "Nothing found." state, uneven vertical gaps between rows of one kind (blank bands under one-line text), counts that read wrong at 1 ("1 items"), a name repeated in every row under a header that already shows it, items with no icon (they need a placeholder so headers stay aligned), text that should be selectable as one block (read-only `TextInput`, one per block).
4. **Fix and re-run** until every image is right. A new page or list needs a `PAGES` entry and fixture data in `crates/app/src/fixtures.rs` (with an item whose count is 1 and one with no icon, if the page has counts or icons); a new `@tr` string needs `dev/gen_po.py`.
5. **Report:** what was verified in the images, what only by tests, and what the user must test by hand. Anything behind `pkexec` always goes in the manual list.
