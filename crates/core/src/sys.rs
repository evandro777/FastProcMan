//! Process and system sampling straight from `/proc` (no `sysinfo`, no `libc`), plus the kill flow.
use std::{collections::HashMap, fs, path::Path, process::Command, time::Instant};

/// Why a process is flagged. Only `Deleted` and `TempDir` count for the "suspicious only" filter:
/// `NameMismatch` fires on Firefox/Electron children that rename themselves, so it is shown but not filtered on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Deleted,
    TempDir,
    NameMismatch,
}

impl Reason {
    pub fn is_strong(self) -> bool {
        self != Reason::NameMismatch
    }
}

/// Worse is bigger; no warning is 0.
pub fn severity(w: Option<Reason>) -> u8 {
    match w {
        None => 0,
        Some(Reason::NameMismatch) => 1,
        Some(Reason::TempDir) => 2,
        Some(Reason::Deleted) => 3,
    }
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    /// Start time in ticks since boot; with `pid` it identifies one process even after PID reuse.
    pub start: u64,
    pub name: String,
    pub user: String,
    /// 100 = one full core (top style).
    pub cpu_pct: f32,
    /// Private resident memory (`RssAnon`): what GNOME System Monitor calls "Memory". Resident (`VmRSS`) also counts
    /// shared libraries once per process that maps them, so it overstates usage and double-counts in tree totals.
    pub mem_kb: u64,
    pub warn: Option<Reason>,
    /// `/proc/<pid>/stat` state letter; `T` = stopped, which the menu offers to resume.
    pub state: char,
    /// Parent PID, for the tree view.
    pub ppid: u32,
    /// Target of `/proc/<pid>/exe`; `None` for kernel threads and other users' processes. Used to find the icon.
    pub exe: Option<String>,
}

/// One physical disk's throughput in bytes per second.
#[derive(Clone, Debug, Default)]
pub struct DiskStats {
    pub name: String,
    pub read: f64,
    pub write: f64,
}

#[derive(Clone, Default, Debug)]
pub struct SysStats {
    pub cpu_pct: f32,
    /// Logical CPUs in `/proc/stat` (0 until the first sample); what `Proc::cpu_pct` is scaled by.
    pub cores: usize,
    /// Each logical CPU's load, 0..100.
    pub cores_pct: Vec<f32>,
    pub mem_used_kb: u64,
    pub mem_total_kb: u64,
    pub swap_used_kb: u64,
    /// 0 when the system has no swap.
    pub swap_total_kb: u64,
    /// Bytes per second.
    pub net_rx: f64,
    pub net_tx: f64,
    pub disks: Vec<DiskStats>,
    pub gpus: Vec<crate::gpu::GpuStats>,
    /// Kernel pressure (PSI) 10 s averages, 0..100: share of time tasks were stalled. 0 when the kernel has none.
    pub psi_cpu: f32,
    pub psi_mem: f32,
    pub psi_io: f32,
}

/// What is slowing the machine down, worst first (memory, then disk, then CPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Memory,
    Io,
    Cpu,
}

// Stall percentages above which the desktop is already noticeably slow; one place to tune them.
const PSI_MEM_LIMIT: f32 = 10.0;
const PSI_IO_LIMIT: f32 = 20.0;
const PSI_CPU_LIMIT: f32 = 60.0;
// Available memory (page cache counts as available) under this share of the total. The stall figures alone give
// almost no warning: measured on a 32 GB machine with 256 MB of swap, a fast allocation went from fine to
// OOM-killed with about 3 s of memory stall in total. Danger is where earlyoom starts killing; watch is the
// earlier point where the desktop still responds but opening one more big program may tip it over.
const MEM_DANGER_PCT: u64 = 10;
const MEM_WATCH_PCT: u64 = 20;
/// A CPU culprit must use at least half a core, so an idle process is never blamed for a busy system.
const CPU_CULPRIT_MIN: f32 = 50.0;
const MAX_CULPRITS: usize = 3;

/// What to warn about: `danger` is false for the early memory watch, true when the system is already stalling.
/// `culprits` are the likeliest processes, worst first (empty when none stands out, e.g. disk wait with no `D` process).
pub struct Alert<'a> {
    pub kind: Pressure,
    pub danger: bool,
    pub culprits: Vec<&'a Proc>,
}

/// The worst pressure right now (memory, then disk, then CPU) with up to three culprits: the biggest memory users,
/// the processes in disk wait (`D`) or the busiest ones.
pub fn pressure<'a>(s: &SysStats, procs: &'a [Proc]) -> Option<Alert<'a>> {
    let avail_under =
        |pct: u64| s.mem_total_kb > 0 && s.mem_total_kb.saturating_sub(s.mem_used_kb) * 100 < s.mem_total_kb * pct;
    let top = |mut v: Vec<&'a Proc>, key: fn(&Proc) -> f32| {
        v.sort_by(|a, b| key(b).total_cmp(&key(a)));
        v.truncate(MAX_CULPRITS);
        v
    };
    if s.psi_mem > PSI_MEM_LIMIT || avail_under(MEM_WATCH_PCT) {
        Some(Alert {
            kind: Pressure::Memory,
            danger: s.psi_mem > PSI_MEM_LIMIT || avail_under(MEM_DANGER_PCT),
            culprits: top(procs.iter().collect(), |p| p.mem_kb as f32),
        })
    } else if s.psi_io > PSI_IO_LIMIT {
        Some(Alert {
            kind: Pressure::Io,
            danger: true,
            culprits: top(procs.iter().filter(|p| p.state == 'D').collect(), |p| p.mem_kb as f32),
        })
    } else if s.psi_cpu > PSI_CPU_LIMIT {
        Some(Alert {
            kind: Pressure::Cpu,
            danger: true,
            culprits: top(procs.iter().filter(|p| p.cpu_pct >= CPU_CULPRIT_MIN).collect(), |p| {
                p.cpu_pct
            }),
        })
    } else {
        None
    }
}

