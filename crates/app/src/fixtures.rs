//! Canned data for `scripts/ui-snapshot.sh` (`FASTPROCMAN_FIXTURES=empty|single|many`): every page can be rendered
//! with zero, one or many rows without sampling or touching the real system.
//! `FASTPROCMAN_FILTER=<text>` also applies a filter, to see the "no matching processes" state.
//! `FASTPROCMAN_CALM=1` turns the slowdown banner off in `many`.
use crate::{APP, Main, icons, on_sample, open_properties, refresh, select};
use fastprocman_core::{
    gpu::GpuStats,
    mounts::Mount,
    services::{Service, Status},
    sys::{Details, DiskStats, Proc, Reason, Sample, SysStats},
    tree,
};
use slint::{ComponentHandle, Model};
use std::time::Duration;

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
            pgrp: 0,
            sid: 0,
            tty: false,
        })
        .collect()
}

/// `single` (mode 1) has no swap, no GPU and one disk, so one snapshot covers the cards that disappear and an odd
/// number of cards; `many` has swap, two GPUs (one with a long name, one with load only) and three disks (eight cards).
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
            vec![
                GpuStats {
                    name: "NVIDIA GeForce RTX 4070 SUPER".into(),
                    busy: (50.0 + 40.0 * t.sin()) as f32,
                    enc: Some((20.0 + 15.0 * (t * 1.3).cos()) as f32),
                    dec: Some(if i % 12 < 6 { 35.0 } else { 0.0 }),
                    mem_used: 1_500_000_000 + 10_000_000 * i as u64,
                    mem_total: 12_878_610_432,
                },
                // An integrated card: load only, no video memory.
                GpuStats {
                    name: "GPU 1".into(),
                    busy: (25.0 + 20.0 * (t * 0.7).cos()) as f32,
                    ..Default::default()
                },
            ]
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
            active: matches!(status, Status::Running | Status::Exited),
            startup: startup.into(),
            pid,
            user: user.into(),
            cpu_pct,
            mem_kb,
        })
        .collect()
}

/// Rows and chart history as `[0, 1, 18]` and `[1, 2, 60]` samples, so one fixture covers all three states.
fn mounts(n: usize) -> Vec<Mount> {
    // Sizes in MB; a nearly full stick (warning colour) and a long share path (elided).
    [
        ("/dev/nvme0n1p2", "/", "btrfs", 487_873, 335_571, 151_251),
        ("/dev/nvme0n1p2", "/home", "btrfs", 487_873, 335_571, 151_251),
        ("/dev/nvme0n1p1", "/boot/efi", "vfat", 511, 6, 505),
        ("/dev/sda1", "/media/user/USB STICK", "exfat", 29_500, 28_100, 1_400),
        (
            "//nas.local/backups",
            "/mnt/nas backups/photos",
            "cifs",
            3_800_000,
            1_200_000,
            2_600_000,
        ),
    ]
    .into_iter()
    .take(n)
    .map(
        |(device, target, fstype, total, used, avail): (_, _, _, u64, u64, u64)| Mount {
            device: device.into(),
            target: target.into(),
            fstype: fstype.into(),
            total: total << 20,
            used: used << 20,
            avail: avail << 20,
        },
    )
    .collect()
}

/// `FASTPROCMAN_PAGE` opens a page, a dialog or the context menu directly, for the same script. With or without
/// fixtures.
pub fn open_page(ui: &Main) {
    match std::env::var("FASTPROCMAN_PAGE").as_deref() {
        Ok("about") => ui.set_about_open(true),
        Ok("settings") => ui.set_settings_open(true),
        Ok(p @ ("confirm" | "confirm-kill")) => {
            // The fixture's one process with an icon (fastprocman, 1011), so the snapshot shows it in the title.
            if let Some(row) = APP.with_borrow(|s| s.shown.iter().position(|k| k.0 == 1011)) {
                select(ui, row as i32);
                ui.set_selected_row(row as i32);
                if let Some(r) = ui.get_rows().row_data(row) {
                    ui.set_confirm_icon(r.icon);
                }
            }
            ui.set_confirm_force(p == "confirm-kill");
            // A parent with workers, so the snapshot shows the checkbox (plural and singular).
            ui.set_confirm_kids(if p == "confirm-kill" { 1 } else { 16 });
            ui.set_confirm_label(ui.get_selected_label());
            ui.set_confirm_open(true);
        }
        Ok("properties") => {
            // The fixture's flagged process (updater, 1002), so the snapshot shows the Warning row and its note.
            if let Some(row) = APP.with_borrow(|s| s.shown.iter().position(|k| k.0 == 1002)) {
                select(ui, row as i32);
            }
            open_properties(ui)
        }
        Ok("tree") => {
            // Not saved: a snapshot must not change the owner's settings. The tree starts open; firefox (1000) is
            // closed, so the snapshot shows both chevrons and a collapsed row's totals.
            ui.set_tree(true);
            // Fixture rows have no launchers (no resolver runs), so name the apps here: firefox is cut out of its
            // parent cinnamon, Xorg and pipewire are two programs of one app (one group), and the rest is
            // "Processes". The apps start closed; Firefox is opened to show its children.
            APP.with_borrow_mut(|s| {
                s.tree = true;
                s.flipped.insert((1000, 1000));
                s.flipped.insert(tree::group_key("Firefox Web Browser"));
                for (key, app) in [
                    ("firefox", "Firefox Web Browser"),
                    ("fastprocman", "FastProcMan"),
                    ("Xorg", "Display Server"),
                    ("pipewire", "Display Server"),
                ] {
                    s.apps.insert(key.into(), app.into());
                }
            });
            refresh(ui);
        }
        Ok("menu") => {
            // The popup needs the window shown and laid out first.
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(300), move || {
                weak.upgrade().iter().for_each(|ui| ui.invoke_show_menu());
            });
        }
        Ok("confirm-service") => {
            ui.set_page(2);
            ui.set_confirm_service(1);
            ui.set_confirm_label(ui.get_svc_selected_label());
            ui.set_confirm_open(true);
        }
        // A window tall enough for every Graphs card, which the 700 px default scrolls.
        Ok("graphs-tall") => {
            ui.set_page(1);
            ui.window().set_size(slint::LogicalSize::new(860.0, 1150.0));
        }
        Ok(p) => ui.set_page(p.parse().unwrap_or(0)),
        Err(_) => {}
    }
}

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
    crate::on_mounts(ui, mounts([0, 1, 5][mode]));
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
