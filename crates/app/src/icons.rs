//! Process icons. Finding them reads hundreds of files, so it runs on its own thread after the window is up: rows
//! start with an empty icon slot and `main.rs` fills the slots in as batches come back.
//!
//! The lookup follows FastCleany: a process maps to a `.desktop` launcher (by program name, `StartupWMClass` or file
//! name), the launcher's `Icon=` is a file or a theme icon name, and a process with no launcher is tried as a theme
//! icon name. Differences: the theme folders are walked once into a name index (a process without an icon, the usual
//! case for daemons, would otherwise cost thousands of `stat` calls), and sizes are tried smallest first because
//! the slot is 16 px.
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

/// One process to find an icon for. `key` is what the answer is filed under (the exe path, or the name).
pub struct Request {
    pub key: String,
    pub name: String,
    pub exe: Option<String>,
    pub pid: u32,
}

/// Per key: the icon file and the application name (see `Index::app`).
pub type Found = Vec<(String, Option<PathBuf>, Option<String>)>;

/// Starts the resolver thread; `done` gets every answered batch (it is called on that thread). The index is built when
/// the first batch arrives.
pub fn spawn(done: impl Fn(Found) + Send + 'static) -> mpsc::Sender<Vec<Request>> {
    let (tx, rx) = mpsc::channel::<Vec<Request>>();
    thread::spawn(move || {
        let mut index: Option<Index> = None;
        for batch in rx {
            let index = index.get_or_insert_with(Index::system);
            done(
                batch
                    .iter()
                    .map(|r| (r.key.clone(), index.find(r), index.app(r)))
                    .collect(),
            );
        }
    });
    tx
}

/// Decodes on the UI thread (`slint::Image` is not `Send`). Slint decodes path images lazily and prints an error at
/// render time, so a file it cannot decode is never handed over by path: no extension is sniffed from the bytes.
pub fn load(path: &Path) -> Option<slint::Image> {
    if path.extension().is_some() {
        slint::Image::load_from_path(path).ok()
    } else {
        slint::Image::load_from_data(&fs::read(path).ok()?, None).ok()
    }
}

/// Smaller is better; PNG before SVG because it decodes cheaper, then the earlier theme, then the size nearest 16.
type Rank = (u8, usize, usize);

const SIZES: [&str; 8] = ["16", "22", "24", "32", "48", "64", "128", "256"];

#[derive(Default)]
struct Index {
    /// Lowercased program name, `StartupWMClass` or launcher file name -> its `Icon=`.
    strong: HashMap<String, String>,
    /// Last part of a reverse-DNS file name (`org.mozilla.firefox` -> `firefox`): a guess, tried after the rest.
    weak: HashMap<String, String>,
    /// Theme icon name -> best file.
    themes: HashMap<String, (Rank, PathBuf)>,
    /// Same keys as `strong` -> the launcher's `Name=`. Stricter than the icons, because a wrong name merges
    /// unrelated processes into one group: hidden launchers (`NoDisplay`) are left out, and a key claimed by two
    /// launchers with different names (a shared `Exec` such as java or electron) is `None`.
    apps: HashMap<String, Option<String>>,
    /// Folder of a launcher's real program -> the launcher's `Name=` and `Icon=`, same `None` rule as `apps`. Only the
    /// fallback of `app` and `find`, for processes not named like the launcher (Chrome: `google-chrome` execs
    /// `chrome`; Cisco's helpers sit next to `vpnui`).
    dirs: HashMap<PathBuf, Option<(String, String)>>,
    /// `PATH` folders, which hold many unrelated programs and so never name an app.
    path: Vec<PathBuf>,
}

