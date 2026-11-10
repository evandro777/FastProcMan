use fastprocman_core::{
    services,
    settings::{Alerts, INTERVALS, Settings},
    sys::{self, KillError, Proc, Reason, Sample, Sampler, Signal, SysStats},
    tree::{self, Node},
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, winit_030::WinitWindowAccessor};
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    fmt::Write,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

slint::include_modules!();

mod fixtures;
mod host;
mod icons;
mod instance;
mod svc;

/// Samples kept per chart: 60 x the refresh interval (1 minute at the default 1 s).
const HIST: usize = 60;
/// Icon keys remembered as already asked before the set is emptied (see `request_icons`).
const ASKED_MAX: usize = 5000;
/// Lowest chart scale for network and disk, so idle noise does not fill the whole card.
const RATE_FLOOR: f64 = 64.0 * 1024.0;

enum Cmd {
    Interval(u32),
    /// Resample soon (after a kill).
    Now,
    /// The Services tab was shown or hidden: list services only while it is visible.
    Services(bool),
}

#[derive(Default)]
struct Hist {
    cpu: VecDeque<f64>,
    mem: VecDeque<f64>,
    swap: VecDeque<f64>,
    rx: VecDeque<f64>,
    tx: VecDeque<f64>,
    /// One series per logical CPU.
    cores: Vec<VecDeque<f64>>,
    /// `(name, read, write)` per disk, in the order the sampler lists them; a disk that vanishes is dropped.
    disks: Vec<(String, VecDeque<f64>, VecDeque<f64>)>,
    /// Per GPU: busy, video encode, video decode, memory %.
    gpus: Vec<[VecDeque<f64>; 4]>,
}

fn push(q: &mut VecDeque<f64>, v: f64) {
    if q.len() == HIST {
        q.pop_front();
    }
    q.push_back(v);
}

struct State {
    procs: Vec<Proc>,
    sys: SysStats,
    /// `(pid, start)` of the rows now in the table, in order: the table is re-sorted every tick, so a row index
    /// is never trusted past the next refresh.
    shown: Vec<(u32, u64)>,
    selected: Option<(u32, u64, String)>,
    /// The processes the slowdown banner names, in its order; a click on one selects it.
    alert: Vec<(u32, u64, String)>,
    /// `(pid, start)` the Properties dialog shows. Not the selection: that follows the table, this is fixed at open.
    props: Option<(u32, u64)>,
    /// Column and ascending; Memory descending to start.
    sort: (usize, bool),
    filter: String,
    strong_only: bool,
    /// `settings.alerts`: when the slowdown banner shows and who it blames (the amber rows).
    alerts: Alerts,
    /// `settings.cpu_whole_machine`: show process CPU % over all cores instead of one core.
    cpu_whole: bool,
    /// `settings.tree`: parents with their children under them instead of a flat list.
    tree: bool,
    /// `settings.group_apps`: in tree mode, application group rows on top.
    group_apps: bool,
    /// Whether a tree row with children is open unless it is in `flipped`: true at start, and set by Expand all
    /// and Collapse all, so a program that appears later follows the last choice.
    open_by_default: bool,
    /// Tree rows, by `(pid, start)`, that differ from `open_by_default` (the ones clicked since).
    flipped: HashSet<(u32, u64)>,
    /// Like `open_by_default`, for the application group rows (they start closed, "Processes" starts open).
    apps_open_by_default: bool,
    /// Icons found so far, by `icon_key`. A program with no icon is simply absent.
    icons: HashMap<String, slint::Image>,
    /// Application name of a program, by `icon_key`, from its visible launcher. Absent for most processes.
    apps: HashMap<String, String>,
    /// Keys already sent to the resolver thread, so none is asked twice (a miss is not retried either).
    asked: HashSet<String>,
    /// The resolver thread, started with the first batch so the window never waits for it.
    icon_tx: Option<mpsc::Sender<Vec<icons::Request>>>,
    hist: Hist,
    model: Rc<VecModel<ProcRow>>,
    /// The Graphs page's cards; rows are updated in place while their number is unchanged, so the page keeps its
    /// scroll position.
    graphs: Rc<VecModel<GraphData>>,
    svc: svc::Svc,
}

/// What an icon is filed under: the program's path, else its name (other users' processes have no readable path).
fn icon_key(p: &Proc) -> &str {
    p.exe.as_deref().unwrap_or(&p.name)
}

impl State {
    fn is_open(&self, p: &Proc) -> bool {
        self.open_by_default != self.flipped.contains(&(p.pid, p.start))
    }

    /// An application group row by name; `""` is the "Processes" group.
    fn group_open(&self, name: &str) -> bool {
        let default = if name.is_empty() {
            self.open_by_default
        } else {
            self.apps_open_by_default
        };
        default != self.flipped.contains(&tree::group_key(name))
    }

    /// A process's CPU % as shown. Sorting uses the raw value: a constant divisor keeps the order.
    fn cpu_shown(&self, pct: f32) -> f32 {
        if self.cpu_whole {
            pct / self.sys.cores.max(1) as f32
        } else {
            pct
        }
    }
}

// The sampler thread hands plain data to the UI thread, which owns all of this.
thread_local! {
    static APP: RefCell<State> = RefCell::new(State {
        procs: Vec::new(),
        sys: SysStats::default(),
        shown: Vec::new(),
        selected: None,
        alert: Vec::new(),
        props: None,
        sort: (4, false),
        filter: String::new(),
        strong_only: false,
        alerts: Alerts::default(),
        cpu_whole: false,
        tree: false,
        group_apps: true,
        open_by_default: true,
        flipped: HashSet::new(),
        apps_open_by_default: false,
        icons: HashMap::new(),
        apps: HashMap::new(),
        asked: HashSet::new(),
        icon_tx: None,
        hist: Hist::default(),
        model: Rc::new(VecModel::default()),
        graphs: Rc::new(VecModel::default()),
        svc: svc::Svc::new(),
    });
}

/// Everything `refresh` puts on screen, built while `APP` is borrowed and applied after the borrow is released,
/// so no `RefCell` borrow is ever held across a call into Slint.
struct View {
    model: Rc<VecModel<ProcRow>>,
    rows: Vec<ProcRow>,
    selected_row: i32,
    selected_label: SharedString,
    selected_stopped: bool,
    count: SharedString,
    alert_text: SharedString,
    alert_danger: bool,
    alert_items: Vec<SharedString>,
    ready: bool,
    graph_model: Rc<VecModel<GraphData>>,
    /// `None` while the Graphs tab is hidden: the history is still recorded, only the strings are skipped.
    graphs: Option<Vec<GraphData>>,
}

fn warning_text(tr: &Tr, w: Option<Reason>) -> SharedString {
    match w {
        None => SharedString::new(),
        Some(Reason::Deleted) => tr.invoke_warning_deleted(),
        Some(Reason::TempDir) => tr.invoke_warning_temp(),
        Some(Reason::NameMismatch) => tr.invoke_warning_name(),
    }
}

/// Orders rows (siblings, in tree mode) by the sorted column. CPU and Memory use the node's totals, which are the
/// process's own values in a flat list. Ties go by PID so the order is stable between ticks.
fn compare((col, asc): (usize, bool), a: &Node, b: &Node) -> Ordering {
    let o = match col {
        // `Node.name`, not `p.name`: an application group row shows the application's name, not its first process's.
        0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        1 => a.p.pid.cmp(&b.p.pid),
        2 => a.p.user.cmp(&b.p.user),
        3 => a.cpu.total_cmp(&b.cpu),
        4 => a.mem_kb.cmp(&b.mem_kb),
        _ => sys::severity(a.p.warn).cmp(&sys::severity(b.p.warn)),
    };
    (if asc { o } else { o.reverse() }).then(a.p.pid.cmp(&b.p.pid))
}

