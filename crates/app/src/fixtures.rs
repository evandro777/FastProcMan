//! Canned data for `scripts/ui-snapshot.sh` (`FASTPROCMAN_FIXTURES=empty|single|many`): every page can be rendered
//! with zero, one or many rows without sampling or touching the real system.
//! `FASTPROCMAN_FILTER=<text>` also applies a filter, to see the "no matching processes" state.
//! `FASTPROCMAN_CALM=1` turns the slowdown banner off in `many`.
use crate::{APP, Main, icons, on_sample, refresh, select};
use fastprocman_core::{
    gpu::GpuStats,
    services::{Service, Status},
    sys::{Details, DiskStats, Proc, Reason, Sample, SysStats},
};

/// 0 empty, 1 single, 2 many; `None` when the variable is unset or unknown.
pub fn mode() -> Option<usize> {
    match std::env::var("FASTPROCMAN_FIXTURES").ok()?.as_str() {
        "empty" => Some(0),
        "single" => Some(1),
        "many" => Some(2),
        _ => None,
    }
}

/// `(name, user, cpu %, rss kB, warning)`; one long name to check truncation, one row per warning, one root row.
#[rustfmt::skip]
const PROCS: [(&str, &str, f32, u64, Option<Reason>); 18] = [
    ("firefox", "seven", 34.2, 1_250_000, None),
    ("a-very-long-process-name-that-should-be-cut-off-instead-of-stretching-the-table-sideways", "seven", 12.5, 90_000, None),
    ("updater", "seven", 8.0, 15_300, Some(Reason::TempDir)),
    ("kworker/u32:1", "root", 0.3, 0, None),
    ("payload", "seven", 5.5, 4_200, Some(Reason::Deleted)),
    ("Web Content", "seven", 4.4, 410_000, Some(Reason::NameMismatch)),
    ("Xorg", "root", 3.1, 120_500, None),
    ("cinnamon", "seven", 2.2, 310_000, None),
    ("pipewire", "seven", 0.9, 18_000, None),
    ("systemd", "root", 0.0, 12_800, None),
    ("zsh", "seven", 0.0, 6_100, None),
    ("fastprocman", "seven", 0.5, 31_000, None),
    // A build chain under zsh, so the tree reaches depth 8 like a real desktop session does.
    ("bash", "seven", 0.0, 3_000, None),
    ("make", "seven", 0.1, 2_500, None),
    ("cargo", "seven", 0.2, 40_000, None),
    ("rustc", "seven", 60.0, 300_000, None),
    ("cc1", "seven", 1.5, 60_000, None),
    ("as", "seven", 0.4, 8_000, None),
];

/// Parent of each row in `PROCS`, by PID (1000 + index): systemd (1009) → cinnamon → firefox → its children, the
/// kernel worker under a missing parent 2 (so it is a root), the rest under systemd, and the build chain
/// zsh → bash → make → cargo → rustc → cc1 → as.
const PPID: [u32; 18] = [
    1007, 1000, 1000, 2, 1000, 1000, 1009, 1009, 1009, 1, 1007, 1009, 1010, 1012, 1013, 1014, 1015, 1016,
];

fn procs(n: usize) -> Vec<Proc> {
    PROCS
        .iter()
        .take(n)
        .zip(1000u32..)
        .map(|(&(name, user, cpu_pct, mem_kb, warn), pid)| Proc {
            pid,
            start: pid as u64,
            name: name.into(),
            user: user.into(),
            cpu_pct,
            mem_kb,
            warn,
            // One stopped process, so the menu's "Resume" state can be captured.
            state: if name == "zsh" { 'T' } else { 'S' },
            ppid: PPID[(pid - 1000) as usize],
            exe: None,
        })
        .collect()
}

/// `single` (mode 1) has no swap, no GPU and one disk, so one snapshot covers the cards that disappear and an odd
/// number of cards; `many` has swap, a GPU with a long name and three disks (seven cards: an odd one out).
fn stats(i: usize, mode: usize) -> SysStats {
    let t = i as f64 / 5.0;
    let many = mode > 1;
    let disk = |name: &str, shift: f64| DiskStats {
        name: name.into(),
        read: if (i + shift as usize) % 9 < 3 {
            8_000_000.0
        } else {
            100_000.0
        },
        write: 2_000_000.0 * (1.0 + (t * 1.7 + shift).sin()),
    };
    SysStats {
        cpu_pct: (35.0 + 25.0 * t.sin()) as f32,
        cores: 8,
        cores_pct: (0..8)
            .map(|c| (45.0 + 40.0 * (t + c as f64 * 0.9).sin()).clamp(0.0, 100.0) as f32)
            .collect(),
        // `many` ends at ~85% used: the amber early-warning banner with three names (the danger line is 90%).
        // `FASTPROCMAN_CALM=1` keeps `many` below the warning, for README screenshots without the banner.
        mem_used_kb: if many && std::env::var_os("FASTPROCMAN_CALM").is_none() {
            13_400_000 + 4_000 * i as u64
        } else {
            7_000_000 + 40_000 * i as u64
        },
        mem_total_kb: 16_000_000,
        swap_used_kb: if many { 500_000 + 20_000 * i as u64 } else { 0 },
        swap_total_kb: if many { 4_000_000 } else { 0 },
        // `single` shows the red disk banner with no names (no `D` row).
        psi_cpu: 0.0,
        psi_mem: 0.0,
        psi_io: if mode == 1 { 30.0 } else { 0.0 },
        net_rx: 1_500_000.0 * (1.0 + t.sin()),
        net_tx: 200_000.0 * (1.0 + t.cos()),
        disks: if many {
            vec![disk("nvme0n1", 0.0), disk("nvme1n1", 2.0), disk("sda", 4.0)]
        } else {
            vec![disk("nvme0n1", 0.0)]
        },
        gpus: if many {
            vec![GpuStats {
                name: "NVIDIA GeForce RTX 4070 SUPER".into(),
                busy: (50.0 + 40.0 * t.sin()) as f32,
                enc: Some((20.0 + 15.0 * (t * 1.3).cos()) as f32),
                dec: Some(if i % 12 < 6 { 35.0 } else { 0.0 }),
                mem_used: 1_500_000_000 + 10_000_000 * i as u64,
                mem_total: 12_878_610_432,
            }]
        } else {
            Vec::new()
        },
    }
}