impl Index {
    /// The folders a desktop on Linux Mint has, plus the Flatpak exports (which are not in `XDG_DATA_DIRS` unless
    /// the session was set up for them). shortcut: built once; apps installed while running get no icon until restart.
    fn system() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
        let dirs = if dirs.is_empty() {
            "/usr/local/share:/usr/share".into()
        } else {
            dirs
        };
        let mut shares = vec![data_home, home.join(".local/share/flatpak/exports/share")];
        shares.extend(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
        shares.push("/var/lib/flatpak/exports/share".into());
        let desktop: Vec<PathBuf> = shares.iter().map(|s| s.join("applications")).collect();
        // The active theme is not known without a settings daemon, so every installed theme counts, hicolor first.
        let mut theme_dirs: Vec<PathBuf> = shares
            .iter()
            .map(|s| s.join("icons"))
            .chain([home.join(".icons")])
            .filter_map(|r| fs::read_dir(r).ok())
            .flat_map(|d| d.flatten().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        theme_dirs.sort_by_key(|t| (!t.ends_with("hicolor"), t.clone()));
        theme_dirs.dedup();
        let path: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        Self::build(&desktop, &theme_dirs, Path::new("/usr/share/pixmaps"), path)
    }

    fn build(desktop: &[PathBuf], themes: &[PathBuf], pixmaps: &Path, path: Vec<PathBuf>) -> Self {
        let mut index = Self {
            path,
            ..Self::default()
        };
        // A launcher file name seen in an earlier folder shadows the same name in a later one (XDG: the user's copy
        // replaces the system's), so it must not add a second `Name=` to the same keys.
        let mut seen = HashSet::new();
        for dir in desktop {
            let mut files: Vec<PathBuf> = fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
                .collect();
            // read_dir order is arbitrary; sorted so the same icon is picked every run.
            files.sort();
            for f in files {
                if !seen.insert(f.file_name().map(ToOwned::to_owned)) {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(&f) {
                    index.add_launcher(&f, &text);
                }
            }
        }
        for (i, t) in themes.iter().enumerate() {
            index.add_theme(i, t);
        }
        index.add_dir(themes.len(), usize::MAX, pixmaps);
        index
    }

    fn add_launcher(&mut self, file: &Path, text: &str) {
        let entry = parse_launcher(text);
        let name = entry.name.clone().filter(|n| !n.is_empty() && !entry.hidden);
        let Some(icon) = entry.icon.filter(|i| !i.is_empty()) else {
            return;
        };
        if let (Some(n), Some(dir)) = (&name, entry.bare.as_deref().and_then(|b| self.program_dir(b))) {
            let slot = self.dirs.entry(dir).or_insert_with(|| Some((n.clone(), icon.clone())));
            if slot.as_ref().is_some_and(|(old, _)| old != n) {
                *slot = None;
            }
        }
        let mut strong = vec![];
        strong.extend(entry.program);
        strong.extend(entry.wm_class);
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if let Some((_, last)) = stem.rsplit_once('.') {
            self.weak.entry(last.to_string()).or_insert_with(|| icon.clone());
        }
        strong.push(stem);
        for k in strong.into_iter().filter(|k| !k.is_empty()) {
            if let Some(n) = &name {
                let slot = self.apps.entry(k.clone()).or_insert_with(|| Some(n.clone()));
                if slot.as_ref().is_some_and(|old| old != n) {
                    *slot = None;
                }
            }
            self.strong.entry(k).or_insert_with(|| icon.clone());
        }
    }

    /// The folder a launcher's program really lives in (symlinks followed, a bare name searched in `PATH`), unless it
    /// is a folder shared by unrelated programs.
    fn program_dir(&self, program: &str) -> Option<PathBuf> {
        let found = if program.contains('/') {
            PathBuf::from(program)
        } else {
            self.path.iter().map(|d| d.join(program)).find(|p| p.is_file())?
        };
        let dir = fs::canonicalize(found).ok()?.parent()?.to_path_buf();
        (!shared_dir(&dir, &self.path)).then_some(dir)
    }

    /// Layouts differ (`48x48/apps`, `48/apps`, `apps/48`, `scalable/apps`); only `apps` holds program icons.
    fn add_theme(&mut self, theme: usize, dir: &Path) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let (path, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
            if name == "apps" {
                for s in fs::read_dir(&path).into_iter().flatten().flatten() {
                    self.add_dir(theme, size_rank(&s.file_name().to_string_lossy()), &s.path());
                }
            } else {
                self.add_dir(theme, size_rank(&name), &path.join("apps"));
            }
        }
    }

    fn add_dir(&mut self, theme: usize, size: usize, dir: &Path) {
        for f in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = f.path();
            let ext = match path.extension().and_then(|x| x.to_str()) {
                Some("png") => 0,
                Some("svg") => 1,
                _ => continue,
            };
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let rank = (ext, theme, size);
            match self.themes.get(stem) {
                Some((best, _)) if *best <= rank => {}
                _ => {
                    self.themes.insert(stem.to_string(), (rank, path));
                }
            }
        }
    }

