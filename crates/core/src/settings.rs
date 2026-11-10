//! User settings in one TOML file under the XDG data dir (`~/.local/share/FastProcMan`).
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::files::{read_or_backup, write_atomic};

/// Folder name under the XDG data dir.
pub const APP_DIR: &str = "FastProcMan";
pub const MIN_SIZE: (u32, u32) = (640, 400);
/// Upper bound for a hand-edited size; a huge value would fail at window creation.
pub const MAX_SIZE: u32 = 8192;
/// Allowed refresh intervals in seconds, in the order the UI lists them.
pub const INTERVALS: [u32; 3] = [1, 2, 5];

/// When the slowdown banner shows and which processes get the amber rows. The defaults are the measured values.
/// Memory: available memory (page cache counts as available) under this share of the total. The stall figures alone
/// give almost no warning: on a 32 GB machine with 256 MB of swap, a fast allocation went from fine to OOM-killed
/// with about 3 s of memory stall in total. Danger is where earlyoom starts killing; watch is the earlier point where
/// the desktop still responds but opening one more big program may tip it over. The stalls are the share of time
/// programs waited (PSI) above which the desktop is already noticeably slow.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Alerts {
    pub mem_watch_pct: u32,
    pub mem_danger_pct: u32,
    pub psi_mem: u32,
    pub psi_io: u32,
    pub psi_cpu: u32,
    /// A CPU culprit must use at least this much (100 = one core), so an idle process is never blamed.
    pub cpu_culprit_min: u32,
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            mem_watch_pct: 20,
            mem_danger_pct: 10,
            psi_mem: 10,
            psi_io: 20,
            psi_cpu: 60,
            cpu_culprit_min: 50,
        }
    }
}

impl Alerts {
    /// A hand-edited file (or a typed value) must not make the banner impossible or permanent. Only per-field ranges:
    /// the spin boxes report every keystroke, so a cross-field rule would drag the other number down while the user
    /// is still typing ("5" on the way to "50"). `sys::pressure` applies danger <= watch instead.
    pub fn normalize(&mut self) {
        self.mem_watch_pct = self.mem_watch_pct.clamp(1, 99);
        self.mem_danger_pct = self.mem_danger_pct.clamp(1, 99);
        self.psi_mem = self.psi_mem.clamp(1, 100);
        self.psi_io = self.psi_io.clamp(1, 100);
        self.psi_cpu = self.psi_cpu.clamp(1, 100);
        self.cpu_culprit_min = self.cpu_culprit_min.clamp(1, 1000);
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)] // older files keep loading when a field is added
pub struct Settings {
    /// "" follows the system locale; otherwise a gettext code such as "pt_BR".
    pub language: String,
    pub width: u32,
    pub height: u32,
    /// Refresh interval in seconds: 1, 2 or 5 (anything else falls back to 1).
    pub interval_secs: u32,
    /// Per-process CPU % as a share of the whole machine (100 = all cores) instead of one core (top style).
    pub cpu_whole_machine: bool,
    /// Show processes as a tree (parent → children) instead of a flat list.
    pub tree: bool,
    /// In tree mode, group the processes by application (like Mission Center) instead of one plain forest.
    pub group_apps: bool,
    /// "" follows the system's light or dark mode; otherwise "light" or "dark".
    pub theme: String,
    /// Slowdown banner limits (`[alerts]` table).
    pub alerts: Alerts,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: String::new(),
            width: 860,
            height: 560,
            interval_secs: 1,
            cpu_whole_machine: false,
            tree: false,
            group_apps: true,
            theme: String::new(),
            alerts: Alerts::default(),
        }
    }
}

fn parse(text: &str) -> Result<Settings, toml::de::Error> {
    toml::from_str(text)
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_DIR)
}

fn path() -> PathBuf {
    data_dir().join("settings.toml")
}

impl Settings {
    pub fn load() -> Self {
        // The file is hand-edited; a bad one is moved aside so the next save() must not overwrite what the user wrote.
        let mut s = read_or_backup(&path(), |t| parse(t).map_err(|e| e.to_string())).0;
        s.normalize();
        s
    }

