# AGENTS.md: FastProcMan

Notes for AI agents and contributors. Read this before changing anything.

## What this is

A **light and fast** Linux process manager in Rust + Slint: sort/filter processes by CPU and memory, flag suspicious ones, end or kill one quickly, and graph CPU (and each core), memory (and swap), network, GPU and each disk. Priorities: fast start, low memory and CPU, simplicity. Measured on a real desktop (490 processes, 2 s refresh): ~24 MB RSS, ~1.2% of one core. The default refresh is now 1 s, which should be about twice the CPU: not re-measured yet. Re-measured with per-core/per-disk/GPU history (`--profile fast`, Graphs tab, 1 s, NVIDIA desktop, 60 s): 29.7 MB `VmRSS` (9.4 MB `RssAnon`), 2.7% of one core **without** NVML; 50.4 MB (25.9 MB `RssAnon`), 3.2% **with** it. So opening NVML costs about 21 MB of RSS and 0.4% CPU (it loads `libcuda` and others); the rest of the page is cheap.

## Layout notes

`crates/core` is UI-independent (no Slint) so a CLI can reuse it; `crates/app` is the Slint UI. `flatpak/build.sh` builds outside the repo (`~/.cache/fastprocman-flatpak`). `scripts/ui-snapshot.sh` renders pages on a private Xvfb with canned data (`crates/app/src/fixtures.rs`, env `FASTPROCMAN_FIXTURES=empty|single|many`, `FASTPROCMAN_PAGE=<n>|about|confirm|confirm-kill|properties|menu|settings|tree`) to `target/ui-snapshots/`. `dev/gen_po.py` writes the `.po` files from one table. `crates/app/src/instance.rs` makes it one window: a second start sends `show` over `$XDG_RUNTIME_DIR/fastprocman.sock` and exits, and the running window is raised. It is a verbatim copy of FastHello's (change it there, not here). The check comes before `host::run()`, and `host.rs` forwards `FLATPAK_ID` to the host copy so both use the socket in `$XDG_RUNTIME_DIR/app/<id>/`. Keep `README.md` (short, for users), `docs/FEATURES.md` (every behaviour in detail) and `BUILD.md` in step with behaviour. The screenshots in `docs/screenshots/` are made with the snapshot script on the `many` fixture and cropped to the 860x560 window: `main.png` (processes, with the amber banner) as is, `tree`, `services` and `graphs` with `FASTPROCMAN_CALM=1` (no banner), e.g. `FASTPROCMAN_CALM=1 MODES=many scripts/ui-snapshot.sh tree` then `convert target/ui-snapshots/tree-many.png -crop 860x560+0+0 +repage docs/screenshots/tree.png`. Redo them after a visible UI change.

System deps: `libfontconfig-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-dev libxcb1-dev`. No GL/Skia/femtovg.

## Code conventions: Editing code

- Use the Edit tool for code changes. Don't splice files with sed, awk, python or heredocs through Bash.
- After editing Rust, run `cargo fmt && cargo clippy -- -D warnings && cargo test`. Re-read the surrounding block to confirm no braces, lines or whole components (e.g. pages) were deleted.
- After changing dependencies (`Cargo.toml` or `Cargo.lock`), also run `cargo audit`: CI runs it and fails on known vulnerabilities (warnings such as "unmaintained" do not fail). Fix with `cargo update -p <crate>`.
- After any change, also run `cargo build --profile fast` so the binary is built and cached for the owner's manual testing.

## Architecture decisions (inherited from FastMoji, measured there)

- **Software renderer only** (`winit` + `software`, forced in `main.rs` and in `Cargo.toml` features with `default-features = false`): ~27 MB RSS versus ~108 MB with the GL renderer.
- No async runtime. Scans/work in threads, results handed to the UI thread with `slint::invoke_from_event_loop`.
- Never hold a `RefCell` borrow while calling something that makes Slint re-read a model.
- Keep dependencies few and light.
- Slint notes: a `VerticalLayout` inside a `ScrollView` needs `alignment: start`, or a short list spreads its rows over the window; read-only `TextInput` (one per block) makes text selectable; use `content-y` (not `viewport-y`); a callback named `close` shadows a builtin (use `dismiss`); `FocusScope.capture-key-pressed` intercepts keys before `LineEdit`; a wrapped read-only `TextInput` in a nested layout reports one line of height, so give it room with `min-height`, estimated from the available width (a fixed character count leaves a blank band under lines that still fit on one line); `Text` and `TextInput` have no `padding`, so put the space in the parent layout's `spacing` or `padding`.

## Process manager notes (measured or checked in the Slint 1.18 sources)