    /// An `Icon=` value is an absolute file or a theme icon name. A missing file or an `.xpm` (Slint cannot decode it)
    /// falls back to the theme icon of the same name.
    fn icon_file(&self, icon: &str) -> Option<PathBuf> {
        let path = Path::new(icon);
        if path.is_absolute() && path.is_file() && path.extension().is_none_or(|e| e != "xpm") {
            return Some(path.to_path_buf());
        }
        let name = if path.is_absolute() {
            path.file_stem()?.to_str()?
        } else {
            // `Icon=foo.png` appears in the wild.
            icon.strip_suffix(".png")
                .or_else(|| icon.strip_suffix(".svg"))
                .unwrap_or(icon)
        };
        self.themes.get(name).map(|(_, p)| p.clone())
    }

    fn base(r: &Request) -> &str {
        r.exe.as_deref().and_then(|e| e.rsplit('/').next()).unwrap_or_default()
    }

    /// Lowercased names a launcher may be filed under, best first.
    fn names(r: &Request) -> Vec<String> {
        // A Flatpak app's own id is the launcher's file name.
        let flatpak = r
            .exe
            .as_deref()
            .filter(|e| e.starts_with("/app/"))
            .and_then(|_| flatpak_id(r.pid));
        // An AppImage's real process runs from `/tmp/.mount_*`; its launcher runs the `.appimage` file.
        let appimage = r
            .exe
            .as_deref()
            .filter(|e| e.starts_with("/tmp/.mount_"))
            .and_then(|e| appimage_from_environ(&fs::read(format!("/proc/{}/environ", r.pid)).ok()?, e));
        Self::names_from(r, flatpak.or(appimage))
    }

    fn names_from(r: &Request, lead: Option<String>) -> Vec<String> {
        let names: Vec<String> = lead
            .into_iter()
            .chain([Self::base(r).to_string(), r.name.clone()])
            .filter(|n| !n.is_empty())
            .map(|n| n.to_lowercase())
            .collect();
        // A launcher often runs a wrapper script (`Exec=firefox`) that execs `firefox-bin`: try the plain name last.
        let plain: Vec<String> = names
            .iter()
            .filter_map(|n| n.strip_suffix("-bin").map(str::to_string))
            .collect();
        names.into_iter().chain(plain).collect()
    }

    /// The application a process belongs to, from a visible launcher only (never the theme or pixmaps guesses).
    fn app(&self, r: &Request) -> Option<String> {
        Self::names(r)
            .iter()
            .find_map(|n| self.apps.get(n)?.clone())
            // Only when no name matched, and only the exe's own folder: `chrome` is run by `google-chrome`.
            .or_else(|| self.in_folder(r).map(|(name, _)| name))
    }

    /// The launcher whose program lives in the process's own folder (see `dirs`): its name and icon.
    fn in_folder(&self, r: &Request) -> Option<(String, String)> {
        self.dirs.get(Path::new(r.exe.as_deref()?).parent()?)?.clone()
    }