pub struct Sample {
    pub procs: Vec<Proc>,
    pub sys: SysStats,
}

/// Keeps the previous counters so each `sample()` can turn them into rates. `new()` takes a priming reading,
/// so the first real `sample()` (a few hundred ms later) already has deltas.
pub struct Sampler {
    prev_ticks: HashMap<(u32, u64), u64>,
    prev_total: u64,
    prev_busy: u64,
    prev_net: HashMap<String, (u64, u64)>,
    prev_cores: Vec<(u64, u64)>,
    prev_disk: HashMap<String, (u64, u64)>,
    /// Opened after the priming reading, so NVML's start-up overlaps the first wait.
    gpu: Option<crate::gpu::Reader>,
    prev_at: Instant,
    users: HashMap<u32, String>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut s = Sampler {
            prev_ticks: HashMap::new(),
            prev_total: 0,
            prev_busy: 0,
            prev_net: HashMap::new(),
            prev_cores: Vec::new(),
            prev_disk: HashMap::new(),
            gpu: None,
            prev_at: Instant::now(),
            users: parse_passwd(&read("/etc/passwd")),
        };
        s.sample();
        s.gpu = Some(crate::gpu::Reader::new());
        s
    }

    pub fn sample(&mut self) -> Sample {
        let now = Instant::now();
        let secs = now.duration_since(self.prev_at).as_secs_f64().max(0.001);
        self.prev_at = now;

        let ((busy, total), core_ticks) = parse_cpu(&read("/proc/stat"));
        let cores = core_ticks.len().max(1);
        let d_total = total.saturating_sub(self.prev_total);
        let d_busy = busy.saturating_sub(self.prev_busy);
        self.prev_total = total;
        self.prev_busy = busy;
        // A changed core count (CPU hotplug) shifts the indices: start the per-core deltas over.
        let cores_pct: Vec<f32> = if self.prev_cores.len() == core_ticks.len() {
            core_ticks
                .iter()
                .zip(&self.prev_cores)
                .map(|(n, p)| pct(n.0.saturating_sub(p.0), n.1.saturating_sub(p.1), 1))
                .collect()
        } else {
            vec![0.0; core_ticks.len()]
        };
        self.prev_cores = core_ticks;

        let mut procs = Vec::with_capacity(self.prev_ticks.len());
        let mut ticks = HashMap::with_capacity(self.prev_ticks.len());
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let dir = format!("/proc/{pid}");
            // The process may exit between listing and reading: skip it silently on any error.
            let Some((name, t, start, state, ppid)) = parse_stat(&read(&format!("{dir}/stat"))) else {
                continue;
            };
            let Some((uid, mem_kb)) = parse_status(&read(&format!("{dir}/status"))) else {
                continue;
            };
            // EACCES for other users' processes and ENOENT for kernel threads both give None: no verdict.
            // Lossy, so a path with invalid UTF-8 (`/tmp/\xff/x`) is still judged.
            let exe = fs::read_link(format!("{dir}/exe"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let warn = suspicious(&name, exe.as_deref());
            let cpu_pct = self
                .prev_ticks
                .get(&(pid, start))
                .map_or(0.0, |p| pct(t.saturating_sub(*p), d_total, cores));
            ticks.insert((pid, start), t);
            // Not in /etc/passwd (LDAP, SSSD, homed, or added since start): ask NSS once; a miss is cached as the number.
            let user = self.users.entry(uid).or_insert_with(|| lookup_user(uid)).clone();
            procs.push(Proc {
                pid,
                start,
                name,
                user,
                cpu_pct,
                mem_kb,
                warn,
                state,
                ppid,
                exe,
            });
        }
        self.prev_ticks = ticks;

        let (mem_total_kb, avail, swap_total_kb, swap_free) = parse_meminfo(&read("/proc/meminfo"));
        // Only physical devices: loop, zram, dm-*, md*, bridges, veth and VPNs would double-count traffic.
        let net = parse_net_dev(&read("/proc/net/dev"), |n| {
            Path::new("/sys/class/net").join(n).join("device").exists()
        });
        let disk = parse_diskstats(&read("/proc/diskstats"), |n| {
            Path::new("/sys/block").join(n).join("device").exists()
        });
        let rate = |now: u64, prev: u64| now.saturating_sub(prev) as f64 / secs;
        let disks: Vec<DiskStats> = disk
            .iter()
            .map(|(name, r, w)| {
                // A disk seen for the first time has no previous reading: 0, not its lifetime total.
                let (pr, pw) = self.prev_disk.get(name).copied().unwrap_or((*r, *w));
                DiskStats {
                    name: name.clone(),
                    read: rate(*r, pr),
                    write: rate(*w, pw),
                }
            })
            .collect();
        self.prev_disk = disk.into_iter().map(|(n, r, w)| (n, (r, w))).collect();
        // Per adapter, like the disks: an adapter that appears (dock, USB) has no previous reading, so it counts 0
        // for this tick instead of its lifetime total.
        let (net_rx, net_tx) = net_rates(&self.prev_net, &net, secs);
        self.prev_net = net.into_iter().map(|(n, r, t)| (n, (r, t))).collect();
        let sys = SysStats {
            cpu_pct: pct(d_busy, d_total, 1),
            cores,
            cores_pct,
            mem_used_kb: mem_total_kb.saturating_sub(avail),
            mem_total_kb,
            swap_used_kb: swap_total_kb.saturating_sub(swap_free),
            swap_total_kb,
            net_rx,
            net_tx,
            disks,
            gpus: self.gpu.as_mut().map(|g| g.read()).unwrap_or_default(),
            psi_cpu: parse_psi(&read("/proc/pressure/cpu"), "some"),
            psi_mem: parse_psi(&read("/proc/pressure/memory"), "some"),
            psi_io: parse_psi(&read("/proc/pressure/io"), "full"),
        };
        Sample { procs, sys }
    }
}