/// One table row before it is turned into text: where it is in `procs`, how deep, and the values it shows.
struct Shown {
    key: (u32, u64),
    /// Application group row: its name (`""` for "Processes"). `idx` is then its first process, for the icon.
    group: Option<String>,
    idx: usize,
    depth: usize,
    /// 0 none, 1 collapsed, 2 expanded.
    expander: i32,
    cpu: f32,
    mem_kb: u64,
    /// A strong warning inside what this row hides (`tree::Row::hidden_warn`).
    hidden_warn: Option<Reason>,
}

/// Unit (0 B, 1 KB, 2 MB, 3 GB) and number text of `bytes`. The unit is picked from the value as it will be printed,
/// so 1,047,900 bytes is "1.0 MB", not "1024 KB" (KB prints no decimals, MB one).
fn scale(bytes: f64) -> (u8, String) {
    const K: f64 = 1024.0;
    if bytes < K - 0.5 {
        (0, format!("{bytes:.0}"))
    } else if bytes < K * (K - 0.5) {
        (1, format!("{:.0}", bytes / K))
    } else if bytes < K * K * (K - 0.05) {
        (2, format!("{:.1}", bytes / K / K))
    } else {
        (3, format!("{:.2}", bytes / K / K / K))
    }
}

fn size(tr: &Tr, bytes: f64) -> SharedString {
    let (unit, n) = scale(bytes);
    let n = n.into();
    match unit {
        0 => tr.invoke_size_b(n),
        1 => tr.invoke_size_kb(n),
        2 => tr.invoke_size_mb(n),
        _ => tr.invoke_size_gb(n),
    }
}

fn rate(tr: &Tr, bytes: f64) -> SharedString {
    tr.invoke_rate(size(tr, bytes))
}

/// SVG path in the 59 x 100 box of `GraphData`; right-aligned so the newest sample is always at the right edge.
fn path(q: &VecDeque<f64>, scale: f64) -> SharedString {
    let mut s = String::new();
    let x0 = (HIST - q.len()) as f64;
    for (i, v) in q.iter().enumerate() {
        let y = 100.0 - (v / scale * 100.0).clamp(0.0, 100.0);
        let _ = write!(s, "{} {:.1} {y:.1} ", if i == 0 { 'M' } else { 'L' }, x0 + i as f64);
    }
    s.into()
}

/// `path` closed down to the bottom edge: the soft area under a card's first line. Empty with no samples.
fn area(q: &VecDeque<f64>, scale: f64) -> SharedString {
    if q.is_empty() {
        return SharedString::new();
    }
    let x0 = (HIST - q.len()) as f64;
    format!("{}L {} 100 L {x0} 100 Z", path(q, scale), HIST - 1).into()
}

fn peak(a: &VecDeque<f64>, b: &VecDeque<f64>) -> f64 {
    a.iter().chain(b).copied().fold(RATE_FLOOR, f64::max)
}

impl State {
    /// The selected process is stopped (`T`/`t`), so the menu offers Resume instead of Suspend.
    fn selected_stopped(&self) -> bool {
        self.selected.as_ref().is_some_and(|(pid, start, _)| {
            self.procs
                .iter()
                .any(|p| (p.pid, p.start) == (*pid, *start) && matches!(p.state, 'T' | 't'))
        })
    }