    fn find(&self, r: &Request) -> Option<PathBuf> {
        let base = Self::base(r);
        let names = Self::names(r);
        let launcher = |map: &HashMap<String, String>| names.iter().find_map(|n| self.icon_file(map.get(n)?));
        launcher(&self.strong)
            .or_else(|| self.in_folder(r).and_then(|(_, icon)| self.icon_file(&icon)))
            .or_else(|| launcher(&self.weak))
            .or_else(|| {
                // No launcher: the program may still have a theme icon of its own name.
                [base, r.name.as_str()]
                    .into_iter()
                    .chain(names.iter().map(String::as_str))
                    .find_map(|n| self.icon_file(n))
            })
    }
}

fn size_rank(dir: &str) -> usize {
    let digits: String = dir.chars().take_while(char::is_ascii_digit).collect();
    SIZES
        .iter()
        .position(|s| *s == digits)
        .unwrap_or(SIZES.len() + usize::from(dir != "scalable"))
}

/// `[Application] name=` of a sandboxed process, from the file the Flatpak runtime puts at the root of its sandbox.
fn flatpak_id(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/root/.flatpak-info")).ok()?;
    text.lines().find_map(|l| l.strip_prefix("name=")).map(str::to_string)
}

/// The `.appimage` file an AppImage process came from: `APPIMAGE` in its environment, trusted only when the exe is
/// under that process's own `APPDIR` (an unrelated child may inherit the variables). The caller reads `environ`
/// only for exes under `/tmp/.mount_`.
fn appimage_from_environ(environ: &[u8], exe: &str) -> Option<String> {
    let var = |key: &str| {
        environ
            .split(|b| *b == 0)
            .find_map(|kv| kv.strip_prefix(key.as_bytes())?.strip_prefix(b"="))
            .map(|v| String::from_utf8_lossy(v).into_owned())
    };
    let (image, dir) = (var("APPIMAGE")?, var("APPDIR")?);
    let inside = exe.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/'));
    inside
        .then(|| image.rsplit('/').next().map(str::to_lowercase))
        .flatten()
}

#[derive(Default, Debug, PartialEq)]
struct Launcher {
    icon: Option<String>,
    /// Lowercased basename of the program in `Exec=`.
    program: Option<String>,
    wm_class: Option<String>,
    name: Option<String>,
    /// `NoDisplay=true` or `Hidden=true`: not an application the user runs.
    hidden: bool,
    /// The program as written, only when `Exec=` has no arguments but field codes (`%U`): a launcher with more
    /// (`--app-id=`, `--incognito`, `--writer`) is a variant of the app, so it must not speak for the folder.
    bare: Option<String>,
}

/// Only the `[Desktop Entry]` group: its `[Desktop Action ...]` groups have their own `Icon=` and `Exec=`.
fn parse_launcher(text: &str) -> Launcher {
    let mut out = Launcher::default();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[Desktop Entry]";
        } else if inside && let Some((k, v)) = line.split_once('=') {
            match k {
                "Icon" => out.icon = Some(v.trim().to_string()),
                "Exec" => {
                    out.program = program_of(v);
                    out.bare = bare_program(v);
                }
                "StartupWMClass" => out.wm_class = Some(v.trim().to_lowercase()),
                "Name" => out.name = Some(v.trim().to_string()),
                "NoDisplay" | "Hidden" => out.hidden |= v.trim() == "true",
                _ => {}
            }
        }
    }
    out
}

/// Lowercased basename of the program an `Exec=` line starts, looking past `env VAR=x`. Launchers through a shell or
/// `flatpak run` name no program of their own (their file name already does), so they give none.
fn program_of(exec: &str) -> Option<String> {
    let mut words = exec.split_whitespace();
    let mut first = words.next()?;
    if first == "env" {
        first = words.find(|w| !w.contains('=') && !w.starts_with('-'))?;
    }
    let base = first.trim_matches('"').rsplit('/').next()?.to_lowercase();
    (!matches!(base.as_str(), "flatpak" | "snap" | "sh" | "bash" | "gtk-launch" | "") && !base.contains('='))
        .then_some(base)
}