/// What the Properties dialog shows for any fixture row: a long command line to check wrapping, a program path that
/// cannot be read (another user's process) and a long running time.
pub fn details() -> Details {
    Details {
        ppid: 1234,
        state: 'S',
        threads: 12,
        nice: 0,
        age_secs: 93_784,
        cmdline: Some(
            "/usr/lib/firefox/firefox -contentproc -parentBuildID 20260101000000 -prefsHandle 0:41250 \
             -prefMapHandle 1:277810 -jsInitHandle 2:248908 -sbStartup -sbAppPath /usr/lib/firefox -appDir \
             /usr/lib/firefox/browser 4172 true tab"
                .into(),
        ),
        exe: None,
        cwd: Some("/home/seven".into()),
        io: Some((1_610_612_736, 83_886_080)),
        // Resident - shared = the 1.19 GB the fixture row shows as Memory.
        resident_kb: Some(1_560_000),
        shared_kb: Some(340_000),
        virtual_kb: Some(4_200_000_000 / 1024),
    }
}

/// `(name, status, startup, pid, user, cpu %, rss kB)`: running, exited, failed, stopped, a long name and a unit
/// that is installed but not loaded.
#[rustfmt::skip]
const SERVICES: [(&str, Status, &str, u32, &str, f32, u64); 8] = [
    ("cron.service", Status::Running, "enabled", 812, "root", 0.0, 2_400),
    ("NetworkManager.service", Status::Running, "enabled", 790, "root", 1.2, 21_000),
    ("ModemManager.service", Status::Failed, "enabled", 0, "", 0.0, 0),
    ("apparmor.service", Status::Exited, "enabled", 0, "", 0.0, 0),
    ("cups.service", Status::Stopped, "disabled", 0, "", 0.0, 0),
    ("a-very-long-service-name-that-should-be-cut-off-instead-of-stretching-the-table-sideways.service", Status::Running, "static", 4021, "seven", 3.4, 90_000),
    ("ssh.service", Status::Stopped, "disabled", 0, "", 0.0, 0),
    ("systemd-logind.service", Status::Running, "static", 640, "root", 0.1, 3_900),
];

fn services(n: usize) -> Vec<Service> {
    SERVICES
        .iter()
        .take(n)
        .map(|&(name, status, startup, pid, user, cpu_pct, mem_kb)| Service {
            name: name.into(),
            status,
            startup: startup.into(),
            pid,
            user: user.into(),
            cpu_pct,
            mem_kb,
        })
        .collect()
}

/// Rows and chart history as `[0, 1, 18]` and `[1, 2, 60]` samples, so one fixture covers all three states.
pub fn apply(ui: &Main, mode: usize) {
    // No resolver thread here: one row gets the app's own icon and the rest keep the empty slot, so a snapshot shows
    // both. The key is the process name because fixture rows have no program path.
    let png = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/io.github.evandro777.FastProcMan.png"
    );
    if let Some(img) = icons::load(std::path::Path::new(png)) {
        APP.with_borrow_mut(|s| _ = s.icons.insert("fastprocman".into(), img));
    }
    for i in 0..[1, 2, 60][mode] {
        on_sample(
            ui,
            Sample {
                procs: procs([0, 1, 18][mode]),
                sys: stats(i, mode),
            },
        );
    }
    crate::svc::on_services(ui, services([0, 1, 8][mode]));
    if mode > 0 {
        ui.invoke_svc_row_selected(0);
    }
    if let Ok(f) = std::env::var("FASTPROCMAN_FILTER") {
        ui.set_filter(f.as_str().into());
        ui.invoke_filter_changed(f.into());
    }
    // One row selected, to see the label and what the menu and dialogs act on.
    if mode > 0 {
        select(ui, [0, 0, 2][mode]);
        refresh(ui);
    }
}
