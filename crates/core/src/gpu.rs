//! GPU load and video memory. NVIDIA through NVML, loaded at run time with `dlopen` (no crate, no link-time
//! dependency: a machine without the driver simply has no GPU card); AMD through sysfs; Intel (i915 and xe) through the
//! DRM client counters in `/proc/<pid>/fdinfo`, load only (an integrated card has no video memory of its own).
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    fs,
    path::Path,
    time::Instant,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuStats {
    pub name: String,
    /// 0..100.
    pub busy: f32,
    /// Video encoder and decoder load, 0..100; `None` when the driver does not report it (AMD).
    pub enc: Option<f32>,
    pub dec: Option<f32>,
    /// Bytes.
    pub mem_used: u64,
    pub mem_total: u64,
}

// glibc 2.34+ has these in libc itself, which std already links.
unsafe extern "C" {
    fn dlopen(file: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}
const RTLD_NOW: c_int = 2;
const DRM: &str = "/sys/class/drm";

type Dev = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct Util {
    gpu: c_uint,
    memory: c_uint,
}
#[repr(C)]
#[derive(Default)]
struct Mem {
    total: u64,
    free: u64,
    used: u64,
}

struct Nvml {
    util: unsafe extern "C" fn(Dev, *mut Util) -> c_int,
    enc: unsafe extern "C" fn(Dev, *mut c_uint, *mut c_uint) -> c_int,
    dec: unsafe extern "C" fn(Dev, *mut c_uint, *mut c_uint) -> c_int,
    mem: unsafe extern "C" fn(Dev, *mut Mem) -> c_int,
    devs: Vec<(Dev, String)>,
}

impl Nvml {
    fn open() -> Option<Nvml> {
        // SAFETY: each symbol is cast to the signature NVML documents for it; the library is never unloaded.
        unsafe {
            let lib = dlopen(c"libnvidia-ml.so.1".as_ptr(), RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            // `T` is always a function pointer, the same size as the `*mut c_void` dlsym returns.
            let sym = |name: &CStr| {
                let p = dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            macro_rules! f {
                ($name:literal) => {
                    std::mem::transmute_copy(&sym($name)?)
                };
            }
            let init: unsafe extern "C" fn() -> c_int = f!(c"nvmlInit_v2");
            let count: unsafe extern "C" fn(*mut c_uint) -> c_int = f!(c"nvmlDeviceGetCount_v2");
            let handle: unsafe extern "C" fn(c_uint, *mut Dev) -> c_int = f!(c"nvmlDeviceGetHandleByIndex_v2");
            let name: unsafe extern "C" fn(Dev, *mut c_char, c_uint) -> c_int = f!(c"nvmlDeviceGetName");
            let mut nvml = Nvml {
                util: f!(c"nvmlDeviceGetUtilizationRates"),
                enc: f!(c"nvmlDeviceGetEncoderUtilization"),
                dec: f!(c"nvmlDeviceGetDecoderUtilization"),
                mem: f!(c"nvmlDeviceGetMemoryInfo"),
                devs: Vec::new(),
            };
            if init() != 0 {
                return None;
            }
            let mut n = 0;
            if count(&mut n) != 0 {
                return None;
            }
            for i in 0..n {
                let mut d: Dev = std::ptr::null_mut();
                if handle(i, &mut d) != 0 {
                    continue;
                }
                let mut buf = [0 as c_char; 96];
                let label = if name(d, buf.as_mut_ptr(), buf.len() as c_uint) == 0 {
                    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
                } else {
                    format!("GPU {i}")
                };
                nvml.devs.push((d, label));
            }
            (!nvml.devs.is_empty()).then_some(nvml)
        }
    }

    fn read(&self) -> Vec<GpuStats> {
        self.devs
            .iter()
            .map(|(d, name)| {
                let (mut u, mut m) = (Util::default(), Mem::default());
                let (mut e, mut dc, mut period) = (0, 0, 0);
                // SAFETY: valid handle and out-pointers; a failed call leaves its zero (or None).
                unsafe {
                    let ok_u = (self.util)(*d, &mut u) == 0;
                    let ok_e = (self.enc)(*d, &mut e, &mut period) == 0;
                    let ok_d = (self.dec)(*d, &mut dc, &mut period) == 0;
                    let _ = (self.mem)(*d, &mut m);
                    GpuStats {
                        name: name.clone(),
                        busy: if ok_u { u.gpu as f32 } else { 0.0 },
                        enc: ok_e.then_some(e as f32),
                        dec: ok_d.then_some(dc as f32),
                        mem_used: m.used,
                        mem_total: m.total,
                    }
                }
            })
            .collect()
    }
}

pub struct Reader {
    nvml: Option<Nvml>,
    /// Returned (with the load zeroed) while the NVIDIA card is asleep, so it is not woken every second.
    last: Vec<GpuStats>,
    /// The AMD cards of the previous reading, for the same purpose.
    last_amd: Vec<GpuStats>,
    intel: Intel,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    pub fn new() -> Reader {
        Reader {
            nvml: Nvml::open(),
            last: Vec::new(),
            last_amd: Vec::new(),
            intel: Intel::new(Path::new(DRM)),
        }
    }

    pub fn read(&mut self) -> Vec<GpuStats> {
        let mut out = Vec::new();
        if let Some(n) = &self.nvml {
            if asleep(Path::new(DRM), "0x10de") {
                out.extend(self.last.iter().cloned().map(|mut g| {
                    g.busy = 0.0;
                    g.enc = g.enc.map(|_| 0.0);
                    g.dec = g.dec.map(|_| 0.0);
                    g
                }));
            } else {
                self.last = n.read();
                out.extend(self.last.iter().cloned());
            }
        }
        self.last_amd = read_amd(Path::new(DRM), &self.last_amd);
        out.extend(self.last_amd.iter().cloned());
        out.extend(self.intel.read(Path::new("/proc")));
        out
    }
}

/// `(drm-pdev, drm-client-id, engine class)`.
type Key = (String, u64, String);
/// Engine time used so far: ns (i915) or cycles (xe), and xe's `drm-total-cycles` for the same class.
type Count = (u64, Option<u64>);

/// Intel cards: no load file in sysfs, so the load is the sum, over every DRM client we are allowed to see, of the
/// engine time it has used since the last tick. `intel_gpu_top` reads the same thing through perf, which needs root.
struct Intel {
    /// `(N of cardN, PCI address as fdinfo's drm-pdev spells it)`, listed once: empty on every other machine, and then
    /// `/proc` is never scanned.
    cards: Vec<(u32, String)>,
    prev: HashMap<Key, Count>,
    at: Option<Instant>,
}

impl Intel {
    fn new(drm: &Path) -> Intel {
        let name = |p: std::path::PathBuf| fs::read_link(p).ok()?.file_name()?.to_str().map(String::from);
        let mut cards: Vec<_> = fs::read_dir(drm)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let idx: u32 = e.file_name().to_str()?.strip_prefix("card")?.parse().ok()?;
                let dev = e.path().join("device");
                matches!(name(dev.join("driver"))?.as_str(), "i915" | "xe").then_some(())?;
                Some((idx, name(dev)?))
            })
            .collect();
        cards.sort();
        Intel {
            cards,
            prev: HashMap::new(),
            at: None,
        }
    }

    fn read(&mut self, proc: &Path) -> Vec<GpuStats> {
        if self.cards.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let wall = self.at.replace(now).map_or(0, |t| (now - t).as_nanos() as u64);
        self.step(proc, wall)
    }

    /// `wall` is the nanoseconds since the previous reading; 0 on the first one, which therefore reads 0 load.
    fn step(&mut self, proc: &Path, wall: u64) -> Vec<GpuStats> {
        let cur = scan(proc);
        let out = self
            .cards
            .iter()
            .map(|(idx, pdev)| {
                // Per engine class: (time used by all clients, time available). A client first seen this tick has no
                // previous count and adds nothing, like a new network adapter.
                let mut classes: HashMap<&str, (u64, u64)> = HashMap::new();
                for (k, (v, total)) in cur.iter().filter(|(k, _)| k.0 == *pdev) {
                    let Some((pv, ptotal)) = self.prev.get(k) else { continue };
                    let c = classes.entry(&k.2).or_default();
                    c.0 += v.saturating_sub(*pv);
                    let avail = match (total, ptotal) {
                        (Some(a), Some(b)) => a.saturating_sub(*b),
                        _ => wall,
                    };
                    c.1 = c.1.max(avail);
                }
                // The busiest class stands for the card (render is what a desktop uses; video would be one more).
                let busy = classes
                    .values()
                    .filter(|c| c.1 > 0)
                    .map(|c| c.0 as f64 / c.1 as f64 * 100.0);
                GpuStats {
                    name: format!("GPU {idx}"),
                    busy: busy.fold(0.0, f64::max).min(100.0) as f32,
                    ..Default::default()
                }
            })
            .collect();
        self.prev = cur;
        out
    }
}

/// Every DRM client of every process we can read. A client shared by dup'd or inherited fds has one key, so it is
/// counted once. shortcut: other users' processes are unreadable and missing from the load; a root helper would fix it.
fn scan(proc: &Path) -> HashMap<Key, Count> {
    let mut out = HashMap::new();
    for p in fs::read_dir(proc).into_iter().flatten().flatten() {
        if !p
            .file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        for fd in fs::read_dir(p.path().join("fd")).into_iter().flatten().flatten() {
            if !fs::read_link(fd.path()).is_ok_and(|l| l.starts_with("/dev/dri/")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(p.path().join("fdinfo").join(fd.file_name())) else {
                continue;
            };
            if let Some((pdev, client, classes)) = parse_fdinfo(&text) {
                for (class, count) in classes {
                    out.insert((pdev.clone(), client, class), count);
                }
            }
        }
    }
    out
}

/// i915: `drm-engine-<class>: <ns> ns`. xe: `drm-cycles-<class>: <n>` with `drm-total-cycles-<class>: <n>`.
/// `drm-engine-capacity-<class>` is a count of engines, not a time.
fn parse_fdinfo(text: &str) -> Option<(String, u64, BTreeMap<String, Count>)> {
    let (mut pdev, mut client, mut classes) = (None, None, BTreeMap::<String, Count>::new());
    for (k, v) in text.lines().filter_map(|l| l.split_once(':')) {
        let n = || v.split_whitespace().next()?.parse::<u64>().ok();
        match k {
            "drm-pdev" => pdev = Some(v.trim().to_string()),
            "drm-client-id" => client = n(),
            _ => {
                if let Some(c) = k.strip_prefix("drm-total-cycles-") {
                    classes.entry(c.into()).or_default().1 = n();
                } else if let Some(c) = k.strip_prefix("drm-cycles-").or(k.strip_prefix("drm-engine-"))
                    && !c.starts_with("capacity-")
                {
                    classes.entry(c.into()).or_default().0 = n()?;
                }
            }
        }
    }
    Some((pdev?, client?, classes))
}

/// True when there is at least one card of `vendor` and every one is runtime-suspended.
fn asleep(drm: &Path, vendor: &str) -> bool {
    let mut any = false;
    for e in fs::read_dir(drm).into_iter().flatten().flatten() {
        let dev = e.path().join("device");
        let Ok(v) = fs::read_to_string(dev.join("vendor")) else {
            continue;
        };
        if v.trim() != vendor {
            continue;
        }
        any = true;
        // An unreadable status counts as awake: better to read the card than to hide it forever.
        if !fs::read_to_string(dev.join("power/runtime_status")).is_ok_and(|s| s.trim() == "suspended") {
            return false;
        }
    }
    any
}

fn num(path: std::path::PathBuf) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Cards whose `device/gpu_busy_percent` exists (amdgpu). `cardN` only: `cardN-DP-1` are connectors. A suspended card
/// is not read (that would wake it) but kept, idle and with its memory from `last`: dropping it would make the card
/// come and go on the Graphs page and shift the history of the others, which is matched by position.
fn read_amd(drm: &Path, last: &[GpuStats]) -> Vec<GpuStats> {
    let mut cards: Vec<_> = fs::read_dir(drm)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            let idx: u32 = n.strip_prefix("card")?.parse().ok()?;
            Some((idx, e.path().join("device")))
        })
        .collect();
    cards.sort();
    cards
        .into_iter()
        .filter_map(|(idx, dev)| {
            let name = format!("GPU {idx}");
            if fs::read_to_string(dev.join("power/runtime_status")).is_ok_and(|s| s.trim() == "suspended") {
                // `exists` is a stat, which does not wake the card; other vendors' cards have no such file.
                return dev.join("gpu_busy_percent").exists().then(|| GpuStats {
                    busy: 0.0,
                    ..last.iter().find(|g| g.name == name).cloned().unwrap_or(GpuStats {
                        name,
                        ..Default::default()
                    })
                });
            }
            Some(GpuStats {
                name,
                busy: num(dev.join("gpu_busy_percent"))? as f32,
                enc: None,
                dec: None,
                mem_used: num(dev.join("mem_info_vram_used")).unwrap_or(0),
                mem_total: num(dev.join("mem_info_vram_total")).unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amd_reads_sysfs_and_skips_connectors_and_other_vendors() {
        let root = std::env::temp_dir().join(format!("fpm-drm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = root.join("card0/device");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(root.join("card0-DP-1/device")).unwrap();
        fs::create_dir_all(root.join("card1/device")).unwrap(); // NVIDIA: no gpu_busy_percent
        fs::write(dev.join("gpu_busy_percent"), "42\n").unwrap();
        fs::write(dev.join("mem_info_vram_used"), "100\n").unwrap();
        fs::write(dev.join("mem_info_vram_total"), "400\n").unwrap();
        let got = read_amd(&root, &[]);
        // Asleep: still listed (so the card does not come and go), idle, with the memory seen last time.
        fs::create_dir_all(dev.join("power")).unwrap();
        fs::write(dev.join("power/runtime_status"), "suspended\n").unwrap();
        let asleep = read_amd(&root, &got);
        let first_seen_asleep = read_amd(&root, &[]);
        let _ = fs::remove_dir_all(&root);
        let awake = GpuStats {
            name: "GPU 0".into(),
            busy: 42.0,
            enc: None,
            dec: None,
            mem_used: 100,
            mem_total: 400,
        };
        assert_eq!(got, vec![awake.clone()]);
        assert_eq!(asleep, vec![GpuStats { busy: 0.0, ..awake }]);
        assert_eq!(first_seen_asleep.len(), 1);
        assert_eq!(first_seen_asleep[0].mem_total, 0);
    }

    #[test]
    fn asleep_needs_a_card_and_all_suspended() {
        let root = std::env::temp_dir().join(format!("fpm-sleep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(!asleep(&root, "0x10de"));
        let dev = root.join("card1/device");
        fs::create_dir_all(dev.join("power")).unwrap();
        fs::write(dev.join("vendor"), "0x10de\n").unwrap();
        fs::write(dev.join("power/runtime_status"), "suspended\n").unwrap();
        let sleeping = asleep(&root, "0x10de");
        fs::write(dev.join("power/runtime_status"), "active\n").unwrap();
        let active = asleep(&root, "0x10de");
        fs::remove_dir_all(dev.join("power")).unwrap();
        let unreadable = asleep(&root, "0x10de");
        let _ = fs::remove_dir_all(&root);
        assert!(sleeping && !active && !unreadable);
    }

    #[test]
    fn intel_load_from_fdinfo_counts_a_client_once_and_ignores_other_files() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("fpm-intel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let pci = root.join("pci/0000:00:02.0");
        fs::create_dir_all(&pci).unwrap();
        symlink("/x/drivers/i915", pci.join("driver")).unwrap();
        fs::create_dir_all(root.join("drm/card0-eDP-1")).unwrap();
        fs::create_dir_all(root.join("drm/card0")).unwrap();
        symlink(&pci, root.join("drm/card0/device")).unwrap();
        let proc = root.join("proc");
        // pid 10 has the client twice (fd 3 and a dup, fd 5) and an unrelated file with the same counters (fd 4).
        for (fd, link) in [(3, "/dev/dri/renderD128"), (5, "/dev/dri/card0"), (4, "/dev/null")] {
            fs::create_dir_all(proc.join("10/fd")).unwrap();
            fs::create_dir_all(proc.join("10/fdinfo")).unwrap();
            symlink(link, proc.join(format!("10/fd/{fd}"))).unwrap();
        }
        let info = |ns: u64| {
            format!(
                "drm-pdev:\t0000:00:02.0\ndrm-client-id:\t7\ndrm-engine-render:\t{ns} ns\ndrm-engine-capacity-render:\t1\n"
            )
        };
        let write = |ns: u64| {
            for fd in [3, 4, 5] {
                fs::write(proc.join(format!("10/fdinfo/{fd}")), info(ns)).unwrap();
            }
        };
        let mut intel = Intel::new(&root.join("drm"));
        assert_eq!(intel.cards, vec![(0, "0000:00:02.0".to_string())]);
        write(1_000);
        assert_eq!(intel.step(&proc, 0)[0].busy, 0.0); // first reading: nothing to subtract yet
        write(1_000 + 250_000_000);
        let got = intel.step(&proc, 1_000_000_000);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(got[0].name, "GPU 0");
        assert_eq!(got[0].busy, 25.0);
        assert_eq!(got[0].mem_total, 0);
    }

    #[test]
    fn xe_fdinfo_uses_its_own_total_cycles() {
        let t = "drm-pdev: 0000:00:02.0\ndrm-client-id: 3\ndrm-cycles-rcs: 50\ndrm-total-cycles-rcs: 400\n";
        let (pdev, id, c) = parse_fdinfo(t).unwrap();
        assert_eq!((pdev.as_str(), id, c["rcs"]), ("0000:00:02.0", 3, (50, Some(400))));
        assert!(parse_fdinfo("drm-engine-render: 5 ns\n").is_none()); // no pdev/client: not usable
    }

    /// Manual: compare with `nvidia-smi --query-gpu=utilization.gpu,utilization.encoder,utilization.decoder,memory.used,memory.total --format=csv`.
    #[test]
    #[ignore]
    fn live_nvml() {
        println!("{:#?}", Reader::new().read());
    }
}