- `/proc` is read directly (no `sysinfo`, no `libc`). Per process: `stat` (the name sits between the first `(` and the **last** `)`), `status` (real uid, `RssAnon` = `Proc::mem_kb`, GNOME's "Memory": private memory, so shared pages are not double-counted in tree and app totals; Properties also reads `VmRSS`, `RssFile` + `RssShmem` and `VmSize` on demand) and the `exe` link (EACCES for other users' processes: those never get a warning). CPU % = process tick delta / total tick delta (first 8 `cpu` fields, guest excluded) x cores, so 100 = one core. Previous ticks are keyed by `(pid, start)`, so a reused PID gets no bogus delta. `Sampler::new()` takes a priming reading; the sampler thread waits 300 ms before the first real one.
- Network and disk totals only count devices that have a `device` entry under `/sys/class/net/<if>` and `/sys/block/<name>`: that drops lo, veth, bridges, VPNs, loop, zram, dm-* and md* (all of which double-count).
- The table is re-sorted and rebuilt every tick, so a row index is never trusted: the selection is stored as `(pid, start)` and `kill` re-reads `start` right before signalling (`Exited` if it changed). Rust sets `selected-row` after replacing the model; Rust never calls `ProcessesPage.select-row` (it emits `row-selected` and scrolls). The Slint side does, on purpose, for user actions only (a click, a right click, Up/Down, PageUp/PageDown): it ignores out-of-range rows, so Up with nothing selected is a no-op. `selected-row` is not clamped when the row count shrinks, but Rust sets it on every refresh.
- `State` lives in a `thread_local` on the UI thread. Build the `View` while it is borrowed and apply it after, so no borrow is held across a Slint call. (`row-selected` fires only from `select-row`, never from `set_vec` or from Rust setting `selected-row`.)
- `kill` runs the commands with `LC_ALL=C`: it matches "not permitted" / "No such process" in stderr, which are translated otherwise (pt_BR would never reach pkexec). Process names and exe paths are read lossily (invalid UTF-8 must not hide a process or escape the warnings). The PID-reuse check happens before the polkit dialog, not after it.
- The table is our own (`HeaderCell` + a `ListView` of `ProcRowItem`, fixed 24 px rows, in `main.slint`), because `StandardTableView` cells are text only and cannot indent or hold an icon. Column widths are in the `Cols` global, shared by header and rows; the fluent `ListView` scroll bar is 14 px and drawn over the rows, so both keep `Cols.bar` of right padding. Do not name a property `row` or `color` on a `Rectangle` (built-ins). A row is a `ProcRow` struct filled in Rust.
- Sorting is done in Rust on the numbers (`compare` in `main.rs`), from the header's `sort-ascending/descending(column)` callbacks; the page keeps the arrow state (`sort-col`, `sort-asc`; text columns start ascending, the others descending).
- **Subsystem notes** (Tree, Services tab, App groups, Icons, Slowdown banner and its real test, Graphs page, GPU, Look) are in [`docs/INTERNALS.md`](docs/INTERNALS.md): read the matching section before changing that subsystem. Rules that hold everywhere: the Confirm dialog is the only way to end/kill/stop/restart anything; process actions are gated on `selected-label != ""`, not `selected-row >= 0` (group rows have no process); never trust a row index, the table is rebuilt every tick.
- The three pages are stacked in one `Rectangle` under the kit's `TabBar` (`controls.slint`) (not Fluent's `TabWidget`, which stretches the tabs over the whole width) and only hidden with `visible: root.page == n`. Pages made with `if` would be destroyed on every tab switch and lose the scroll position and sort arrows. Ctrl+Tab is in `Main.shortcuts`.
- Sparklines are `Path`s with `fit: fill` (the default `contain` keeps the viewbox aspect and draws a small centred line). Rust writes the points in a 59 x 100 box, newest sample at the right, y = 100 - v. Strokes are not scaled by `fill`.
- A child of a plain `Rectangle` keeps its preferred size: give the table's `VerticalLayout` and overlay `Text` `width/height: parent`.
- `kill` blocks on pkexec (the polkit dialog), so it always runs in its own thread. Order: `/usr/bin/kill`, then `timeout 60 pkexec /usr/bin/kill` only if stderr says "not permitted"; exit 126 (dismissed) and 124 (`timeout`: dialog open for 60 s) are "cancelled", 127 is a failure with pkexec's message. The 60 s cap (`PKEXEC_WAIT_SECS`) bounds the known PID-reuse window (see `same_process`): the check runs before the dialog, so a forgotten dialog could otherwise signal a new process that got the PID. Not closed completely; that would need a root helper using a pidfd.
- The `/tmp/.mount_*` AppImage exemption holds only when that folder is a mount on another device than `/tmp` (`is_appimage_mount`): a plain folder with that name is flagged. Not verified against a real AppImage yet.
- The Warning rules and their known noise are documented on `suspicious` in `sys.rs`. `NameMismatch` fires on Firefox/Electron children and on `claude`; it is shown but not part of the "Suspicious only" filter (`Reason::is_strong`).

