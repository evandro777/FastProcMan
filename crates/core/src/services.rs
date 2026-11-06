//! systemd services: the unit list from `systemctl`, PID/CPU/memory from the cgroup tree, and start/stop/restart.
use crate::sys::{KillError, Proc};
use std::{collections::HashMap, fs, path::Path, process::Command};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    /// Active but not running: a oneshot that finished.
    Exited,
    Failed,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub status: Status,
    /// Unit-file state as systemd reports it (`enabled`, `disabled`, `static`, `masked`...); empty when unknown.
    pub startup: String,
    /// Lowest PID in the service's cgroup; 0 when it has no process.
    pub pid: u32,
    pub user: String,
    pub cpu_pct: f32,
    pub mem_kb: u64,
}

impl Service {
    pub fn enabled(&self) -> bool {
        self.startup.starts_with("enabled")
    }
}

/// Read-only listings run on the sampler thread: `timeout` keeps a stuck systemd or D-Bus from freezing the whole
/// process table (it gives an empty list instead). Not used by `control`, which waits for the polkit dialog.
fn systemctl(args: &[&str]) -> String {
    Command::new("timeout")
        .args(["5", "systemctl"])
        .env("LC_ALL", "C")
        .args(args)
        .args(["--type=service", "--plain", "--no-legend", "--no-pager"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Unit name → unit-file state. Slow (about 0.7 s): call it when the tab opens, not on every tick.
pub fn unit_files() -> HashMap<String, String> {
    parse_unit_files(&systemctl(&["list-unit-files"]))
}

/// `UNIT FILE   STATE   PRESET` lines. Aliases are the same unit under another name, so they are dropped.
fn parse_unit_files(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .filter(|(_, state)| state != "alias")
        .collect()
}

/// `UNIT LOAD ACTIVE SUB DESCRIPTION` lines as `(name, active, sub)`; units that do not exist (`not-found`) are
/// dropped: they cannot be started.
fn parse_units(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, load, active, sub) = (it.next()?, it.next()?, it.next()?, it.next()?);
            (load != "not-found" && name.ends_with(".service"))
                .then(|| (name.to_string(), active.to_string(), sub.to_string()))
        })
        .collect()
}

/// Service name → PIDs, from the cgroup tree. It stops at a `*.service` directory: `user@1000.service` would
/// otherwise not be the only one whose sub-cgroups swallow a whole session.
// shortcut: a service's sub-cgroups are not summed, so delegated services (Docker) under-count; read the
// subtree if that matters.
fn cgroup_pids(dir: &Path, out: &mut HashMap<String, Vec<u32>>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".service") {
            let pids = fs::read_to_string(e.path().join("cgroup.procs")).unwrap_or_default();
            out.insert(name, pids.lines().filter_map(|l| l.trim().parse().ok()).collect());
        } else {
            cgroup_pids(&e.path(), out);
        }
    }
}

/// The unit file a loaded unit comes from: an instance (`getty@tty1.service`) uses its template.
fn startup_of<'a>(files: &'a HashMap<String, String>, name: &str) -> &'a str {
    let template = name.split_once('@').map(|(base, _)| format!("{base}@.service"));
    files
        .get(name)
        .or_else(|| template.and_then(|t| files.get(&t)))
        .map_or("", String::as_str)
}

fn build(
    units: Vec<(String, String, String)>,
    files: &HashMap<String, String>,
    cgroups: &HashMap<String, Vec<u32>>,
    procs: &[Proc],
) -> Vec<Service> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out: Vec<Service> = units
        .iter()
        .map(|(name, active, sub)| {
            seen.insert(name);
            let pids = cgroups.get(name).map(Vec::as_slice).unwrap_or_default();
            let mine: Vec<&Proc> = procs.iter().filter(|p| pids.contains(&p.pid)).collect();
            let first = mine.iter().min_by_key(|p| p.pid);
            Service {
                name: name.clone(),
                status: match (active.as_str(), sub.as_str()) {
                    (_, "running") => Status::Running,
                    ("active", _) => Status::Exited,
                    ("failed", _) => Status::Failed,
                    _ => Status::Stopped,
                },
                startup: startup_of(files, name).to_string(),
                pid: first.map_or(0, |p| p.pid),
                user: first.map(|p| p.user.clone()).unwrap_or_default(),
                cpu_pct: mine.iter().map(|p| p.cpu_pct).sum(),
                mem_kb: mine.iter().map(|p| p.mem_kb).sum(),
            }
        })
        .collect();
    // Installed but not loaded (inactive and not wanted by anything): still listed, as Stopped. Templates cannot be
    // started by name.
    for (name, state) in files {
        if !name.contains("@.") && !seen.contains(&name.as_str()) && state != "masked" {
            out.push(Service {
                name: name.clone(),
                status: Status::Stopped,
                startup: state.clone(),
                pid: 0,
                user: String::new(),
                cpu_pct: 0.0,
                mem_kb: 0,
            });
        }
    }
    out
}

