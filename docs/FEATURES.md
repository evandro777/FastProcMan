# FastProcMan features in detail

The full behaviour reference. The README has the short version; keep both in step when behaviour changes.

## Processes tab

Name, PID, user, CPU % and memory of every process, read straight from `/proc` (Memory is the process's own private memory, the same number as "Memory" in GNOME System Monitor: resident minus shared, so shared libraries are not counted again in every program, nor in the tree's totals), with the program's icon when one is found (the window opens first and the icons fill in a moment later). Click a column to sort (Memory, descending, by default).

The **Tree** switch (bottom bar, saved in `settings.toml`) shows each process under its parent, grouped by application like Mission Center (Settings → "Group the tree by application", on by default; off gives one plain tree):

- Programs with a visible `.desktop` launcher get a bold row named after it (collapsed at first, with the CPU and memory of the whole group), and everything else is under **Processes** (open).
- A program started from another one (Code from Files) is its own application, not part of the first.
- Group rows are not processes: Quit, Force quit, Properties and the menu do nothing on them.
- A Flatpak app's `bwrap` helper lands in Processes.
- Inside a group everything starts expanded. A collapsed row shows the CPU and memory of itself plus everything under it.
- `Left` and `Right` close and open the selected row. **Expand all** and **Collapse all** (in the bottom bar, in tree mode) do the whole tree.
- A filter shows the matches with their parents.

Just start typing to filter by name or PID (`Ctrl+F` also focuses the box); `Esc` clears the filter; `Up` and `Down` move through the list, even from the filter box; `PageUp` and `PageDown` move a page at a time; `Alt+Enter` opens the Properties of the selected process.

## Right-click a process

The `Menu` key or `Shift+F10` opens the same menu for the selected row.

- **Quit…** (asks the program to close, SIGTERM, `Delete`, also a button in the bottom bar when the tree is off) and **Force quit…** (stops it at once, SIGKILL, `Shift+Delete`; for a program that does not respond) always ask for confirmation first. Your own processes are signalled directly; for other users' processes the app falls back to `pkexec`, so polkit asks for the password (the prompt is cancelled after 60 seconds). The process is checked again right before the signal (before the password prompt, for other users' processes), so a re-sorted table or a reused PID does not kill the wrong one. PID 1 and FastProcMan itself are refused.
- **Suspend / Resume** (SIGSTOP / SIGCONT).
- **Priority:** very high (-10), high (-5), normal (0), low (5) or very low (19). Raising the priority needs `pkexec`.
- **Properties:** parent PID, state, threads, priority, running time, memory, resident memory, shared memory, virtual memory, disk read and written since start, program, command line and working folder (selectable text). What the system hides from you, for example another user's program path, shows as "Unavailable". The values update while it is open, and `Esc` closes it.

## Suspicious processes

The **Warning** column flags:

- a program that was deleted from disk while running (not under `/usr`, `/opt`, `/snap`, `/app`),
- one running from `/tmp`, `/var/tmp` or `/dev/shm`,
- one whose name differs from its program.

**Suspicious only** filters on the first two; the name check is shown but not filtered on, because Firefox, Electron and similar apps rename their child processes. The first two are red in the table, and a collapsed row or closed app group in tree mode shows the one hidden inside it; **Properties** explains the flag in one sentence. Programs of other users cannot be inspected, so they are never flagged. It is a hint, not a malware detector.

## Slowdown warning

A banner above the tabs warns before the system freezes.

- **Amber** when less than 20% of the memory is available ("opening another big app may freeze the system": the desktop still responds).
- **Red** when less than 10% is left or the kernel reports that tasks are stalling (pressure stall information, `/proc/pressure`, 10 s average: memory above 10%, disk above 20%, CPU above 60%).
- One button per likely cause, up to three: the biggest memory users, processes waiting on the disk (state `D`) or the busiest processes. Those rows are amber in the table. Click a button to select that process on the Processes tab, then `Delete` to end it (it still asks first).
- Nothing is ended automatically, and a machine that is already frozen cannot show it: use `earlyoom` or `systemd-oomd` for that. Needs a kernel with PSI (5.2+); without it there is no warning.

## Settings

The ☰ button at the top right of the Processes tab, or `Ctrl+,`; **About** is a button inside it.

- **CPU % of the whole machine.** Off by default: the CPU % of a process is per core, as in `top` (100 = one core, so a busy process can pass 100). On: 100 = all cores together. Saved as `cpu_whole_machine` in `settings.toml`. The CPU graph is always the whole machine.
- **Language:** follows the system or picks English or Brazilian Portuguese, and applies at once.
- The window size is remembered too (saved as `width` and `height` when the window closes, not while it is maximized or fullscreen).

## Graphs tab

The last 60 samples (one minute at the default 1 s), refreshed every 1 (default), 2 or 5 seconds (**Refresh** at the bottom), in cards that scroll when they do not fit:

- **CPU** (the whole machine in colour, one faint line per logical core behind it),
- **Memory** (with a swap line and its own figures when the system has swap),
- **Network** (download/upload),
- one **GPU** card per graphics card (processing, video encode, video decode and video memory),
- one **Disk** card per physical disk (read/write).

Network and disks count physical devices only. The GPU card needs the NVIDIA driver (read through its `libnvidia-ml.so.1`, loaded only if present) or an AMD card (sysfs; no encode/decode figures); without either there is no GPU card, and Intel graphics are not supported.

## Services tab

The system's systemd services with the process table's columns (Name, PID, User, CPU %, Memory) plus **Status** (Running, Exited for a finished oneshot, Failed, Stopped) and **Startup** (Enabled, Disabled, Static, Masked).

- PID, CPU and memory are the sums over the service's cgroup (blank when it has no process).
- Just start typing to filter by name, and pick a **Status** (Running / Failed / Stopped) and a **Startup** (Enabled / Disabled / Static / Masked) in two separate boxes, which combine.
- **Start**, **Stop** and **Restart** the selected one from the bottom bar. Stop and Restart ask first. The password prompt is polkit's, shown by `systemctl` itself.
- Only system services; user services and enabling/disabling are not supported. The list is read only while the tab is shown. `Ctrl+F` focuses its filter box.

## Keyboard

Type to filter, `Esc` clear the filter (or close a dialog), `Ctrl+F` filter box, `Up`/`Down` select, `PageUp`/`PageDown` select a page away, `Alt+Enter` properties, `Delete` quit, `Shift+Delete` force quit, `Menu` or `Shift+F10` menu, `Enter` confirm, `Ctrl+Tab` next tab.