    fn view(&mut self, tr: &Tr, graphs: bool) -> View {
        let needle = self.filter.trim().to_lowercase();
        let filtering = !needle.is_empty() || self.strong_only;
        let matches = |p: &Proc| {
            (!self.strong_only || p.warn.is_some_and(Reason::is_strong))
                && (needle.is_empty() || p.name.to_lowercase().contains(&needle) || p.pid.to_string().contains(&needle))
        };
        let sort = self.sort;
        let cmp = move |a: &Node, b: &Node| compare(sort, a, b);

        // Which processes are shown, in order, and (tree mode) every process's parent, for a hidden selection.
        let (list, parent, matching): (Vec<Shown>, Vec<Option<usize>>, usize) = if self.tree {
            let filter = filtering.then_some(&matches as &dyn Fn(&Proc) -> bool);
            let app = |p: &Proc| {
                self.apps
                    .get(icon_key(p))
                    .filter(|_| self.group_apps)
                    .map(String::as_str)
            };
            let t = tree::build(
                &self.procs,
                &app,
                &|name| self.group_open(name),
                &|p| self.is_open(p),
                filter,
                &cmp,
            );
            let list = t
                .rows
                .iter()
                .map(|r| Shown {
                    key: r.key,
                    group: r.group.clone(),
                    idx: r.idx,
                    depth: r.depth,
                    expander: match (r.has_kids, r.expanded) {
                        (false, _) => 0,
                        (true, false) => 1,
                        (true, true) => 2,
                    },
                    cpu: r.cpu,
                    mem_kb: r.mem_kb,
                    hidden_warn: r.hidden_warn,
                })
                .collect();
            // The count is of matching processes: with everything collapsed, the rows shown are only the roots.
            (list, t.parent, self.procs.iter().filter(|p| matches(p)).count())
        } else {
            let mut nodes: Vec<(usize, Node)> = self
                .procs
                .iter()
                .enumerate()
                .filter(|(_, p)| matches(p))
                .map(|(i, p)| {
                    let node = Node {
                        p,
                        name: &p.name,
                        cpu: p.cpu_pct,
                        mem_kb: p.mem_kb,
                    };
                    (i, node)
                })
                .collect();
            nodes.sort_by(|a, b| cmp(&a.1, &b.1));
            let list: Vec<Shown> = nodes
                .iter()
                .map(|(i, n)| Shown {
                    key: (n.p.pid, n.p.start),
                    group: None,
                    idx: *i,
                    depth: 0,
                    expander: 0,
                    cpu: n.cpu,
                    mem_kb: n.mem_kb,
                    hidden_warn: None,
                })
                .collect();
            let n = list.len();
            (list, vec![], n)
        };

        // The slowdown banner and the processes it names (their rows are drawn amber).
        let alert = sys::pressure(&self.sys, &self.procs, &self.alerts);
        let left = size(
            tr,
            self.sys.mem_total_kb.saturating_sub(self.sys.mem_used_kb) as f64 * 1024.0,
        );
        let (alert_text, alert_danger) = match &alert {
            None => (SharedString::new(), false),
            Some(a) => (
                match (a.kind, a.danger) {
                    (sys::Pressure::Memory, false) => tr.invoke_alert_memory_watch(left),
                    (sys::Pressure::Memory, true) => tr.invoke_alert_memory_danger(left),
                    (sys::Pressure::Io, _) => tr.invoke_alert_io(),
                    (sys::Pressure::Cpu, _) => tr.invoke_alert_cpu(),
                },
                a.danger,
            ),
        };
        let alert_items: Vec<SharedString> = alert
            .iter()
            .flat_map(|a| {
                a.culprits.iter().map(|p| {
                    let value = match a.kind {
                        sys::Pressure::Cpu => tr.invoke_percent(format!("{:.0}", self.cpu_shown(p.cpu_pct)).into()),
                        _ => size(tr, p.mem_kb as f64 * 1024.0),
                    };
                    tr.invoke_alert_item(p.name.as_str().into(), value)
                })
            })
            .collect();
        let culprits: Vec<(u32, u64, String)> = alert
            .iter()
            .flat_map(|a| a.culprits.iter().map(|p| (p.pid, p.start, p.name.clone())))
            .collect();
        let risky: HashSet<(u32, u64)> = culprits.iter().map(|(pid, start, _)| (*pid, *start)).collect();
        self.alert = culprits;

        let rows = list
            .iter()
            .map(|s| {
                let p = &self.procs[s.idx];
                if let Some(name) = &s.group {
                    // An application (or "Processes") has no PID, user or warning of its own: its totals are all
                    // there is, and the first process lends the icon.
                    let processes = name.is_empty();
                    return ProcRow {
                        icon: if processes {
                            Default::default()
                        } else {
                            self.icons.get(icon_key(p)).cloned().unwrap_or_default()
                        },
                        depth: 0,
                        expander: s.expander,
                        name: if processes {
                            tr.invoke_group_processes()
                        } else {
                            name.as_str().into()
                        },
                        pid: SharedString::new(),
                        user: SharedString::new(),
                        cpu: format!("{:.1}", self.cpu_shown(s.cpu)).into(),
                        memory: size(tr, s.mem_kb as f64 * 1024.0),
                        // Only a strong warning inside the closed group, so the red flag is not lost behind it.
                        warning: warning_text(tr, s.hidden_warn),
                        strong: s.hidden_warn.is_some(),
                        risky: false,
                        group: true,
                    };
                }
                // A collapsed row also shows a strong warning of its hidden children, unless its own is strong.
                let reason = if p.warn.is_some_and(Reason::is_strong) {
                    p.warn
                } else {
                    s.hidden_warn.or(p.warn)
                };
                ProcRow {
                    icon: self.icons.get(icon_key(p)).cloned().unwrap_or_default(),
                    depth: s.depth as i32,
                    expander: s.expander,
                    name: p.name.as_str().into(),
                    pid: p.pid.to_string().into(),
                    user: p.user.as_str().into(),
                    cpu: format!("{:.1}", self.cpu_shown(s.cpu)).into(),
                    memory: size(tr, s.mem_kb as f64 * 1024.0),
                    warning: warning_text(tr, reason),
                    strong: reason.is_some_and(Reason::is_strong),
                    risky: risky.contains(&(p.pid, p.start)),
                    group: false,
                }
            })
            .collect();
        let shown: Vec<(u32, u64)> = list.iter().map(|s| s.key).collect();
        let count = tr.invoke_count(matching as i32);

        // The selection follows its process across re-sorts, and is dropped when the process is gone or filtered out.
        // Hidden inside a collapsed parent (not filtered out): the nearest parent that is shown takes it over.
        let followed = self.selected.as_ref().and_then(|(pid, start, _)| {
            tree::follow_selection(&shown, &self.procs, &parent, (*pid, *start), self.tree && !filtering)
        });
        let pos = followed.as_ref().map(|f| f.row);
        if let Some(a) = followed.and_then(|f| f.moved_to) {
            let p = &self.procs[a];
            self.selected = Some((p.pid, p.start, p.name.clone()));
        }
        self.shown = shown;
        if pos.is_none() {
            self.selected = None;
        }
        let selected_label = self
            .selected
            .as_ref()
            .map(|(pid, _, name)| tr.invoke_process_label(name.as_str().into(), *pid as i32))
            .unwrap_or_default();

        View {
            model: self.model.clone(),
            rows,
            selected_row: pos.map_or(-1, |i| i as i32),
            selected_label,
            selected_stopped: self.selected_stopped(),
            count,
            alert_text,
            alert_danger,
            alert_items,
            ready: self.hist.cpu.len() >= 2,
            graph_model: self.graphs.clone(),
            graphs: graphs.then(|| self.graph_cards(tr)),
        }
    }

    /// The Graphs page's cards: CPU (total plus one faint line per core), Memory (and swap), Network, each GPU,
    /// then each disk.
    fn graph_cards(&self, tr: &Tr) -> Vec<GraphData> {
        let (h, s) = (&self.hist, &self.sys);
        let last = |q: &VecDeque<f64>| q.back().copied().unwrap_or(0.0);
        let pct_text = |v: f64| SharedString::from(format!("{v:.0}"));
        let list = |v: Vec<SharedString>| ModelRc::from(v.as_slice());
        let mut cards = vec![GraphData {
            title: tr.invoke_graph_cpu(),
            value: tr.invoke_percent(pct_text(last(&h.cpu))),
            extra: SharedString::new(),
            legend: list(vec![]),
            paths: list(vec![path(&h.cpu, 100.0)]),
            fill: area(&h.cpu, 100.0),
            faint: h
                .cores
                .iter()
                .map(|c| path(c, 100.0))
                .collect::<Vec<_>>()
                .join("")
                .into(),
        }];

        let mem = |used_kb: u64, total_kb: u64, series: &VecDeque<f64>| {
            tr.invoke_mem_summary(
                size(tr, used_kb as f64 * 1024.0),
                size(tr, total_kb as f64 * 1024.0),
                pct_text(last(series)),
            )
        };
        // No swap: no second line, line or legend.
        let has_swap = s.swap_total_kb > 0;
        cards.push(GraphData {
            title: tr.invoke_graph_memory(),
            value: mem(s.mem_used_kb, s.mem_total_kb, &h.mem),
            extra: if has_swap {
                tr.invoke_labelled(tr.invoke_graph_swap(), mem(s.swap_used_kb, s.swap_total_kb, &h.swap))
            } else {
                SharedString::new()
            },
            legend: list(if has_swap {
                vec![tr.invoke_graph_memory(), tr.invoke_graph_swap()]
            } else {
                vec![]
            }),
            paths: list(if has_swap {
                vec![path(&h.mem, 100.0), path(&h.swap, 100.0)]
            } else {
                vec![path(&h.mem, 100.0)]
            }),
            fill: area(&h.mem, 100.0),
            faint: SharedString::new(),
        });

        let net_scale = peak(&h.rx, &h.tx);
        cards.push(GraphData {
            title: tr.invoke_graph_network(),
            value: tr.invoke_net_summary(rate(tr, last(&h.rx)), rate(tr, last(&h.tx))),
            extra: tr.invoke_graph_peak(rate(tr, net_scale)),
            legend: list(vec![tr.invoke_graph_download(), tr.invoke_graph_upload()]),
            paths: list(vec![path(&h.rx, net_scale), path(&h.tx, net_scale)]),
            fill: area(&h.rx, net_scale),
            faint: SharedString::new(),
        });

        for (g, q) in s.gpus.iter().zip(&h.gpus) {
            // Encode and decode are absent when the driver does not report them (AMD): drop their lines and legend.
            let parts = [
                (tr.invoke_graph_processing(), true, &q[0]),
                (tr.invoke_graph_encode(), g.enc.is_some(), &q[1]),
                (tr.invoke_graph_decode(), g.dec.is_some(), &q[2]),
                (tr.invoke_graph_memory(), true, &q[3]),
            ];
            let shown: Vec<_> = parts.iter().filter(|p| p.1).collect();
            cards.push(GraphData {
                title: g.name.as_str().into(),
                value: tr.invoke_percent(pct_text(last(&q[0]))),
                extra: if g.mem_total > 0 {
                    tr.invoke_labelled(
                        tr.invoke_graph_memory(),
                        mem(g.mem_used / 1024, g.mem_total / 1024, &q[3]),
                    )
                } else {
                    SharedString::new()
                },
                legend: list(shown.iter().map(|p| p.0.clone()).collect()),
                paths: list(shown.iter().map(|p| path(p.2, 100.0)).collect()),
                fill: area(&q[0], 100.0),
                faint: SharedString::new(),
            });
        }

        for (name, rd, wr) in &h.disks {
            let scale = peak(rd, wr);
            cards.push(GraphData {
                title: tr.invoke_graph_disk(name.as_str().into()),
                value: tr.invoke_disk_summary(rate(tr, last(rd)), rate(tr, last(wr))),
                extra: tr.invoke_graph_peak(rate(tr, scale)),
                legend: list(vec![tr.invoke_graph_read(), tr.invoke_graph_write()]),
                paths: list(vec![path(rd, scale), path(wr, scale)]),
                fill: area(rd, scale),
                faint: SharedString::new(),
            });
        }
        cards
    }
}