/// Lossy: a process may rename itself to invalid UTF-8, and it must not vanish from the list for that.
/// Valid UTF-8 (nearly always) keeps the buffer as it is instead of copying it again.
fn read(path: &str) -> String {
    fs::read(path)
        .map(|b| String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
        .unwrap_or_default()
}

fn pct(delta: u64, delta_total: u64, cores: usize) -> f32 {
    if delta_total == 0 {
        return 0.0;
    }
    delta as f32 / delta_total as f32 * 100.0 * cores as f32
}

/// `avg10` of the `some` or `full` line of a `/proc/pressure/*` file; 0 when the file or line is missing.
fn parse_psi(text: &str, kind: &str) -> f32 {
    text.lines()
        .find_map(|l| {
            l.strip_prefix(kind)?
                .split_whitespace()
                .find_map(|f| f.strip_prefix("avg10="))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// `(name, fields after the name)`. The name sits between the first `(` and the LAST `)` because it may contain `) `.
/// In the fields, field 3 (state) is index 0.
fn split_stat(text: &str) -> Option<(&str, Vec<&str>)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    Some((
        text.get(open + 1..close)?,
        text.get(close + 1..)?.split_whitespace().collect(),
    ))
}

/// `(name, utime+stime, starttime, state, ppid)`.
fn parse_stat(text: &str) -> Option<(String, u64, u64, char, u32)> {
    let (name, rest) = split_stat(text)?;
    // utime = field 14 -> index 11, stime = 15 -> 12, starttime = 22 -> 19, ppid = 4 -> 1.
    let ticks = rest.get(11)?.parse::<u64>().ok()? + rest.get(12)?.parse::<u64>().ok()?;
    let start = rest.get(19)?.parse().ok()?;
    let ppid = rest.get(1)?.parse().ok()?;
    Some((name.to_string(), ticks, start, rest.first()?.chars().next()?, ppid))
}

/// A `/proc/<pid>/status` line's number in kB: `VmRSS:   2048 kB` -> 2048.
fn kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// `(real uid, RssAnon in kB)`; kernel threads have no memory lines.
fn parse_status(text: &str) -> Option<(u32, u64)> {
    let uid = text
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok());
    Some((uid?, kb(text, "RssAnon:").unwrap_or(0)))
}

/// `(resident, shared, virtual)` in kB for Properties. Shared is file-backed plus shared-memory pages, so that
/// resident - shared is the private part (`RssAnon`), as in GNOME System Monitor.
fn parse_mem(text: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let shared = match (kb(text, "RssFile:"), kb(text, "RssShmem:")) {
        (None, None) => None,
        (f, s) => Some(f.unwrap_or(0) + s.unwrap_or(0)),
    };
    (kb(text, "VmRSS:"), shared, kb(text, "VmSize:"))
}

/// `(busy, total)` of a `cpu` line's numbers. Total sums the first 8 fields (guest is already inside user);
/// busy = total - idle - iowait.
fn cpu_line(rest: &str) -> Option<(u64, u64)> {
    let f: Vec<u64> = rest.split_whitespace().take(8).filter_map(|x| x.parse().ok()).collect();
    (f.len() == 8).then(|| {
        let total: u64 = f.iter().sum();
        (total.saturating_sub(f[3] + f[4]), total)
    })
}

/// `(busy, total)` of the whole machine and of each `cpuN` line, from `/proc/stat`.
fn parse_cpu(text: &str) -> ((u64, u64), Vec<(u64, u64)>) {
    let mut all = (0, 0);
    let mut cores = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cpu") else { continue };
        let line = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let per_core = line.len() != rest.len();
        let v = cpu_line(line).unwrap_or((0, 0));
        if per_core {
            cores.push(v);
        } else {
            all = v;
        }
    }
    (all, cores)
}

/// `(MemTotal, MemAvailable, SwapTotal, SwapFree)` in kB.
fn parse_meminfo(text: &str) -> (u64, u64, u64, u64) {
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|x| x.parse().ok())
            .unwrap_or(0)
    };
    (
        get("MemTotal:"),
        get("MemAvailable:"),
        get("SwapTotal:"),
        get("SwapFree:"),
    )
}

/// `(name, rx bytes, tx bytes)` of each interface `keep` accepts.
fn parse_net_dev(text: &str, keep: impl Fn(&str) -> bool) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !keep(name) {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        out.push((name.to_string(), n(0), n(8)));
    }
    out
}