## Dialogs, menu, shortcuts and focus

Dialogs, the context menu, keyboard shortcuts and the focus traps are in `crates/app/ui/AGENTS.md`: read it before changing `crates/app/ui` or the handlers in `crates/app/src/main.rs` (agents that load nested `AGENTS.md` files get it automatically when working there). The two rules that matter everywhere: the Confirm dialog is the only way to End or Kill (never call those Rust callbacks from anywhere else), and every shortcut lives in `Main.shortcuts`.

## Translations

- UI strings use `@tr()`; strings built in Rust go through the `Tr` global in `ui/main.slint`. Never `format!` user-visible text in Rust.
- `.po` files are generated by `dev/gen_po.py` (edit its table, run it, commit both). Slint's gettext context is the component name (`Main`, `Tr`). A new `@tr` string needs an entry in every language. A string with a count needs its own singular string for `n == 1` ("1 connection", not "1 connections"), with its own entry.
- Language = `settings.language` (`""` = system locale).
- Appearance = `settings.theme` (`""` = follow the system, `"light"`, `"dark"`). The Appearance box sets `Palette.color-scheme` in the `.slint` (`unknown` hands it back to the desktop). `THEMES` and `theme_index` in `crates/app/src/main.rs` keep the order of that list, and a test checks they agree. `Family.dark` reads the drawn background, so it stays right in every state.

## Conventions

- Rust 2024, `rustfmt` max_width 120, clippy `-D warnings`. Comments explain *why*, not what.
- Settings are serde TOML with `#[serde(default)]`; add a unit test for each new option so old files keep loading.
- Settings path: `$XDG_DATA_HOME/FastProcMan/settings.toml`.
- Update `README.md` when behaviour changes, and this file when you learn something non-obvious.

## Testing

- `cargo test`. Manual harnesses (if any) go in `dev/`, must never capture the whole screen, and must never click or type at hard-coded coordinates.
- Never `pkill -f` with a pattern that appears in your own command line; kill by PID.

The real test of the slowdown banner (memory ramp and CPU loops) is in `docs/INTERNALS.md`, "Real test of the pressure warning".

### Verifying UI changes

- Any change to `crates/app/ui` or to the Rust that fills it is not done until `scripts/ui-snapshot.sh <page>` has been run and the PNGs in `target/ui-snapshots/` (`<page>-empty|single|many.png`) have been read. The script uses a private Xvfb display and canned data (`FASTPROCMAN_FIXTURES`), so it never scans or changes the real system.
- Specifically check: rows don't stretch when a list has 1 item, badges and buttons are right-aligned, icons are sharp, items with no icon get a placeholder so headers stay aligned, vertical gaps between rows of one kind are even, counts read right at 1 ("1 items"), a name already in a header is not repeated in every row under it, text is selectable as a block, truncated text, wrong units, and empty or error states (e.g. "nothing found") don't throw errors. Then fix and re-run.
- Every new page needs an entry in the script's `PAGES` and fixture data in `crates/app/src/fixtures.rs`. Anything behind `pkexec` still goes on the manual test list.
- Controls in a row are the same height (32 px); tabs, search boxes and dialog cards use the kit's components (`TabBar`, `SearchBox`, the card in FastHello's `docs/UI-STYLE.md`).
- If you can't launch it (e.g. the flow needs pkexec or a live X11 session), say so explicitly and list what I should test manually.

## GitHub flow and releases

Work on branches, PRs to `master`. CI: `ci.yml` (fmt, clippy, tests, MSRV, metainfo/desktop validation), `appimage.yml` (AppImage on every push/PR; on a `v*` tag it attaches to the release), `flatpak.yml` (bundle on every push/PR; on a `v*` tag it also publishes the signed remote and the landing page to GitHub Pages, then attaches the bundle), `virustotal.yml` (scans the AppImage and the Flatpak after their workflows; needs the `VIRUSTOTAL_API_KEY` secret). `RELEASING.md` has the one-time setup and the steps. Never commit or push on the owner's behalf.

Release gotchas (each one broke a first release):