fn refresh(ui: &Main) {
    let v = APP.with_borrow_mut(|s| s.view(&ui.global::<Tr>(), ui.get_page() == 1));
    v.model.set_vec(v.rows);
    if let Some(graphs) = v.graphs {
        if v.graph_model.row_count() == graphs.len() {
            for (i, g) in graphs.into_iter().enumerate() {
                v.graph_model.set_row_data(i, g);
            }
        } else {
            v.graph_model.set_vec(graphs);
        }
    }
    // After the model: the table does not clamp its current row when the row count changes.
    ui.set_selected_row(v.selected_row);
    ui.set_selected_label(v.selected_label);
    ui.set_selected_stopped(v.selected_stopped);
    ui.set_count_text(v.count);
    ui.set_alert_text(v.alert_text);
    ui.set_alert_danger(v.alert_danger);
    ui.set_alert_items(ModelRc::from(v.alert_items.as_slice()));
    ui.set_graphs_ready(v.ready);
}

fn on_sample(ui: &Main, sample: Sample) {
    APP.with_borrow_mut(|s| {
        let m = &sample.sys;
        let mem_pct = if m.mem_total_kb == 0 {
            0.0
        } else {
            m.mem_used_kb as f64 / m.mem_total_kb as f64 * 100.0
        };
        push(&mut s.hist.cpu, m.cpu_pct as f64);
        push(&mut s.hist.mem, mem_pct);
        push(
            &mut s.hist.swap,
            if m.swap_total_kb == 0 {
                0.0
            } else {
                m.swap_used_kb as f64 / m.swap_total_kb as f64 * 100.0
            },
        );
        push(&mut s.hist.rx, m.net_rx);
        push(&mut s.hist.tx, m.net_tx);
        let h = &mut s.hist;
        h.cores.resize_with(m.cores_pct.len(), VecDeque::new);
        for (q, v) in h.cores.iter_mut().zip(&m.cores_pct) {
            push(q, *v as f64);
        }
        let mut old = std::mem::take(&mut h.disks);
        h.disks = m
            .disks
            .iter()
            .map(|d| {
                let mut e = old
                    .iter()
                    .position(|o| o.0 == d.name)
                    .map(|i| old.swap_remove(i))
                    .unwrap_or_else(|| (d.name.clone(), VecDeque::new(), VecDeque::new()));
                push(&mut e.1, d.read);
                push(&mut e.2, d.write);
                e
            })
            .collect();
        h.gpus.resize_with(m.gpus.len(), Default::default);
        for (q, g) in h.gpus.iter_mut().zip(&m.gpus) {
            let mem_pct = if g.mem_total == 0 {
                0.0
            } else {
                g.mem_used as f64 / g.mem_total as f64 * 100.0
            };
            for (series, v) in q.iter_mut().zip([
                g.busy as f64,
                g.enc.unwrap_or(0.0) as f64,
                g.dec.unwrap_or(0.0) as f64,
                mem_pct,
            ]) {
                push(series, v);
            }
        }
        s.sys = sample.sys;
        s.procs = sample.procs;
        if !s.flipped.is_empty() {
            let live: HashSet<_> = s.procs.iter().map(|p| (p.pid, p.start)).collect();
            // Group rows have PID 0 and live as long as the app is on screen: nothing to expire.
            s.flipped.retain(|k| k.0 == 0 || live.contains(k));
        }
    });
    request_icons(ui);
    refresh(ui);
    update_properties(ui);
}

/// Sends the programs whose icon is not known yet to the resolver thread (started on the first call). The rows are
/// already on screen by then; `on_icons` redraws them when the answer comes back.
fn request_icons(ui: &Main) {
    // Fixtures never read the real system.
    if fixtures::mode().is_some() {
        return;
    }
    let sent = APP.with_borrow_mut(|s| {
        let State { procs, asked, .. } = &mut *s;
        // Programs that run from a new path every time (build scripts) would grow this set for as long as the app
        // runs. Forgetting it costs one cheap re-ask per running program; icons already found stay in `State.icons`.
        if asked.len() > ASKED_MAX {
            asked.clear();
        }
        let batch: Vec<icons::Request> = procs
            .iter()
            // Kernel threads (children of kthreadd) have no program.
            .filter(|p| p.pid != 2 && !(p.exe.is_none() && p.ppid == 2))
            .filter(|p| asked.insert(icon_key(p).to_string()))
            .map(|p| icons::Request {
                key: icon_key(p).to_string(),
                name: p.name.clone(),
                exe: p.exe.clone(),
                pid: p.pid,
            })
            .collect();
        if batch.is_empty() {
            return None;
        }
        let weak = ui.as_weak();
        let tx = s.icon_tx.get_or_insert_with(|| {
            icons::spawn(move |found| {
                let _ = weak.upgrade_in_event_loop(move |ui| on_icons(&ui, found));
            })
        });
        Some((tx.clone(), batch))
    });
    if let Some((tx, batch)) = sent {
        let _ = tx.send(batch);
    }
}

/// The resolver's answer: decode the files here (a `slint::Image` is not `Send`) and redraw once for the batch.
fn on_icons(ui: &Main, found: icons::Found) {
    APP.with_borrow_mut(|s| {
        for (key, path, app) in found {
            if let Some(img) = path.as_deref().and_then(icons::load) {
                s.icons.insert(key.clone(), img);
            }
            if let Some(app) = app {
                s.apps.insert(key, app);
            }
        }
    });
    refresh(ui);
}

/// Opens or closes the tree row at table row `row`.
fn toggle_expand(ui: &Main, row: i32) {
    APP.with_borrow_mut(|s| {
        if let Some(key) = usize::try_from(row).ok().and_then(|i| s.shown.get(i).copied())
            && !s.flipped.remove(&key)
        {
            s.flipped.insert(key);
        }
    });
    refresh(ui);
}

/// Expand all / Collapse all: the new default for every row, including programs that start later.
fn set_all_open(ui: &Main, open: bool) {
    APP.with_borrow_mut(|s| {
        s.open_by_default = open;
        s.apps_open_by_default = open;
        s.flipped.clear();
    });
    refresh(ui);
}