/// All services now. `files` comes from `unit_files()`, read once when the tab opens. `None` when `systemctl` gave
/// nothing (timed out or failed): the caller keeps the previous list instead of showing a machine with no services.
pub fn list(procs: &[Proc], files: &HashMap<String, String>) -> Option<Vec<Service>> {
    let units = parse_units(&systemctl(&["list-units", "--all"]));
    if units.is_empty() {
        return None;
    }
    let mut cg = HashMap::new();
    cgroup_pids(Path::new("/sys/fs/cgroup"), &mut cg);
    Some(build(units, files, &cg, procs))
}

/// `systemctl start|stop|restart <unit>`. systemctl asks polkit itself (no pkexec). Blocking: call it from a thread.
pub fn control(verb: &str, unit: &str) -> Result<(), KillError> {
    let out = Command::new("systemctl")
        .env("LC_ALL", "C")
        .args([verb, "--no-pager", "--", unit])
        .output()
        .map_err(|e| KillError::Failed(e.to_string()))?;
    control_result(out.status.success(), String::from_utf8_lossy(&out.stderr).trim())
}

/// A dismissed polkit dialog is "Access denied"; "Interactive authentication required" (no agent) stays a failure so
/// it is not mistaken for a cancel.
fn control_result(ok: bool, err: &str) -> Result<(), KillError> {
    match ok {
        true => Ok(()),
        false if err.contains("Access denied") => Err(KillError::Cancelled),
        false => Err(KillError::Failed(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu: f32, mem: u64) -> Proc {
        Proc {
            pid,
            start: 1,
            name: "x".into(),
            user: format!("u{pid}"),
            cpu_pct: cpu,
            mem_kb: mem,
            warn: None,
            state: 'S',
            ppid: 1,
            exe: None,
        }
    }

    /// Prints the real list to compare with `systemctl`:
    /// `cargo test -p fastprocman-core live_services -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_services() {
        let mut sampler = crate::sys::Sampler::new();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let procs = sampler.sample().procs;
        let t = std::time::Instant::now();
        let files = unit_files();
        println!("unit_files: {} entries in {:?}", files.len(), t.elapsed());
        let t = std::time::Instant::now();
        let list = list(&procs, &files).unwrap_or_default();
        println!("list: {} services in {:?}", list.len(), t.elapsed());
        for s in list.iter().filter(|s| s.pid != 0).take(8) {
            println!("{s:?}");
        }
        assert!(list.iter().all(|s| !s.name.contains("@.")));
    }

    #[test]
    fn systemctl_errors_map_to_cancelled_or_failed() {
        assert_eq!(control_result(true, ""), Ok(()));
        assert_eq!(
            control_result(false, "Failed to stop x.service: Access denied"),
            Err(KillError::Cancelled)
        );
        // No polkit agent: not a cancel.
        let err = "Failed to stop x.service: Interactive authentication required.";
        assert_eq!(control_result(false, err), Err(KillError::Failed(err.into())));
    }

    #[test]
    fn parses_and_merges() {
        let units = parse_units(
            "cron.service loaded active running Regular background program processing daemon\n\
             boot.service loaded active exited Boot\n\
             bad.service loaded failed failed Bad\n\
             gone.service not-found inactive dead gone.service\n\
             getty@tty1.service loaded active running Getty\n",
        );
        assert_eq!(units.len(), 4);
        let files = parse_unit_files(
            "cron.service enabled enabled\ngetty@.service enabled enabled\nsshd.service disabled enabled\n\
             old.service alias -\nmask.service masked enabled\nfoo@.service disabled -\n",
        );
        assert!(!files.contains_key("old.service"));
        let mut cg = HashMap::new();
        cg.insert("cron.service".to_string(), vec![30, 20]);
        let s = build(
            units,
            &files,
            &cg,
            &[proc(20, 1.5, 100), proc(30, 0.5, 50), proc(99, 9.0, 9)],
        );
        let by = |n: &str| s.iter().find(|s| s.name == n).unwrap();
        let cron = by("cron.service");
        assert_eq!((cron.pid, cron.mem_kb, cron.cpu_pct), (20, 150, 2.0));
        assert_eq!(cron.user, "u20");
        assert!(cron.enabled());
        assert_eq!(by("boot.service").status, Status::Exited);
        assert_eq!(by("bad.service").status, Status::Failed);
        assert_eq!(by("getty@tty1.service").startup, "enabled");
        // Listed though not loaded; masked units and templates are not.
        assert_eq!(by("sshd.service").status, Status::Stopped);
        assert!(!s.iter().any(|s| s.name == "mask.service" || s.name == "foo@.service"));
    }
}