/// The program of an `Exec=` line when nothing follows it but field codes (see `Launcher::bare`). `env VAR=x` is
/// looked past, like `program_of`; the same wrappers give none.
fn bare_program(exec: &str) -> Option<String> {
    let mut words = exec.split_whitespace().peekable();
    if words.peek() == Some(&"env") {
        words.next();
        while words.peek().is_some_and(|w| w.contains('=')) {
            words.next();
        }
    }
    let first = words.next()?.trim_matches('"');
    program_of(first)?;
    words.all(|w| w.starts_with('%')).then(|| first.to_string())
}

/// Folders that hold many unrelated programs, so they can't name one app: `PATH` entries, the library roots (also
/// the multiarch ones), and anything at most one level below a home folder (`~/AppImages`).
fn shared_dir(dir: &Path, path: &[PathBuf]) -> bool {
    const ROOTS: [&str; 11] = [
        "/",
        "/usr",
        "/opt",
        "/usr/lib",
        "/usr/lib32",
        "/usr/lib64",
        "/usr/libexec",
        "/usr/local",
        "/usr/local/lib",
        "/usr/share",
        "/usr/local/share",
    ];
    let multiarch = dir.parent() == Some(Path::new("/usr/lib"))
        && dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains("-linux-gnu"));
    path.iter().any(|p| p == dir)
        || ROOTS.iter().any(|r| dir == Path::new(r))
        || multiarch
        || (dir.starts_with("/home") && dir.components().count() <= 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_uses_the_main_group_only() {
        let text = "[Desktop Entry]\nName=Foo\nName[pt_BR]=Fu\nExec=env GDK=1 /opt/Foo/Foo-Bin %U\nIcon=foo\n\
                    StartupWMClass=FooApp\n[Desktop Action new]\nName=New\nExec=other\nIcon=other\nNoDisplay=true\n";
        assert_eq!(
            parse_launcher(text),
            Launcher {
                icon: Some("foo".into()),
                program: Some("foo-bin".into()),
                wm_class: Some("fooapp".into()),
                name: Some("Foo".into()),
                hidden: false,
                bare: Some("/opt/Foo/Foo-Bin".into()),
            }
        );
    }

    #[test]
    fn an_appimage_process_is_named_after_its_file() {
        let env = b"HOME=/h\0APPIMAGE=/h/AppImages/SpotiFast.appimage\0APPDIR=/tmp/.mount_spotifX\0";
        let exe = "/tmp/.mount_spotifX/usr/bin/spotifast";
        assert_eq!(appimage_from_environ(env, exe), Some("spotifast.appimage".into()));
        // Not under its own APPDIR (an inherited variable), a sibling folder with the same prefix, or no variables.
        assert_eq!(appimage_from_environ(env, "/usr/bin/other"), None);
        assert_eq!(appimage_from_environ(env, "/tmp/.mount_spotifXY/usr/bin/x"), None);
        assert_eq!(appimage_from_environ(b"HOME=/h\0", exe), None);
        assert_eq!(appimage_from_environ(b"APPIMAGE=/h/a.appimage\0", exe), None);
    }

    #[test]
    fn the_appimage_launcher_wins_over_a_stale_one() {
        let d = scratch("appimg");
        fs::create_dir_all(d.join("apps")).unwrap();
        let real = d.join("icon-file");
        touch(&real);
        let w = |f: &str, t: String| fs::write(d.join("apps").join(f), t).unwrap();
        // The stale launcher sorts first and its icon does not exist.
        w(
            "Spotifast.desktop",
            "[Desktop Entry]\nName=Spotifast\nExec=flatpak run x.Spotifast\nIcon=x.Spotifast\n".into(),
        );
        w(
            "spotifast.desktop",
            format!(
                "[Desktop Entry]\nName=Spotifast\nExec=env A=1 /h/AppImages/spotifast.appimage %u\nIcon={}\n",
                real.display()
            ),
        );
        let index = Index::build(&[d.join("apps")], &[], &d.join("pix"), vec![]);
        let r = request("AppRun", Some("/tmp/.mount_spotifX/usr/bin/spotifast"));
        let names = Index::names_from(&r, Some("spotifast.appimage".into()));
        assert_eq!(names[0], "spotifast.appimage");
        let icon = names.iter().find_map(|n| index.icon_file(index.strong.get(n)?));
        assert_eq!(icon, Some(real));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn only_a_bare_exec_speaks_for_its_folder() {
        assert_eq!(bare_program("/opt/x/x %U"), Some("/opt/x/x".into()));
        assert_eq!(bare_program("env A=1 x"), Some("x".into()));
        assert_eq!(bare_program("/opt/x/x --app-id=1"), None);
        assert_eq!(bare_program("/opt/x/x --incognito"), None);
        assert_eq!(bare_program("flatpak run org.x.Y"), None);
    }

    #[test]
    fn shared_folders_name_no_app() {
        let path = vec![PathBuf::from("/usr/bin")];
        for d in [
            "/usr/bin",
            "/opt",
            "/usr/lib",
            "/usr/lib/x86_64-linux-gnu",
            "/home/me",
            "/home/me/AppImages",
        ] {
            assert!(shared_dir(Path::new(d), &path), "{d}");
        }
        for d in [
            "/opt/google/chrome",
            "/usr/lib/libreoffice/program",
            "/usr/share/code",
            "/home/me/apps/foo",
        ] {
            assert!(!shared_dir(Path::new(d), &path), "{d}");
        }
    }

    #[test]
    fn app_falls_back_to_the_install_folder() {
        let d = scratch("dirs");
        let (apps, foo) = (d.join("apps"), d.join("opt/foo"));
        fs::create_dir_all(&apps).unwrap();
        fs::create_dir_all(&foo).unwrap();
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(foo.join("foo-launch"), "").unwrap();
        // `PATH` has a symlink to the wrapper, like /usr/bin/google-chrome-stable.
        std::os::unix::fs::symlink(foo.join("foo-launch"), d.join("bin/foo-stable")).unwrap();
        fs::write(d.join("bin/other"), "").unwrap();
        let w = |f: &str, t: &str| fs::write(apps.join(f), t).unwrap();
        w("foo.desktop", "[Desktop Entry]\nName=Foo\nExec=foo-stable %U\nIcon=a\n");
        // A variant in the same folder does not blank it, and a bare launcher of a program in `PATH` names nothing.
        w(
            "meet.desktop",
            &format!(
                "[Desktop Entry]\nName=Meet\nExec={}/foo-launch --app-id=1\nIcon=a\n",
                foo.display()
            ),
        );
        w("other.desktop", "[Desktop Entry]\nName=Other\nExec=other\nIcon=a\n");
        let index = Index::build(std::slice::from_ref(&apps), &[], &d.join("pix"), vec![d.join("bin")]);
        let exe = |p: &str| request("real", Some(&d.join(p).to_string_lossy()));
        assert_eq!(index.app(&exe("opt/foo/real")), Some("Foo".into()));
        // The icon comes from the same launcher.
        assert_eq!(index.in_folder(&exe("opt/foo/real")), Some(("Foo".into(), "a".into())));
        // Exact folder only, and a name match still comes first.
        assert_eq!(index.app(&exe("opt/foo/sub/real")), None);
        assert_eq!(
            index.app(&request("other", Some(&d.join("bin/other").to_string_lossy()))),
            Some("Other".into())
        );
        assert_eq!(index.app(&request("x", Some(&d.join("bin/x").to_string_lossy()))), None);
        // A second bare launcher with another name makes the folder ambiguous.
        w(
            "bar.desktop",
            &format!(
                "[Desktop Entry]\nName=Bar\nExec={}/foo-launch %U\nIcon=a\n",
                foo.display()
            ),
        );
        let index = Index::build(&[apps], &[], &d.join("pix"), vec![d.join("bin")]);
        assert_eq!(index.app(&exe("opt/foo/real")), None);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn a_shadowed_launcher_adds_no_second_name() {
        let d = scratch("shadow");
        for (dir, name) in [("user", "Mine"), ("sys", "Theirs")] {
            fs::create_dir_all(d.join(dir)).unwrap();
            fs::write(
                d.join(dir).join("g.desktop"),
                format!("[Desktop Entry]\nName={name}\nExec=g\nIcon=a\n"),
            )
            .unwrap();
        }
        let index = Index::build(&[d.join("user"), d.join("sys")], &[], &d.join("pix"), vec![]);
        assert_eq!(index.app(&request("g", None)), Some("Mine".into()));
        let _ = fs::remove_dir_all(d);
    }

    /// Manual: `cargo test -p fastprocman live_apps -- --ignored --nocapture` lists the application of every process
    /// on this machine, to compare with another task manager.
    #[test]
    #[ignore]
    fn live_apps() {
        let index = Index::system();
        let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
        for e in fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let name = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
            let r = Request {
                key: String::new(),
                name: name.trim().into(),
                exe: exe.map(|p| p.to_string_lossy().into_owned()),
                pid,
            };
            if let Some(app) = index.app(&r) {
                // An app that has a name but no icon is a launcher lookup that missed.
                let tag = if index.find(&r).is_some() { "" } else { " (no icon)" };
                *seen.entry(format!("{app}{tag}")).or_default() += 1;
            }
        }
        for (app, n) in seen {
            println!("{app}: {n}");
        }
    }

    #[test]
    fn app_names_come_from_visible_launchers_with_one_name_per_key() {
        let d = scratch("apps");
        let w = |f: &str, t: &str| fs::write(d.join("apps").join(f), t).unwrap();
        fs::create_dir_all(d.join("apps")).unwrap();
        w("a.desktop", "[Desktop Entry]\nName=Alpha\nExec=alpha %U\nIcon=a\n");
        // Hidden launchers name nothing and do not take a key from a visible one.
        w(
            "b-hidden.desktop",
            "[Desktop Entry]\nName=Hid\nExec=alpha\nIcon=b\nNoDisplay=true\n",
        );
        w(
            "c-hidden.desktop",
            "[Desktop Entry]\nName=Hid\nExec=pyx\nIcon=b\nNoDisplay=true\n",
        );
        // Two visible launchers sharing one program: ambiguous, so no app.
        w("d1.desktop", "[Desktop Entry]\nName=One\nExec=java -jar 1\nIcon=a\n");
        w("d2.desktop", "[Desktop Entry]\nName=Two\nExec=java -jar 2\nIcon=a\n");
        touch(&d.join("icons/hicolor/48x48/apps/htop.png"));
        let index = Index::build(&[d.join("apps")], &[d.join("icons/hicolor")], &d.join("pix"), vec![]);
        let app = |n: &str| index.app(&request(n, None));
        assert_eq!(app("alpha"), Some("Alpha".into()));
        // A wrapper script's real program (`Exec=alpha` runs `alpha-bin`, as with Firefox).
        let bin = request("alpha-bin", Some("/usr/lib/alpha/alpha-bin"));
        assert_eq!(index.app(&bin), Some("Alpha".into()));
        assert_eq!(app("pyx"), None);
        assert_eq!(app("java"), None);
        // A theme icon alone is not an application.
        assert_eq!(app("htop"), None);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn wrapper_programs_name_nothing() {
        assert_eq!(program_of("flatpak run org.x.Y"), None);
        assert_eq!(program_of("env A=1 B=2 firefox %u"), Some("firefox".into()));
        assert_eq!(program_of("\"/usr/bin/Foo\" --x"), Some("foo".into()));
    }

    #[test]
    fn smaller_sizes_rank_first_and_scalable_last() {
        assert!(size_rank("16x16") < size_rank("48x48"));
        assert!(size_rank("48") < size_rank("scalable"));
        assert!(size_rank("scalable") < size_rank("weird"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fpm-icons-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "").unwrap();
    }

    fn request(name: &str, exe: Option<&str>) -> Request {
        Request {
            key: String::new(),
            name: name.into(),
            exe: exe.map(Into::into),
            pid: 0,
        }
    }

    #[test]
    fn a_process_finds_its_icon_through_launcher_class_theme_and_pixmaps() {
        let d = scratch("find");
        fs::create_dir_all(d.join("apps")).unwrap();
        fs::write(
            d.join("apps/ff.desktop"),
            "[Desktop Entry]\nExec=/usr/lib/firefox/firefox %u\nIcon=firefox\n",
        )
        .unwrap();
        fs::write(
            d.join("apps/org.gnome.Calc.desktop"),
            "[Desktop Entry]\nExec=flatpak run org.gnome.Calc\nIcon=calc-icon\n",
        )
        .unwrap();
        fs::write(
            d.join("apps/x.desktop"),
            "[Desktop Entry]\nExec=xed\nIcon=text-editor\nStartupWMClass=Xed\n",
        )
        .unwrap();
        // The 48 px PNG beats the SVG and a 256 px PNG; hicolor (theme 0) beats a later theme.
        touch(&d.join("icons/hicolor/48x48/apps/firefox.png"));
        touch(&d.join("icons/hicolor/scalable/apps/firefox.svg"));
        touch(&d.join("icons/other/256x256/apps/firefox.png"));
        touch(&d.join("icons/hicolor/16x16/apps/calc-icon.png"));
        touch(&d.join("icons/hicolor/apps/32/text-editor.png"));
        touch(&d.join("icons/hicolor/32/apps/htop.png"));
        touch(&d.join("pix/pixie.png"));
        let themes = [d.join("icons/hicolor"), d.join("icons/other")];
        let index = Index::build(&[d.join("apps")], &themes, &d.join("pix"), vec![]);

        let ends = |r: Request, tail: &str| {
            let p = index.find(&r).unwrap_or_default();
            assert!(p.ends_with(tail), "{:?} for {tail}", p);
        };
        // By the program in Exec=, whatever the process calls itself.
        ends(
            request("GeckoMain", Some("/usr/lib/firefox/firefox")),
            "hicolor/48x48/apps/firefox.png",
        );
        // By the last part of the launcher's reverse-DNS file name, and by StartupWMClass.
        ends(
            request("calc", Some("/usr/bin/calc")),
            "hicolor/16x16/apps/calc-icon.png",
        );
        ends(request("xed", None), "hicolor/apps/32/text-editor.png");
        // No launcher: a theme icon of the process's own name, then /usr/share/pixmaps.
        ends(request("htop", None), "hicolor/32/apps/htop.png");
        ends(request("pixie", None), "pix/pixie.png");
        assert_eq!(index.find(&request("kworker", None)), None);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn an_absolute_icon_that_is_missing_or_xpm_falls_back_to_the_theme() {
        let d = scratch("abs");
        touch(&d.join("icons/hicolor/48x48/apps/tool.png"));
        touch(&d.join("real.png"));
        touch(&d.join("old.xpm"));
        let index = Index::build(&[], &[d.join("icons/hicolor")], &d.join("pix"), vec![]);
        assert_eq!(
            index.icon_file(d.join("real.png").to_str().unwrap()),
            Some(d.join("real.png"))
        );
        let themed = |p: PathBuf| index.icon_file(p.to_str().unwrap()).unwrap_or_default();
        assert!(themed(d.join("old.xpm")).as_os_str().is_empty());
        assert!(themed(d.join("gone/tool.png")).ends_with("hicolor/48x48/apps/tool.png"));
        let _ = fs::remove_dir_all(d);
    }
}