/// Remembers the process behind table row `row` (`-1` for none).
fn select(ui: &Main, row: i32) {
    let (label, stopped) = APP.with_borrow_mut(|s| {
        let key = usize::try_from(row).ok().and_then(|i| s.shown.get(i).copied());
        s.selected = key.and_then(|(pid, start)| {
            s.procs
                .iter()
                .find(|p| (p.pid, p.start) == (pid, start))
                .map(|p| (pid, start, p.name.clone()))
        });
        (s.selected.clone(), s.selected_stopped())
    });
    let label = label.map(|(pid, _, name)| ui.global::<Tr>().invoke_process_label(name.as_str().into(), pid as i32));
    ui.set_selected_label(label.unwrap_or_default());
    ui.set_selected_stopped(stopped);
}

/// What the menu and the confirmation can do to the selected process.
#[derive(Clone, Copy)]
enum Action {
    Signal(Signal),
    Nice(i32),
}

fn act_selected(ui: &Main, tx: &mpsc::Sender<Cmd>, action: Action) {
    // Fixtures use made-up PIDs; never signal anything for them.
    if fixtures::mode().is_some() {
        return;
    }
    let Some((pid, start, name)) = APP.with_borrow(|s| s.selected.clone()) else {
        // The process left the list while the dialog was open: say so instead of doing nothing.
        ui.set_status(ui.global::<Tr>().invoke_status_exited());
        return;
    };
    // End and Kill come from the Confirm dialog, the rest from the context menu. In tree mode the selection can move
    // to an ancestor while either is open (the process lost its parent and the new one is collapsed): never act on
    // something the dialog or the menu did not name.
    let named = match action {
        Action::Signal(Signal::Term | Signal::Kill) => ui.get_confirm_label(),
        _ => ui.get_menu_label(),
    };
    if named != ui.global::<Tr>().invoke_process_label(name.as_str().into(), pid as i32) {
        ui.set_status(ui.global::<Tr>().invoke_status_exited());
        return;
    }
    let (weak, tx) = (ui.as_weak(), tx.clone());
    // pkexec waits for the polkit dialog, so this must never run on the UI thread.
    thread::spawn(move || {
        let result = match action {
            Action::Signal(sig) => sys::signal(pid, start, sig),
            Action::Nice(n) => sys::renice(pid, start, n),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let tr = ui.global::<Tr>();
            let label = tr.invoke_process_label(name.as_str().into(), pid as i32);
            ui.set_status(match result {
                Ok(()) => match action {
                    Action::Signal(Signal::Term) => tr.invoke_status_ended(label),
                    Action::Signal(Signal::Kill) => tr.invoke_status_killed(label),
                    Action::Signal(Signal::Stop) => tr.invoke_status_suspended(label),
                    Action::Signal(Signal::Cont) => tr.invoke_status_resumed(label),
                    Action::Nice(n) => tr.invoke_status_priority(label, n.to_string().into()),
                },
                Err(KillError::Exited) => tr.invoke_status_exited(),
                Err(KillError::Cancelled) => tr.invoke_status_cancelled(),
                Err(KillError::Refused) => tr.invoke_status_refused(),
                Err(KillError::Failed(e)) => tr.invoke_status_failed(e.into()),
            });
        });
        let _ = tx.send(Cmd::Now);
    });
}

/// Whether a key's text should start filtering: anything but control characters and Slint's private-use key codes
/// (arrows, F-keys, Menu…, U+F700 to U+F8FF).
fn printable(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
}

/// Appends typed `text` to the filter `field` picks and returns the whole filter, or `None` when the key is not used.
/// Shared by the Processes and Services tabs.
fn push_typed(text: &str, field: fn(&mut State) -> &mut String) -> Option<String> {
    if !printable(text) {
        return None;
    }
    APP.with_borrow_mut(|s| {
        let filter = field(s);
        // A space as the first character would only hide that the box has text (the filter is trimmed).
        if filter.is_empty() && text.trim().is_empty() {
            return None;
        }
        filter.push_str(text);
        Some(filter.clone())
    })
}

/// Typing while the table has focus goes into the filter box. Returns whether the key was used.
fn type_char(ui: &Main, text: &str) -> bool {
    if ui.get_page() == 2 {
        return svc::type_char(ui, text);
    }
    let Some(filter) = push_typed(text, |s| &mut s.filter) else {
        return false;
    };
    ui.set_filter(filter.as_str().into());
    // Byte offset: the box also takes UTF-8 text, so a character count would put the cursor in the wrong place.
    ui.invoke_focus_filter(filter.len() as i32);
    refresh(ui);
    true
}

/// At most `max` characters, with an ellipsis when cut: a command line can be kilobytes long.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn age(tr: &Tr, secs: u64) -> SharedString {
    let n = |v: u64| SharedString::from(v.to_string());
    match secs {
        0..60 => tr.invoke_age_s(n(secs)),
        60..3600 => tr.invoke_age_min(n(secs / 60), n(secs % 60)),
        3600..86400 => tr.invoke_age_h(n(secs / 3600), n(secs % 3600 / 60)),
        _ => tr.invoke_age_d(n(secs / 86400), n(secs % 86400 / 3600)),
    }
}

fn state_text(tr: &Tr, state: char) -> SharedString {
    match state {
        'R' => tr.invoke_state_running(),
        'S' | 'I' => tr.invoke_state_sleeping(),
        'D' => tr.invoke_state_disk(),
        'T' | 't' => tr.invoke_state_stopped(),
        'Z' => tr.invoke_state_zombie(),
        other => other.to_string().into(),
    }
}

/// A memory value from `/proc` status (kB), or "Unavailable" (kernel threads have none).
fn kb_text(tr: &Tr, kb: Option<u64>) -> SharedString {
    kb.map_or_else(|| tr.invoke_unavailable(), |k| size(tr, k as f64 * 1024.0))
}

fn prop_data(tr: &Tr, p: &Proc, d: &sys::Details) -> PropData {
    let text = |v: &Option<String>| {
        v.as_deref()
            .map_or_else(|| tr.invoke_unavailable(), |s| clip(s, 600).into())
    };
    PropData {
        name: p.name.as_str().into(),
        pid: p.pid.to_string().into(),
        ppid: d.ppid.to_string().into(),
        user: p.user.as_str().into(),
        state: state_text(tr, d.state),
        threads: d.threads.to_string().into(),
        priority: d.nice.to_string().into(),
        age: age(tr, d.age_secs),
        cpu: tr.invoke_percent(format!("{:.1}", APP.with_borrow(|s| s.cpu_shown(p.cpu_pct))).into()),
        memory: size(tr, p.mem_kb as f64 * 1024.0),
        resident: kb_text(tr, d.resident_kb),
        shared: kb_text(tr, d.shared_kb),
        virtual_mem: kb_text(tr, d.virtual_kb),
        io: d.io.map_or_else(
            || tr.invoke_unavailable(),
            |(r, w)| tr.invoke_disk_summary(size(tr, r as f64), size(tr, w as f64)),
        ),
        warning: warning_text(tr, p.warn),
        warning_note: match p.warn {
            None => SharedString::new(),
            Some(Reason::Deleted) => tr.invoke_warning_deleted_note(),
            Some(Reason::TempDir) => tr.invoke_warning_temp_note(),
            Some(Reason::NameMismatch) => tr.invoke_warning_name_note(),
        },
        exe: text(&d.exe),
        cmdline: text(&d.cmdline),
        cwd: text(&d.cwd),
    }
}