- Bump the version in `Cargo.toml`, then run `cargo check` and commit `Cargo.lock`: CI uses `--locked`, so a stale lock fails every job.
- The metainfo needs a `<release version="X">` for the tag, or `flatpak.yml` fails.
- `appimagetool` builds `.DirIcon` from a PNG at the AppDir root, not from the SVG. Regenerate `packaging/io.github.evandro777.FastProcMan.png` when the SVG changes: `convert -background none -density 384 packaging/io.github.evandro777.FastProcMan.svg -resize 256x256 packaging/io.github.evandro777.FastProcMan.png`. ImageMagick ignores gradients and opacity in the SVG, so use solid colours.
- The first tag needs the one-time setup in `RELEASING.md`: the signing key and its secret, Pages, and the `v*` tag rule on the `github-pages` environment.

## Flatpak

App id `io.github.evandro777.FastProcMan`; `flatpak/build.sh --devel` installs `...FastProcMan.Devel` next to a release. Needs `flatpak-builder` and `org.freedesktop.Sdk//25.08` + `org.freedesktop.Sdk.Extension.rust-stable//25.08`. The runtime version `25.08` appears in five places (the manifest, the container image tag, the rust-stable install and its `GITHUB_PATH` line in `flatpak.yml`, and the header of `flatpak/build.sh`): bump them together.

- Auto-update = our own signed OSTree remote on GitHub Pages (`flatpak.yml`: `bundle` → `pages` → `release`), deployed on `v*` tags only. Every deploy replaces the whole site, so never deploy Pages from another trigger. The public key is `flatpak/signing-key.b64` (base64 of the binary export, made by `flatpak/new-signing-key.sh`; the template ships none); the private half is the secret `FLATPAK_GPG_PRIVATE_KEY`. See `docs/FLATPAK-SIGNING.md`.
- Keep `finish-args` minimal (the manifest says how for X11-only apps and trays). The build vendors crates (`cargo vendor`), so `flatpak/vendor` must not be in the manifest's `skip:`.
- Patterns to copy when an app needs them (FastClipboard has each one working): inside the sandbox `FLATPAK_ID` is set. A single-instance Unix socket must live in `$XDG_RUNTIME_DIR/app/$FLATPAK_ID/`, the only directory shared between `flatpak run`s. A ksni tray needs `.disable_dbus_name(true)` there. Start at login must go through the Background portal (`org.freedesktop.portal.Background.RequestBackground`, blocking, so from a thread), not a file in `~/.config`. A host command (for example `tesseract`) is reachable only through `flatpak-spawn --host`, which needs `--talk-name=org.freedesktop.Flatpak`. Normally that is the user's own opt-in `flatpak override`, never in the manifest. **FastProcMan is the exception**: the owner wants a Flatpak that sees every process (like Mission Center), so our manifest has it, `src/host.rs` relaunches the app on the host, and the README warns that it is not sandboxed. Do not copy that to a Flathub manifest.
- `src/host.rs` runs `flatpak-spawn --host --watch-bus` with the runtime's `ld.so` and `--library-path` (`<runtime>/lib/<triple>:<app>/lib`, both host paths read from `/.flatpak-info`), `FASTPROCMAN_ON_HOST=1` and `XDG_DATA_HOME` forwarded, display variables not. A copy that exits non-zero in under 30 s counts as failed and the app carries on in the sandbox with a status message. It only relaunches when `FLATPAK_ID` starts with the app id, so another Flatpak's variable cannot trigger it.
- **Flathub policy:** manifests must NOT contain AI-generated content and AI must not open the submission PR. Do not write a Flathub manifest or submission text for the owner; `docs/FLATHUB.md` explains the process.

## Open items

- **Flatpak on the host is untested on a real desktop** (see the Flatpak section). Biggest unknown: pkexec from a process started by the user's systemd, outside the login session, may say "Not authorized" without a prompt. Also unknown: fonts and xkb data come from the host.
- Not built: column resizing, per-process network (needs root or eBPF) and per-process disk columns (Properties shows read/write totals instead), tray, clearing the status line after a while. The Up/Down shortcuts from the filter go through `select-row` and leave the keyboard focus in the filter.
- Numbers use `.` as the decimal separator in every language (formatted in Rust).
- CPU is ~1.2% of a core at 2 s with ~490 processes. A cheap win if it matters: `statm` instead of `status` for RSS (needs the page size without `libc`). Do **not** cache the uid or the warning per `(pid, start)`: a process can `exec /tmp/evil` and keep both, and a cached "clean" verdict would hide it (setuid changes the uid the same way).
- Tests that spawn a child take the `SPAWN` lock in `sys.rs`: a child forked while another test still has a freshly copied binary open for writing makes its exec fail with `ETXTBSY`.
- Driving the real app on a private Xvfb: `xdotool key` needs XTEST (no `--window`; winit ignores synthetic key events) after `xdotool windowfocus`. Clicks are relative to the window.