    fn normalize(&mut self) {
        self.width = self.width.clamp(MIN_SIZE.0, MAX_SIZE);
        self.height = self.height.clamp(MIN_SIZE.1, MAX_SIZE);
        if !INTERVALS.contains(&self.interval_secs) {
            self.interval_secs = 1;
        }
        self.alerts.normalize();
    }

    /// Atomic write so a crash can never leave a half-written file. Failures are reported on stderr;
    /// the app keeps working with the in-memory settings.
    pub fn save(&self) {
        if let Err(e) = self.try_save() {
            eprintln!("cannot save settings to {}: {e}", path().display());
        }
    }

    fn try_save(&self) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(&path(), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_still_load() {
        let parsed: Settings = toml::from_str("width = 700").unwrap();
        assert_eq!((parsed.width, parsed.language.as_str()), (700, ""));
    }

    #[test]
    fn alerts_default_to_the_measured_values_and_load_partially() {
        let s: Settings = toml::from_str("width = 700").unwrap();
        assert_eq!(s.alerts, Alerts::default());
        let s: Settings = toml::from_str("[alerts]\nmem_watch_pct = 30").unwrap();
        assert_eq!((s.alerts.mem_watch_pct, s.alerts.psi_cpu), (30, 60));
    }

    #[test]
    fn alerts_are_clamped_per_field() {
        let mut s: Settings = toml::from_str("[alerts]\nmem_watch_pct = 500\nmem_danger_pct = 0\npsi_io = 0").unwrap();
        s.normalize();
        assert_eq!(
            (s.alerts.mem_watch_pct, s.alerts.mem_danger_pct, s.alerts.psi_io),
            (99, 1, 1)
        );
        let mut a = Alerts {
            mem_watch_pct: 5,
            mem_danger_pct: 50,
            ..Alerts::default()
        };
        a.normalize();
        // Not clamped against each other: typing "5" on the way to "50" must not drag the other number down.
        assert_eq!((a.mem_watch_pct, a.mem_danger_pct), (5, 50));
    }

    #[test]
    fn interval_defaults_and_rejects_odd_values() {
        let mut s: Settings = toml::from_str("width = 700").unwrap();
        assert_eq!(s.interval_secs, 1);
        s.interval_secs = 3;
        s.normalize();
        assert_eq!(s.interval_secs, 1);
        s.interval_secs = 5;
        s.normalize();
        assert_eq!(s.interval_secs, 5);
    }

    #[test]
    fn theme_defaults_to_the_system_and_loads_when_set() {
        let old: Settings = toml::from_str("width = 700").unwrap();
        assert_eq!(old.theme, "");
        let dark: Settings = toml::from_str("theme = \"dark\"").unwrap();
        assert_eq!(dark.theme, "dark");
    }

    #[test]
    fn a_saved_interval_is_kept_when_the_default_changes() {
        let s: Settings = toml::from_str("interval_secs = 2").unwrap();
        assert_eq!(s.interval_secs, 2);
    }

    #[test]
    fn cpu_scope_defaults_to_per_core_and_loads_when_set() {
        let old: Settings = toml::from_str("width = 700").unwrap();
        assert!(!old.cpu_whole_machine);
        let on: Settings = toml::from_str("cpu_whole_machine = true").unwrap();
        assert!(on.cpu_whole_machine);
    }

    #[test]
    fn tree_defaults_to_flat_and_loads_when_set() {
        let old: Settings = toml::from_str("width = 700").unwrap();
        assert!(!old.tree);
        let on: Settings = toml::from_str("tree = true").unwrap();
        assert!(on.tree);
    }

    #[test]
    fn group_apps_defaults_to_on_and_loads_when_set() {
        let old: Settings = toml::from_str("width = 700").unwrap();
        assert!(old.group_apps);
        let off: Settings = toml::from_str("group_apps = false").unwrap();
        assert!(!off.group_apps);
    }

    #[test]
    fn invalid_file_is_an_error_not_defaults() {
        assert!(parse("width = \"wide\"").is_err());
    }

    #[test]
    fn window_size_is_clamped_both_ways() {
        let mut s: Settings = toml::from_str("width = 4000000000\nheight = 1").unwrap();
        s.normalize();
        assert_eq!((s.width, s.height), (MAX_SIZE, MIN_SIZE.1));
    }
}