/// Reads `/proc` for the selected process now (never on the sampling path) and opens the dialog.
fn open_properties(ui: &Main) {
    let selected = APP.with_borrow(|s| {
        let (pid, start, _) = s.selected.as_ref()?;
        s.procs.iter().find(|p| (p.pid, p.start) == (*pid, *start)).cloned()
    });
    let Some(p) = selected else {
        return;
    };
    let details = if fixtures::mode().is_some() {
        Some(fixtures::details())
    } else {
        sys::details(p.pid, p.start)
    };
    let tr = ui.global::<Tr>();
    match details {
        Some(d) => {
            APP.with_borrow_mut(|s| s.props = Some((p.pid, p.start)));
            ui.set_prop_data(prop_data(&tr, &p, &d));
            ui.set_properties_open(true);
        }
        None => ui.set_status(tr.invoke_status_exited()),
    }
}

/// Keeps an open Properties dialog current: CPU, running time, state, threads, memory and disk change every tick.
/// It follows the process it was opened for, not the table selection, and says "Exited" once that process is gone.
fn update_properties(ui: &Main) {
    // Fixtures use made-up PIDs and a fixed dialog.
    if !ui.get_properties_open() || fixtures::mode().is_some() {
        return;
    }
    let Some((pid, start)) = APP.with_borrow(|s| s.props) else {
        return;
    };
    let proc = APP.with_borrow(|s| s.procs.iter().find(|p| (p.pid, p.start) == (pid, start)).cloned());
    let tr = ui.global::<Tr>();
    match proc.zip(sys::details(pid, start)) {
        Some((p, d)) => ui.set_prop_data(prop_data(&tr, &p, &d)),
        None => {
            let mut data = ui.get_prop_data();
            data.state = tr.invoke_state_exited();
            ui.set_prop_data(data);
        }
    }
}