/// Total `(rx, tx)` bytes per second over `secs`. An adapter missing from `prev` is new: it counts 0, not its
/// lifetime total.
fn net_rates(prev: &HashMap<String, (u64, u64)>, now: &[(String, u64, u64)], secs: f64) -> (f64, f64) {
    now.iter().fold((0.0, 0.0), |(rx, tx), (name, r, t)| {
        let (pr, pt) = prev.get(name).copied().unwrap_or((*r, *t));
        (
            rx + r.saturating_sub(pr) as f64 / secs,
            tx + t.saturating_sub(pt) as f64 / secs,
        )
    })
}

/// `(name, bytes read, bytes written)` of each device `keep` accepts; sectors are always 512 bytes here.
fn parse_diskstats(text: &str, keep: impl Fn(&str) -> bool) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !keep(f[2]) {
            continue;
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0) * 512;
        out.push((f[2].to_string(), n(5), n(9)));
    }
    out
}

fn parse_passwd(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

/// User name of `uid` through NSS (`getent`, so no libc), or the number when unknown. `timeout` keeps a dead LDAP
/// server from freezing the sampler.
fn lookup_user(uid: u32) -> String {
    Command::new("timeout")
        .args(["2", "getent", "passwd", &uid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()
        .and_then(|o| parse_passwd(&String::from_utf8_lossy(&o.stdout)).remove(&uid))
        .unwrap_or_else(|| uid.to_string())
}

/// `exe` is the `/proc/<pid>/exe` target; `None` (no permission, kernel thread) never gets a verdict.
pub fn suspicious(name: &str, exe: Option<&str>) -> Option<Reason> {
    suspicious_in(name, exe, is_appimage_mount)
}

/// AppImages run from `/tmp/.mount_XXXX`, which is a mount of its own (FUSE). Any user can make a plain folder with
/// that name, so the exemption holds only when the folder sits on a different device than `/tmp`.
// shortcut: an unprivileged user can still FUSE-mount there (any AppImage does), check the filesystem type in
// /proc/self/mountinfo if that ever matters.
fn is_appimage_mount(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(rest) = path.strip_prefix("/tmp/.mount_") else {
        return false;
    };
    let dir = format!("/tmp/.mount_{}", rest.split('/').next().unwrap_or(""));
    match (fs::metadata(dir), fs::metadata("/tmp")) {
        (Ok(d), Ok(t)) => d.dev() != t.dev(),
        _ => false,
    }
}

/// `suspicious` with the mount check injected, so the rules can be tested without a real mount.
fn suspicious_in(name: &str, exe: Option<&str>, is_mount: impl Fn(&str) -> bool) -> Option<Reason> {
    let exe = exe?;
    let (path, deleted) = match exe.strip_suffix(" (deleted)") {
        Some(p) => (p, true),
        None => (exe, false),
    };
    // A package upgrade leaves running apps "deleted" under these; `/memfd:...` (code living only in RAM) stays flagged.
    if deleted
        && !["/usr/", "/opt/", "/snap/", "/app/"]
            .iter()
            .any(|p| path.starts_with(p))
    {
        return Some(Reason::Deleted);
    }
    // AppImages (this app too) run from a /tmp/.mount_* mount.
    if !is_mount(path) && ["/tmp/", "/var/tmp/", "/dev/shm/"].iter().any(|p| path.starts_with(p)) {
        return Some(Reason::TempDir);
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    let wrapper = matches!(base, "sh" | "bash" | "dash" | "zsh" | "perl" | "ruby" | "node" | "java")
        || base.starts_with("python")
        || base.starts_with("wine");
    // comm is cut at 15 chars, so a prefix match is a match.
    (!wrapper && !base.starts_with(name)).then_some(Reason::NameMismatch)
}

#[derive(Debug, PartialEq, Eq)]
pub enum KillError {
    /// The process is gone, or its PID now belongs to another process.
    Exited,
    /// The polkit dialog was dismissed or not authorized.
    Cancelled,
    /// PID 1, or stopping/killing this app itself.
    Refused,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    Term,
    Kill,
    Stop,
    Cont,
}

impl Signal {
    fn arg(self) -> &'static str {
        match self {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
            Signal::Stop => "-STOP",
            Signal::Cont => "-CONT",
        }
    }
}

/// How long the polkit dialog may stay open before it is cancelled.
const PKEXEC_WAIT_SECS: &str = "60";

/// Guards against PID reuse and the table re-sorting under the cursor.
/// shortcut: only checked before acting. With pkexec the dialog can stay open, and if the target exits meanwhile and
/// its PID is reused, root signals the new process. `PKEXEC_WAIT_SECS` caps that window (a forgotten dialog was the
/// realistic case). Closing it fully needs a root helper that re-checks and signals through a pidfd (pkexec drops open
/// file descriptors, so the app cannot hand one over); upgrade only if a wrong signal is ever seen.
fn same_process(pid: u32, start: u64) -> bool {
    parse_stat(&read(&format!("/proc/{pid}/stat"))).is_some_and(|(_, _, s, _, _)| s == start)
}

/// Runs `tool args` and retries it through pkexec when it is not allowed. Blocking (pkexec waits for the polkit
/// dialog): call it from a thread.
fn run_with_pkexec(tool: &str, args: &[String]) -> Result<(), KillError> {
    // LC_ALL=C: the error text below is matched in English, and the tools' messages are translated otherwise.
    let out = Command::new(tool)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| KillError::Failed(e.to_string()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if err.contains("No such process") {
        return Err(KillError::Exited);
    }
    // kill says "not permitted", renice says "Permission denied".
    if !(err.contains("not permitted") || err.contains("Permission denied")) {
        return Err(KillError::Failed(err));
    }
    let out = Command::new("timeout")
        .args([PKEXEC_WAIT_SECS, "pkexec"])
        .env("LC_ALL", "C")
        .arg(tool)
        .args(args)
        .output()
        .map_err(|e| KillError::Failed(e.to_string()))?;
    pkexec_result(out.status.code(), String::from_utf8_lossy(&out.stderr).trim())
}

/// Maps pkexec's exit code (or `timeout`'s). 126 is a dismissed dialog and 124 one left open for `PKEXEC_WAIT_SECS`;
/// 127 also means "no authentication agent", "not authorized" or a failed launch, so it stays a failure with pkexec's
/// own message.
fn pkexec_result(code: Option<i32>, err: &str) -> Result<(), KillError> {
    match code {
        Some(0) => Ok(()),
        Some(124 | 126) => Err(KillError::Cancelled),
        // The process can exit while the polkit dialog is open.
        _ if err.contains("No such process") => Err(KillError::Exited),
        _ => Err(KillError::Failed(err.to_string())),
    }
}

/// Sends `sig` to the process `(pid, start)`.
pub fn signal(pid: u32, start: u64, sig: Signal) -> Result<(), KillError> {
    // Stopping, ending or killing ourselves would freeze or close the window that asked (without saving its size).
    if pid == 1 || (pid == std::process::id() && matches!(sig, Signal::Term | Signal::Stop | Signal::Kill)) {
        return Err(KillError::Refused);
    }
    if !same_process(pid, start) {
        return Err(KillError::Exited);
    }
    run_with_pkexec("/usr/bin/kill", &[sig.arg().into(), pid.to_string()])
}

/// Sets the nice value (-20..=19) of the process `(pid, start)`. Raising the priority needs root, hence pkexec.
pub fn renice(pid: u32, start: u64, nice: i32) -> Result<(), KillError> {
    if !same_process(pid, start) {
        return Err(KillError::Exited);
    }
    let nice = nice.clamp(-20, 19);
    run_with_pkexec(
        "/usr/bin/renice",
        &["-n".into(), nice.to_string(), "-p".into(), pid.to_string()],
    )
}

/// What the Properties dialog shows. `None` fields could not be read (other users' processes, kernel threads).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub ppid: u32,
    pub state: char,
    pub threads: u32,
    pub nice: i32,
    /// Seconds since the process started.
    pub age_secs: u64,
    pub cmdline: Option<String>,
    pub exe: Option<String>,
    pub cwd: Option<String>,
    /// `(read_bytes, write_bytes)` since the process started; `/proc/<pid>/io` is owner-only.
    pub io: Option<(u64, u64)>,
    /// Memory in kB (`None` for kernel threads): resident (`VmRSS`), shared (`RssFile` + `RssShmem`), virtual
    /// (`VmSize`). The private part is `Proc::mem_kb`.
    pub resident_kb: Option<u64>,
    pub shared_kb: Option<u64>,
    pub virtual_kb: Option<u64>,
}

fn parse_io(text: &str) -> Option<(u64, u64)> {
    let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.trim().parse().ok());
    Some((get("read_bytes:")?, get("write_bytes:")?))
}

/// Arguments are NUL-separated; a process that blanked its own cmdline (or a kernel thread) gives `None`.
fn parse_cmdline(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes).replace('\0', " ");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Reads `/proc/<pid>` on demand (never on the sampling path). `None` when the process is gone or its PID was reused.
pub fn details(pid: u32, start: u64) -> Option<Details> {
    let dir = format!("/proc/{pid}");
    let stat = read(&format!("{dir}/stat"));
    let (_, rest) = split_stat(&stat)?;
    if parse_stat(&stat)?.2 != start {
        return None;
    }
    let field = |i: usize| rest.get(i).copied().unwrap_or("");
    let uptime: f64 = read("/proc/uptime")
        .split_whitespace()
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0.0);
    let link = |name: &str| {
        fs::read_link(format!("{dir}/{name}"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    };
    let (resident_kb, shared_kb, virtual_kb) = parse_mem(&read(&format!("{dir}/status")));
    Some(Details {
        // Fields 4 (ppid), 19 (nice) and 20 (threads) are indexes 1, 16 and 17.
        ppid: field(1).parse().unwrap_or(0),
        state: field(0).chars().next().unwrap_or('?'),
        threads: field(17).parse().unwrap_or(0),
        nice: field(16).parse().unwrap_or(0),
        // shortcut: assumes 100 ticks per second (true on every Linux desktop kernel), upgrade if one ever differs.
        age_secs: (uptime - start as f64 / 100.0).max(0.0) as u64,
        cmdline: fs::read(format!("{dir}/cmdline")).ok().and_then(|b| parse_cmdline(&b)),
        exe: link("exe"),
        cwd: link("cwd"),
        io: parse_io(&read(&format!("{dir}/io"))),
        resident_kb,
        shared_kb,
        virtual_kb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child forked while another test still holds a freshly copied binary open for writing makes that binary's
    /// exec fail with ETXTBSY, so every test that spawns takes this lock around its copy + spawn.
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
        SPAWN.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn stat_with_paren_in_name() {
        let line = "42 (we) ird) S 1 42 42 0 -1 4194560 1 0 0 0 7 5 0 0 20 0 1 0 12345 1000 10 18446744073709551615";
        assert_eq!(parse_stat(line), Some(("we) ird".into(), 12, 12345, 'S', 1)));
    }

    #[test]
    fn status_without_memory_lines_is_zero() {
        assert_eq!(parse_status("Name:\tkthreadd\nUid:\t0\t0\t0\t0\n"), Some((0, 0)));
        // Memory is the private part, not VmRSS.
        assert_eq!(
            parse_status("Uid:\t1000\t0\t0\t0\nVmRSS:\t  4096 kB\nRssAnon:\t  2048 kB\n"),
            Some((1000, 2048))
        );
    }

    #[test]
    fn properties_memory_is_resident_shared_and_virtual() {
        // The numbers of GNOME System Monitor's screenshot: resident - shared = private.
        let text = "VmSize:\t 1500000 kB\nVmRSS:\t  405200 kB\nRssAnon:\t 282000 kB\nRssFile:\t 100000 kB\nRssShmem:\t  23200 kB\n";
        assert_eq!(parse_mem(text), (Some(405_200), Some(123_200), Some(1_500_000)));
        assert_eq!(parse_mem("Name:\tkthreadd\n"), (None, None, None));
    }

    #[test]
    fn cpu_total_skips_guest_and_counts_cores() {
        let text = "cpu  10 0 10 70 10 0 0 0 5 0\ncpu0 5 0 5 35 5 0 0 0 0 0\ncpu1 5 0 5 35 5 0 0 0 0 0\n";
        // busy = 100 - 70 idle - 10 iowait
        assert_eq!(parse_cpu(text), ((20, 100), vec![(10, 50), (10, 50)]));
    }

    #[test]
    fn cpu_percent_is_per_core_and_safe_on_zero() {
        assert_eq!(pct(50, 100, 4), 200.0);
        assert_eq!(pct(5, 0, 4), 0.0);
    }

    #[test]
    fn meminfo() {
        assert_eq!(
            parse_meminfo(
                "MemTotal:  1000 kB\nMemFree: 1 kB\nMemAvailable:  400 kB\nSwapTotal: 200 kB\nSwapFree: 150 kB\n"
            ),
            (1000, 400, 200, 150)
        );
    }

    #[test]
    fn psi_reads_avg10_and_defaults_to_zero() {
        let text = "some avg10=1.50 avg60=0.16 avg300=0.47 total=1\nfull avg10=12.25 avg60=0.16 avg300=0.41 total=2\n";
        assert_eq!(parse_psi(text, "some"), 1.5);
        assert_eq!(parse_psi(text, "full"), 12.25);
        assert_eq!(parse_psi("", "full"), 0.0);
        assert_eq!(parse_psi("some avg10=oops", "some"), 0.0);
    }

    #[test]
    fn pressure_names_the_culprit() {
        let p = |pid, cpu_pct, mem_kb, state| Proc {
            pid,
            start: 0,
            name: format!("p{pid}"),
            user: String::new(),
            cpu_pct,
            mem_kb,
            warn: None,
            state,
            ppid: 1,
            exe: None,
        };
        let procs = [
            p(1, 90.0, 10, 'S'),
            p(2, 5.0, 500, 'S'),
            p(3, 0.0, 1, 'D'),
            p(4, 60.0, 300, 'S'),
            p(5, 0.0, 200, 'S'),
        ];
        // (kind, danger, culprit PIDs) of the alert, if any.
        let alert = |s: &SysStats, procs: &[Proc]| {
            pressure(s, procs).map(|a| (a.kind, a.danger, a.culprits.iter().map(|p| p.pid).collect::<Vec<_>>()))
        };
        let mut s = SysStats::default();
        assert!(alert(&s, &procs).is_none());
        // CPU: only processes using at least half a core are named, busiest first.
        s.psi_cpu = 80.0;
        assert_eq!(alert(&s, &procs), Some((Pressure::Cpu, true, vec![1, 4])));
        s.psi_io = 30.0;
        assert_eq!(alert(&s, &procs), Some((Pressure::Io, true, vec![3])));
        // Memory outranks the rest and names the three biggest users, not the busiest.
        s.psi_mem = 15.0;
        assert_eq!(alert(&s, &procs), Some((Pressure::Memory, true, vec![2, 4, 5])));
        // Little memory left warns with no stall at all: watch under 20% available, danger under 10%.
        s.psi_mem = 0.0;
        s.psi_io = 0.0;
        s.psi_cpu = 0.0;
        s.mem_total_kb = 1000;
        s.mem_used_kb = 700;
        assert!(alert(&s, &procs).is_none());
        s.mem_used_kb = 850;
        assert_eq!(alert(&s, &procs), Some((Pressure::Memory, false, vec![2, 4, 5])));
        s.mem_used_kb = 950;
        assert_eq!(alert(&s, &procs), Some((Pressure::Memory, true, vec![2, 4, 5])));
        // Disk wait with no process in `D` still warns, without a name.
        s.mem_total_kb = 0;
        s.psi_io = 30.0;
        assert_eq!(alert(&s, &procs[..2]), Some((Pressure::Io, true, vec![])));
    }

    #[test]
    fn net_dev_keeps_only_accepted_interfaces() {
        let text = "Inter-|   Receive\n face |bytes\n    lo: 500 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0\n  eth0: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(text, |n| n == "eth0"), [("eth0".to_string(), 100, 200)]);
    }

    #[test]
    fn a_new_adapter_counts_zero_for_its_first_tick() {
        let prev = HashMap::from([("eth0".to_string(), (100, 200))]);
        // eth0 moved 50 / 100 bytes in 2 s; usb0 appears with 20 GB of lifetime traffic.
        let now = [
            ("eth0".to_string(), 150, 300),
            ("usb0".to_string(), 20_000_000_000, 20_000_000_000),
        ];
        assert_eq!(net_rates(&prev, &now, 2.0), (25.0, 50.0));
    }

    #[test]
    fn diskstats_uses_512_byte_sectors_and_filters() {
        let text = "8 0 sda 1 0 10 0 1 0 20 0 0 0 0\n8 1 sda1 1 0 99 0 1 0 99 0 0 0 0\n";
        assert_eq!(
            parse_diskstats(text, |n| n == "sda"),
            vec![("sda".to_string(), 5120, 10240)]
        );
        let both = parse_diskstats(text, |_| true);
        assert_eq!(both.len(), 2);
        assert_eq!(both[1], ("sda1".to_string(), 99 * 512, 99 * 512));
    }

    #[test]
    fn passwd() {
        assert_eq!(
            parse_passwd("root:x:0:0:r:/root:/bin/sh\n").get(&0).map(String::as_str),
            Some("root")
        );
    }

    #[test]
    fn suspicious_rules() {
        let s = |name, exe| suspicious_in(name, exe, |p: &str| p.starts_with("/tmp/.mount_"));
        assert_eq!(s("x", None), None);
        assert_eq!(s("evil", Some("/home/u/evil (deleted)")), Some(Reason::Deleted));
        assert_eq!(s("memfd", Some("/memfd:memfd (deleted)")), Some(Reason::Deleted));
        assert_eq!(s("firefox", Some("/usr/lib/firefox/firefox (deleted)")), None);
        assert_eq!(s("x", Some("/tmp/x")), Some(Reason::TempDir));
        assert_eq!(s("x", Some("/dev/shm/x")), Some(Reason::TempDir));
        assert_eq!(
            s("fastprocman", Some("/tmp/.mount_FastPrXYZ/usr/bin/fastprocman")),
            None
        );
        assert_eq!(
            s("Web Content", Some("/usr/lib/firefox/firefox")),
            Some(Reason::NameMismatch)
        );
        // comm is cut at 15 chars
        assert_eq!(s("systemd-journal", Some("/usr/lib/systemd/systemd-journald")), None);
        assert_eq!(s("myscript.py", Some("/usr/bin/python3.12")), None);
    }

    #[test]
    fn a_plain_folder_named_like_an_appimage_mount_is_still_flagged() {
        let dir = format!("/tmp/.mount_fastprocman-test-{}", std::process::id());
        fs::create_dir_all(&dir).unwrap();
        let exe = format!("{dir}/evil");
        let flagged = suspicious("evil", Some(&exe));
        let mount = is_appimage_mount(&exe);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(flagged, Some(Reason::TempDir));
        assert!(!mount);
        // Not under /tmp/.mount_ at all.
        assert!(!is_appimage_mount("/usr/bin/x"));
    }

    #[test]
    fn name_mismatch_is_not_strong() {
        assert!(Reason::Deleted.is_strong() && Reason::TempDir.is_strong() && !Reason::NameMismatch.is_strong());
    }

    /// The first stage of `run_with_pkexec` maps the tool's stderr; the pkexec retry ("not permitted") needs a real
    /// polkit agent and stays on the manual list.
    #[test]
    fn tool_errors_map_to_exited_or_failed() {
        let _g = spawn_lock();
        let sh = |msg: &str| run_with_pkexec("/bin/sh", &["-c".into(), format!("echo '{msg}' >&2; exit 1")]);
        assert_eq!(sh("kill: No such process"), Err(KillError::Exited));
        assert_eq!(sh("boom"), Err(KillError::Failed("boom".into())));
    }

    #[test]
    fn pkexec_exit_codes_map_to_results() {
        assert_eq!(pkexec_result(Some(0), ""), Ok(()));
        assert_eq!(pkexec_result(Some(126), ""), Err(KillError::Cancelled));
        // A dialog left open too long: `timeout` ends pkexec with 124.
        assert_eq!(pkexec_result(Some(124), ""), Err(KillError::Cancelled));
        // No polkit agent: must not look like a cancel.
        assert_eq!(
            pkexec_result(Some(127), "No authentication agent found."),
            Err(KillError::Failed("No authentication agent found.".into()))
        );
        assert_eq!(pkexec_result(Some(1), "kill: No such process"), Err(KillError::Exited));
        assert_eq!(pkexec_result(None, "x"), Err(KillError::Failed("x".into())));
    }

    /// The cap relies on `timeout` ending its command with exit code 124.
    #[test]
    fn a_command_over_the_time_limit_exits_with_124() {
        let _g = spawn_lock();
        let code = Command::new("timeout")
            .args(["0.2", "sleep", "5"])
            .status()
            .unwrap()
            .code();
        assert_eq!(pkexec_result(code, ""), Err(KillError::Cancelled));
    }

    #[test]
    fn unknown_uid_falls_back_to_a_number_and_root_is_found() {
        let _g = spawn_lock();
        assert_eq!(lookup_user(0), "root");
        assert_eq!(lookup_user(4_000_000_123), "4000000123");
    }

    #[test]
    fn signal_refuses_pid_1_ourselves_and_stale_start() {
        assert_eq!(signal(1, 0, Signal::Term), Err(KillError::Refused));
        let me = std::process::id();
        // Ending, stopping or killing this app is refused even with the right start time.
        let start = parse_stat(&read(&format!("/proc/{me}/stat"))).unwrap().2;
        assert_eq!(signal(me, start, Signal::Term), Err(KillError::Refused));
        assert_eq!(signal(me, start, Signal::Stop), Err(KillError::Refused));
        assert_eq!(signal(me, start, Signal::Kill), Err(KillError::Refused));
        // A wrong start time must never be signalled.
        assert_eq!(signal(me, 0, Signal::Cont), Err(KillError::Exited));
        assert_eq!(renice(me, 0, 0), Err(KillError::Exited));
    }

    #[test]
    fn signal_ends_a_real_child_with_term_then_reports_it_gone() {
        let mut child = {
            let _g = spawn_lock();
            Command::new("sleep").arg("60").spawn().unwrap()
        };
        let pid = child.id();
        let start = parse_stat(&read(&format!("/proc/{pid}/stat"))).unwrap().2;
        assert_eq!(signal(pid, start, Signal::Term), Ok(()));
        let status = child.wait().unwrap();
        assert!(!status.success());
        // Reaped: the same (pid, start) must now read as gone, not be signalled again.
        assert_eq!(signal(pid, start, Signal::Kill), Err(KillError::Exited));
    }

    #[test]
    fn stop_and_cont_change_the_state_and_renice_lowers_priority() {
        let mut child = {
            let _g = spawn_lock();
            Command::new("sleep").arg("60").spawn().unwrap()
        };
        let pid = child.id();
        let (_, _, start, _, _) = parse_stat(&read(&format!("/proc/{pid}/stat"))).unwrap();
        let state = || parse_stat(&read(&format!("/proc/{pid}/stat"))).unwrap().3;
        assert_eq!(signal(pid, start, Signal::Stop), Ok(()));
        // The state flips once the kernel delivers the signal; wait for it briefly.
        for _ in 0..50 {
            if state() == 'T' {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(state(), 'T');
        assert_eq!(signal(pid, start, Signal::Cont), Ok(()));
        // Lowering priority never needs root.
        assert_eq!(renice(pid, start, 10), Ok(()));
        assert_eq!(details(pid, start).map(|d| d.nice), Some(10));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(details(pid, start), None);
    }

    #[test]
    fn details_of_this_process() {
        let me = std::process::id();
        let start = parse_stat(&read(&format!("/proc/{me}/stat"))).unwrap().2;
        let d = details(me, start).unwrap();
        assert!(d.threads >= 1 && d.ppid > 0);
        assert!(d.cmdline.is_some() && d.exe.is_some() && d.cwd.is_some());
        assert_eq!(details(me, 0), None);
    }

    #[test]
    fn read_keeps_valid_text_and_repairs_invalid_utf8() {
        let dir = std::env::temp_dir().join(format!("fpm-read-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good"), dir.join("bad"));
        fs::write(&good, "7 (é) S\n").unwrap();
        fs::write(&bad, b"7 (a\xffb) S\n").unwrap();
        let got = (
            read(good.to_str().unwrap()),
            read(bad.to_str().unwrap()),
            read("/nonexistent/x"),
        );
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(got, ("7 (é) S\n".into(), "7 (a\u{fffd}b) S\n".into(), String::new()));
    }

    #[test]
    fn io_and_cmdline_parsing() {
        assert_eq!(
            parse_io("rchar: 1\nread_bytes: 4096\nwrite_bytes: 8192\n"),
            Some((4096, 8192))
        );
        assert_eq!(parse_io(""), None);
        assert_eq!(
            parse_cmdline(b"/bin/sh\0-c\0echo hi\0"),
            Some("/bin/sh -c echo hi".into())
        );
        assert_eq!(parse_cmdline(b""), None);
    }

    #[test]
    fn live_sample_flags_a_program_running_from_tmp() {
        let exe = format!("/tmp/fastprocman-test-{}", std::process::id());
        let mut child = {
            let _g = spawn_lock();
            fs::copy("/usr/bin/sleep", &exe).unwrap();
            Command::new(&exe).arg("30").spawn().unwrap()
        };
        let sample = Sampler::new().sample();
        let found = sample.procs.iter().find(|p| p.pid == child.id()).map(|p| p.warn);
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_file(&exe).unwrap();
        assert_eq!(found, Some(Some(Reason::TempDir)));
    }

    #[test]
    fn live_sample_keeps_and_flags_a_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        // A lone 0xff byte is not valid UTF-8.
        let mut bytes = b"/tmp/fastprocman-".to_vec();
        bytes.push(0xff);
        bytes.extend(std::process::id().to_string().bytes());
        let exe = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
        let mut child = {
            let _g = spawn_lock();
            fs::copy("/usr/bin/sleep", &exe).unwrap();
            Command::new(&exe).arg("30").spawn().unwrap()
        };
        let sample = Sampler::new().sample();
        let found = sample.procs.iter().find(|p| p.pid == child.id()).map(|p| p.warn);
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_file(&exe).unwrap();
        assert_eq!(found, Some(Some(Reason::TempDir)));
    }

    #[test]
    fn live_sample_sees_this_process() {
        let _g = spawn_lock();
        let mut s = Sampler::new();
        let sample = s.sample();
        assert!(sample.procs.iter().any(|p| p.pid == std::process::id()));
        assert!(sample.sys.mem_total_kb > 0);
    }
}