fn spawn_sampler(weak: slint::Weak<Main>, rx: mpsc::Receiver<Cmd>, secs: u32) {
    thread::spawn(move || {
        let mut sampler = Sampler::new();
        let mut interval = Duration::from_secs(secs.into());
        // Services are listed only while their tab is shown; the unit-file states (0.7 s) are read when it opens.
        let mut services_shown = false;
        let mut unit_files = HashMap::new();
        // `Sampler::new` took a priming reading; a short wait makes the first CPU % real instead of zero.
        let settle = Duration::from_millis(300);
        thread::sleep(settle);
        loop {
            let sample = sampler.sample();
            // An empty unit-file map means that read timed out: try again instead of keeping a blank Startup column.
            if services_shown && unit_files.is_empty() {
                unit_files = services::unit_files();
            }
            // `None` (systemctl gave nothing): the tab keeps its last list.
            let services = services_shown
                .then(|| services::list(&sample.procs, &unit_files))
                .flatten();
            let shown = weak.upgrade_in_event_loop(move |ui| {
                on_sample(&ui, sample);
                if let Some(list) = services {
                    svc::on_services(&ui, list);
                }
            });
            if shown.is_err() {
                return;
            }
            match rx.recv_timeout(interval) {
                Ok(cmd) => {
                    match cmd {
                        Cmd::Interval(s) => interval = Duration::from_secs(s.into()),
                        Cmd::Services(on) => {
                            services_shown = on;
                            if on {
                                // Keep the old map if this read timed out and came back empty.
                                let fresh = services::unit_files();
                                if !fresh.is_empty() {
                                    unit_files = fresh;
                                }
                            }
                        }
                        Cmd::Now => {}
                    }
                    // Counters are in 10 ms ticks: a sample right after the last one would be mostly noise.
                    thread::sleep(settle);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

fn main() {
    if std::env::args().skip(1).any(|a| a == "-V" || a == "--version") {
        return println!("fastprocman {}", env!("CARGO_PKG_VERSION"));
    }
    // Fixtures (scripts/ui-snapshot.sh) never touch a real running instance.
    let fixture_run = fixtures::mode().is_some();
    // One window: a copy is already open, so it comes to the front and this one leaves before setting anything up.
    // Before `host::run`, so a second Flatpak start does not spawn another host copy.
    if !fixture_run && instance::send("show") {
        return;
    }
    // In a Flatpak this relaunches the app on the host, where /proc shows every process. It has to come before
    // any Slint object exists, and falls back to the sandbox when the host cannot be reached.
    let host_failed = match host::run() {
        host::Outcome::Exited(code) => std::process::exit(code),
        host::Outcome::Failed => true,
        host::Outcome::NotNeeded => false,
    };
    // Forced so the 100 MB GL renderer is never pulled in by accident.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .expect("cannot start the Slint software renderer");

    let settings = Rc::new(RefCell::new(Settings::load()));
    let ui = Main::new().unwrap();
    // Take the socket now that the window exists; a later start sends "show" and this window is raised.
    if !fixture_run {
        let weak = ui.as_weak();
        let raise = move |_command: String| {
            let _ = weak.upgrade_in_event_loop(|ui| instance::raise(ui.window()));
        };
        if instance::claim("show", raise) {
            return;
        }
    }
    apply_language(&settings.borrow());
    ui.set_language_index(
        LANGUAGES
            .iter()
            .position(|l| *l == settings.borrow().language)
            .unwrap_or(0) as i32,
    );
    ui.set_color_scheme_index(theme_index(&settings.borrow().theme));
    ui.set_rows(ModelRc::from(APP.with_borrow(|s| s.model.clone())));
    ui.set_graphs(ModelRc::from(APP.with_borrow(|s| s.graphs.clone())));
    ui.set_interval_index(
        INTERVALS
            .iter()
            .position(|&i| i == settings.borrow().interval_secs)
            .unwrap_or(0) as i32,
    );
    let cpu_whole = settings.borrow().cpu_whole_machine;
    let tree = settings.borrow().tree;
    let group_apps = settings.borrow().group_apps;
    let alerts = settings.borrow().alerts;
    APP.with_borrow_mut(|s| {
        s.alerts = alerts;
        s.cpu_whole = cpu_whole;
        s.tree = tree;
        s.group_apps = group_apps;
    });
    ui.set_limits(limits_to_ui(&alerts));
    ui.set_cpu_whole_machine(cpu_whole);
    ui.set_group_apps(group_apps);
    ui.set_tree(tree);
    if host_failed {
        ui.set_status(ui.global::<Tr>().invoke_status_sandboxed());
    }

    let (tx, rx) = mpsc::channel::<Cmd>();
    let weak = ui.as_weak();
    svc::connect(&ui, &tx);
    ui.on_row_selected({
        let weak = weak.clone();
        move |row| weak.upgrade().iter().for_each(|ui| select(ui, row))
    });
    // Show the process a slowdown-banner button names: back to the Processes tab with nothing hiding it, selected and
    // scrolled to. Ending it is still Delete, which asks first.
    ui.on_alert_clicked({
        let weak = weak.clone();
        move |i| {
            let Some(ui) = weak.upgrade() else { return };
            APP.with_borrow_mut(|s| {
                s.filter.clear();
                s.strong_only = false;
                // Tree mode: the culprit may sit in a closed app group or collapsed parent, where no row can be
                // selected. Open everything, like "Expand all".
                s.open_by_default = true;
                s.apps_open_by_default = true;
                s.flipped.clear();
                s.selected = usize::try_from(i).ok().and_then(|i| s.alert.get(i).cloned());
            });
            ui.set_filter(SharedString::new());
            ui.set_suspicious_only(false);
            ui.set_page(0);
            refresh(&ui);
            ui.invoke_reveal(ui.get_selected_row());
        }
    });
    ui.on_page_changed({
        let weak = weak.clone();
        move || weak.upgrade().iter().for_each(refresh)
    });
    ui.on_filter_changed({
        let weak = weak.clone();
        move |text| {
            APP.with_borrow_mut(|s| s.filter = text.to_string());
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_suspicious_toggled({
        let weak = weak.clone();
        move |on| {
            APP.with_borrow_mut(|s| s.strong_only = on);
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_tree_toggled({
        let (weak, settings) = (weak.clone(), settings.clone());
        move |on| {
            settings.borrow_mut().tree = on;
            settings.borrow().save();
            APP.with_borrow_mut(|s| s.tree = on);
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_toggle_expand({
        let weak = weak.clone();
        move |row| weak.upgrade().iter().for_each(|ui| toggle_expand(ui, row))
    });
    ui.on_expand_all({
        let weak = weak.clone();
        move || weak.upgrade().iter().for_each(|ui| set_all_open(ui, true))
    });
    ui.on_collapse_all({
        let weak = weak.clone();
        move || weak.upgrade().iter().for_each(|ui| set_all_open(ui, false))
    });
    // The table reports the column only; sorting is ours so it can use the numbers, not the formatted text.
    ui.on_sort_ascending({
        let weak = weak.clone();
        move |col| {
            APP.with_borrow_mut(|s| s.sort = (col as usize, true));
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_sort_descending({
        let weak = weak.clone();
        move |col| {
            APP.with_borrow_mut(|s| s.sort = (col as usize, false));
            weak.upgrade().iter().for_each(refresh);
        }
    });
    // End and kill arrive here only after the confirmation dialog.
    let on_action = |action: fn() -> Action| {
        let (weak, tx) = (weak.clone(), tx.clone());
        move || weak.upgrade().iter().for_each(|ui| act_selected(ui, &tx, action()))
    };
    ui.on_end(on_action(|| Action::Signal(Signal::Term)));
    ui.on_kill(on_action(|| Action::Signal(Signal::Kill)));
    ui.on_suspend(on_action(|| Action::Signal(Signal::Stop)));
    ui.on_resume(on_action(|| Action::Signal(Signal::Cont)));
    ui.on_renice({
        let (weak, tx) = (weak.clone(), tx.clone());
        move |nice| {
            weak.upgrade()
                .iter()
                .for_each(|ui| act_selected(ui, &tx, Action::Nice(nice)))
        }
    });
    ui.on_properties({
        let weak = weak.clone();
        move || weak.upgrade().iter().for_each(open_properties)
    });
    ui.on_type_char({
        let weak = weak.clone();
        move |text| weak.upgrade().is_some_and(|ui| type_char(&ui, &text))
    });
    ui.on_interval_changed({
        let (settings, tx) = (settings.clone(), tx.clone());
        move |index| {
            let Some(&secs) = usize::try_from(index).ok().and_then(|i| INTERVALS.get(i)) else {
                return;
            };
            settings.borrow_mut().interval_secs = secs;
            settings.borrow().save();
            let _ = tx.send(Cmd::Interval(secs));
        }
    });
    ui.on_group_apps_changed({
        let (weak, settings) = (weak.clone(), settings.clone());
        move |on| {
            settings.borrow_mut().group_apps = on;
            settings.borrow().save();
            APP.with_borrow_mut(|s| s.group_apps = on);
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_language_changed({
        let (weak, settings) = (weak.clone(), settings.clone());
        move |i| {
            let Some(code) = usize::try_from(i).ok().and_then(|i| LANGUAGES.get(i)) else {
                return;
            };
            settings.borrow_mut().language = code.to_string();
            settings.borrow().save();
            apply_language(&settings.borrow());
            // The rows, charts and counts hold text built in Rust: build it again in the new language.
            weak.upgrade().iter().for_each(|ui| {
                refresh(ui);
                svc::refresh(ui);
                update_properties(ui);
            });
        }
    });
    // Saved as soon as it changes, like the other options. The window already shows the new mode (`color-scheme-index`).
    ui.on_theme_changed({
        let settings = settings.clone();
        move |i| {
            let Some(theme) = usize::try_from(i).ok().and_then(|i| THEMES.get(i)) else {
                return;
            };
            settings.borrow_mut().theme = theme.to_string();
            settings.borrow().save();
        }
    });
    // Typed values are clamped here, and the clamped numbers go back to the spin boxes. Next tick uses them.
    ui.on_limits_changed({
        let (weak, settings) = (weak.clone(), settings.clone());
        move |l| set_alerts(&weak, &settings, limits_from_ui(&l))
    });
    ui.on_limits_reset({
        let (weak, settings) = (weak.clone(), settings.clone());
        move || set_alerts(&weak, &settings, Alerts::default())
    });
    ui.on_cpu_scope_changed({
        let (weak, settings) = (weak.clone(), settings.clone());
        move |whole| {
            settings.borrow_mut().cpu_whole_machine = whole;
            settings.borrow().save();
            APP.with_borrow_mut(|s| s.cpu_whole = whole);
            // Show it now, not at the next tick.
            weak.upgrade().iter().for_each(|ui| {
                refresh(ui);
                update_properties(ui);
            });
        }
    });
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.on_open_website(|| {
        // Waiting in a thread reaps the child; a dropped `Child` would stay a zombie until exit.
        std::thread::spawn(|| {
            if let Err(e) = std::process::Command::new("xdg-open")
                .arg(env!("CARGO_PKG_REPOSITORY"))
                .status()
            {
                eprintln!("cannot open the website: {e}");
            }
        });
    });

    // With fixtures (scripts/ui-snapshot.sh) nothing is sampled, so the real system is never read or changed.
    match fixtures::mode() {
        Some(mode) => fixtures::apply(&ui, mode),
        None => spawn_sampler(ui.as_weak(), rx, settings.borrow().interval_secs),
    }
    // Opens a page, a dialog or the context menu directly, for the same script.
    match std::env::var("FASTPROCMAN_PAGE").as_deref() {
        Ok("about") => ui.set_about_open(true),
        Ok("settings") => ui.set_settings_open(true),
        Ok(p @ ("confirm" | "confirm-kill")) => {
            // The fixture's one process with an icon (fastprocman, 1011), so the snapshot shows it in the title.
            if let Some(row) = APP.with_borrow(|s| s.shown.iter().position(|k| k.0 == 1011)) {
                select(&ui, row as i32);
                ui.set_selected_row(row as i32);
                if let Some(r) = ui.get_rows().row_data(row) {
                    ui.set_confirm_icon(r.icon);
                }
            }
            ui.set_confirm_force(p == "confirm-kill");
            ui.set_confirm_label(ui.get_selected_label());
            ui.set_confirm_open(true);
        }
        Ok("properties") => {
            // The fixture's flagged process (updater, 1002), so the snapshot shows the Warning row and its note.
            if let Some(row) = APP.with_borrow(|s| s.shown.iter().position(|k| k.0 == 1002)) {
                select(&ui, row as i32);
            }
            open_properties(&ui)
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
            refresh(&ui);
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
        Ok("graphs-tall") => ui.set_page(1),
        Ok(p) => ui.set_page(p.parse().unwrap_or(0)),
        Err(_) => {}
    }
    let s = settings.borrow();
    ui.window()
        .set_size(slint::LogicalSize::new(s.width as f32, s.height as f32));
    drop(s);
    // Remember the size for the next start. Not with fixtures (snapshots must not change the owner's settings), and
    // not while maximized or fullscreen, where the size is the screen's, not the user's choice.
    if fixtures::mode().is_none() {
        let (weak, settings) = (ui.as_weak(), settings.clone());
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                let w = ui.window();
                if !w.is_maximized() && !w.is_fullscreen() {
                    let size = w.size().to_logical(w.scale_factor());
                    let mut s = settings.borrow_mut();
                    (s.width, s.height) = (size.width.round() as u32, size.height.round() as u32);
                    s.save();
                }
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    // A window tall enough for every Graphs card, which the 560 px default scrolls.
    if std::env::var("FASTPROCMAN_PAGE").as_deref() == Ok("graphs-tall") {
        ui.window().set_size(slint::LogicalSize::new(860.0, 1150.0));
    }
    // Ask the window manager to activate the window, so typing works without a click first.
    ui.show().unwrap();
    // winit's X11 `focus_window` does nothing until the window is visible, which it is not right after `show()`;
    // so ask once the event loop has mapped it (otherwise the keys keep going to the launching terminal).
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(200), move || {
        weak.upgrade()
            .iter()
            .for_each(|ui| _ = ui.window().with_winit_window(|w| w.focus_window()));
    });
    slint::run_event_loop().unwrap();
}

/// What the Preferences language box offers, in its order: `""` follows the system, then the bundled languages.
const LANGUAGES: [&str; 3] = ["", "en", "pt_BR"];

/// `settings.theme` for each entry of the Appearance box: system, light, dark.
const THEMES: [&str; 3] = ["", "light", "dark"];

/// The Appearance box index of a `settings.theme` value; anything unknown follows the system.
fn theme_index(theme: &str) -> i32 {
    THEMES.iter().position(|t| *t == theme).unwrap_or(0) as i32
}

fn limits_to_ui(a: &Alerts) -> AlertLimits {
    AlertLimits {
        mem_watch: a.mem_watch_pct as i32,
        mem_danger: a.mem_danger_pct as i32,
        psi_mem: a.psi_mem as i32,
        psi_io: a.psi_io as i32,
        psi_cpu: a.psi_cpu as i32,
        cpu_culprit: a.cpu_culprit_min as i32,
    }
}

/// A spin box can never go below its minimum, but a typed value is still checked by `Alerts::normalize` (which `u32`
/// of a negative number would defeat: `max(0)` first).
fn limits_from_ui(l: &AlertLimits) -> Alerts {
    let n = |v: i32| v.max(0) as u32;
    Alerts {
        mem_watch_pct: n(l.mem_watch),
        mem_danger_pct: n(l.mem_danger),
        psi_mem: n(l.psi_mem),
        psi_io: n(l.psi_io),
        psi_cpu: n(l.psi_cpu),
        cpu_culprit_min: n(l.cpu_culprit),
    }
}

/// Normalizes, saves and applies new slowdown limits, then shows the clamped numbers back in the spin boxes.
fn set_alerts(weak: &slint::Weak<Main>, settings: &RefCell<Settings>, mut a: Alerts) {
    a.normalize();
    settings.borrow_mut().alerts = a;
    settings.borrow().save();
    APP.with_borrow_mut(|s| s.alerts = a);
    if let Some(ui) = weak.upgrade() {
        ui.set_limits(limits_to_ui(&a));
    }
}

/// Selects the language now. A locale with no translation (`de_DE`, or `en_US`) falls back to English explicitly:
/// Slint keeps the previous language on an error, which would leave Portuguese on screen after choosing System.
fn apply_language(settings: &Settings) {
    if slint::select_bundled_translation(&language(settings)).is_err() {
        let _ = slint::select_bundled_translation("en");
    }
}

/// `settings.language`, or the system locale (`LC_ALL`/`LC_MESSAGES`/`LANGUAGE`/`LANG`, minus `.UTF-8`).
fn language(settings: &Settings) -> String {
    if !settings.language.is_empty() {
        return settings.language.clone();
    }
    ["LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .map(|v| v.split(['.', ':']).next().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_closes_the_line_along_the_bottom_edge() {
        assert_eq!(area(&VecDeque::new(), 100.0).as_str(), "");
        // Part-filled history: the line starts at x = HIST - 2, the area must close back to that same x (no slant).
        let q: VecDeque<f64> = [0.0, 100.0].into();
        let a = area(&q, 100.0);
        assert!(a.starts_with("M 58.0 100.0 "), "{a}");
        assert!(a.ends_with("L 59 100 L 58 100 Z"), "{a}");
    }

    #[test]
    fn themes_and_their_indexes_agree() {
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(theme_index(t), i as i32);
        }
        assert_eq!(theme_index("bogus"), 0);
    }

    #[test]
    fn name_sort_uses_the_label_shown_on_a_group_row() {
        let proc = |pid, name: &str| Proc {
            pid,
            start: 0,
            name: name.into(),
            user: String::new(),
            cpu_pct: 0.0,
            mem_kb: 0,
            warn: None,
            state: 'S',
            ppid: 1,
            exe: None,
        };
        // A group labelled "Alpha" is led by Xorg: it must sort before "bash" by its label, not after it as "xorg".
        let (xorg, bash) = (proc(1, "Xorg"), proc(2, "bash"));
        let group = Node {
            p: &xorg,
            name: "Alpha",
            cpu: 0.0,
            mem_kb: 0,
        };
        let plain = Node {
            p: &bash,
            name: "bash",
            cpu: 0.0,
            mem_kb: 0,
        };
        assert_eq!(compare((0, true), &group, &plain), Ordering::Less);
        assert_eq!(compare((0, false), &group, &plain), Ordering::Greater);
    }

    #[test]
    fn only_printable_text_starts_a_filter() {
        assert!(printable("a") && printable("Z") && printable("é") && printable(" ") && printable("7"));
        // Delete, Backspace, Escape, Tab, Return.
        for key in ["\u{7f}", "\u{8}", "\u{1b}", "\t", "\n", ""] {
            assert!(!printable(key), "{key:?}");
        }
        // Slint's codes for the arrows, F-keys and Menu.
        for key in ["\u{f700}", "\u{f701}", "\u{f704}", "\u{f735}"] {
            assert!(!printable(key), "{key:?}");
        }
    }

    #[test]
    fn sizes_step_up_before_the_number_would_print_as_1024() {
        let s = |b: f64| scale(b);
        assert_eq!(s(0.0), (0, "0".into()));
        assert_eq!(s(1023.0), (0, "1023".into()));
        assert_eq!(s(1024.0), (1, "1".into()));
        // 1023.54 KB would print as "1024 KB".
        assert_eq!(s(1_048_100.0), (2, "1.0".into()), "not 1024 KB");
        assert_eq!(s(1_048_576.0), (2, "1.0".into()));
        // 1023.96 MB would print as "1024.0 MB".
        assert_eq!(s(1_073_700_000.0), (3, "1.00".into()), "not 1024.0 MB");
        assert_eq!(s(1_073_741_824.0), (3, "1.00".into()));
        // Just below the step: still the smaller unit.
        assert_eq!(s(1_048_000.0), (1, "1023".into()));
        assert_eq!(s(1_072_000_000.0), (2, "1022.3".into()));
    }

    #[test]
    fn clip_cuts_on_a_character_boundary() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "abc…");
        assert_eq!(clip("ééé", 2), "éé…");
    }
}
