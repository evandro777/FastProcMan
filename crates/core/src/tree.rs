//! Process tree: turns the parent/child links into the rows to show, in order.
use crate::sys::{Proc, Reason, severity};
use std::{
    cmp::Ordering,
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
};

/// A process with the CPU and memory of itself plus all its descendants. It is the sort key in tree mode, so
/// expanding a row (which switches the value shown to its own) never moves it. For an application group, `p` is its
/// first process and `name` the application's.
pub struct Node<'a> {
    pub p: &'a Proc,
    pub name: &'a str,
    pub cpu: f32,
    pub mem_kb: u64,
}

/// Key of a group row, in the `(pid, start)` shape of the processes: PID 0 is never a real process, so the two never
/// meet. `""` is the "Processes" group. `DefaultHasher::new()` is deterministic, so the key is stable across ticks.
pub fn group_key(name: &str) -> (u32, u64) {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    (0, h.finish())
}

pub struct Row {
    /// `(pid, start)` of the process; `group_key` of the name for a group row.
    pub key: (u32, u64),
    /// `Some(application name)` for a group row (`""` is the "Processes" group, which holds what no launcher claims).
    pub group: Option<String>,
    /// Index into the `procs` slice given to `build`.
    pub idx: usize,
    pub depth: usize,
    /// Has children that are shown when expanded.
    pub has_kids: bool,
    pub expanded: bool,
    /// What the row shows: the subtree total while collapsed, the process alone otherwise.
    pub cpu: f32,
    pub mem_kb: u64,
    /// The worst strong warning (see `Reason::is_strong`) among the processes this row hides: its subtree while
    /// collapsed, its tops while a group is closed. `None` when open, or when nothing hidden is flagged.
    pub hidden_warn: Option<Reason>,
}

pub struct Tree {
    pub rows: Vec<Row>,
    /// Parent of every process in the tree (not only the shown ones), as an index into `procs`.
    pub parent: Vec<Option<usize>>,
}

/// Rows to show for `procs`. `is_open` says whether a process with children is expanded. `filter` is `None` for no
/// filter; with a filter only the matches and their ancestors are listed, and those ancestors count as expanded so a
/// match is never hidden inside a collapsed parent. `cmp` orders siblings (it should end with a PID tie-break).
///
/// `app` names the application a process is the main program of (`None` for most). A process whose application
/// differs from the one it already sits in starts a group of its own, cut out of its parent (a program started from
/// another one, like Code from Files, is not part of it). Groups are listed by `cmp` with the rest under "Processes"
/// last; `group_open` says whether a group row (by name, `""` for Processes) is expanded. With no application at all
/// there are no group rows, only the forest.
pub fn build<'n>(
    procs: &[Proc],
    app: &dyn Fn(&Proc) -> Option<&'n str>,
    group_open: &dyn Fn(&str) -> bool,
    is_open: &dyn Fn(&Proc) -> bool,
    filter: Option<&dyn Fn(&Proc) -> bool>,
    cmp: &dyn Fn(&Node, &Node) -> Ordering,
) -> Tree {
    let n = procs.len();
    let by_pid: std::collections::HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut kids_all: Vec<Vec<usize>> = vec![vec![]; n];
    let mut roots = vec![];
    for (i, p) in procs.iter().enumerate() {
        match by_pid.get(&p.ppid) {
            Some(&j) if j != i => kids_all[j].push(i),
            _ => roots.push(i),
        }
    }

    // A spanning forest by DFS with a seen set. A PID reused within one sample can make A and B each other's
    // parent: neither is a root, so the second pass over every index starts a tree at the first unseen one.
    // Walking parent links from any other node could loop forever, which is why nothing does.
    let mut seen = vec![false; n];
    let mut parent = vec![None; n];
    let mut kids: Vec<Vec<usize>> = vec![vec![]; n];
    let mut order = Vec::with_capacity(n);
    let mut stack = vec![];
    for r in roots.iter().copied().chain(0..n) {
        if seen[r] {
            continue;
        }
        seen[r] = true;
        stack.push(r);
        while let Some(i) = stack.pop() {
            order.push(i);
            for &c in &kids_all[i] {
                if !seen[c] {
                    seen[c] = true;
                    parent[c] = Some(i);
                    kids[i].push(c);
                    stack.push(c);
                }
            }
        }
    }

    // Cut every process that starts another application from its parent. `parent` is cut too (not only `kids`): the
    // caller walks it to move a hidden selection to a shown ancestor, and that must never land in another app.
    // `eff` is the application a process sits in: its own, else its parent's.
    let own: Vec<Option<&str>> = procs.iter().map(app).collect();
    let mut eff = own.clone();
    for &i in &order {
        if let Some(pa) = parent[i] {
            if own[i].is_some() && own[i] != eff[pa] {
                kids[pa].retain(|&k| k != i);
                parent[i] = None;
            } else {
                eff[i] = own[i].or(eff[pa]);
            }
        }
    }

    // `order` lists a parent before its children, so walking it backwards finishes every child before its parent.
    let mut cpu: Vec<f32> = procs.iter().map(|p| p.cpu_pct).collect();
    let mut rss: Vec<u64> = procs.iter().map(|p| p.mem_kb).collect();
    let mut keep = vec![true; n];
    if let Some(f) = filter {
        keep = procs.iter().map(f).collect();
    }
    // Worst strong warning in each subtree (itself included), for rows that hide it.
    let worse = |a: Option<Reason>, b: Option<Reason>| if severity(b) > severity(a) { b } else { a };
    let mut bad: Vec<Option<Reason>> = procs.iter().map(|p| p.warn.filter(|w| w.is_strong())).collect();
    for &i in order.iter().rev() {
        if let Some(pa) = parent[i] {
            cpu[pa] += cpu[i];
            rss[pa] += rss[i];
            bad[pa] = worse(bad[pa], bad[i]);
            if keep[i] {
                keep[pa] = true;
            }
        }
    }

    let sorted = |v: &[usize]| {
        let mut v: Vec<usize> = v.iter().copied().filter(|&i| keep[i]).collect();
        let node = |i: usize| Node {
            p: &procs[i],
            name: &procs[i].name,
            cpu: cpu[i],
            mem_kb: rss[i],
        };
        v.sort_by(|&a, &b| cmp(&node(a), &node(b)));
        v
    };

    // The tops of the forest, by application (`None` = what no launcher claims).
    let mut by_app: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for i in (0..n).filter(|&i| parent[i].is_none()) {
        by_app.entry(eff[i]).or_default().push(i);
    }
    // (name, tops in order, their total CPU, their total memory)
    let mut groups: Vec<(Option<&str>, Vec<usize>, f32, u64)> = by_app
        .into_iter()
        .map(|(name, tops)| {
            let tops = sorted(&tops);
            let (c, r) = tops.iter().fold((0.0, 0), |(c, r), &i| (c + cpu[i], r + rss[i]));
            (name, tops, c, r)
        })
        .filter(|g| !g.1.is_empty())
        .collect();
    groups.sort_by(|a, b| match (a.0, b.0) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(an), Some(bn)) => {
            let node = |g: &(Option<&str>, Vec<usize>, f32, u64), name| Node {
                p: &procs[g.1[0]],
                name,
                cpu: g.2,
                mem_kb: g.3,
            };
            cmp(&node(a, an), &node(b, bn))
        }
    });

    let flat = groups.iter().all(|g| g.0.is_none());
    let mut rows = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = vec![];
    for (name, tops, c, r) in &groups {
        let base = if flat {
            0
        } else {
            let label = name.unwrap_or("");
            let open = filter.is_some() || group_open(label);
            rows.push(Row {
                key: group_key(label),
                group: Some(label.to_string()),
                // The first process supplies the application's icon.
                idx: tops[0],
                depth: 0,
                has_kids: true,
                expanded: open,
                cpu: *c,
                mem_kb: *r,
                hidden_warn: if open {
                    None
                } else {
                    tops.iter().fold(None, |w, &i| worse(w, bad[i]))
                },
            });
            if !open {
                continue;
            }
            1
        };
        stack.extend(tops.iter().rev().map(|&i| (i, base)));
        while let Some((i, depth)) = stack.pop() {
            let p = &procs[i];
            let has_kids = kids[i].iter().any(|&k| keep[k]);
            let open = has_kids && (filter.is_some() || is_open(p));
            let (c, r) = if has_kids && !open {
                (cpu[i], rss[i])
            } else {
                (p.cpu_pct, p.mem_kb)
            };
            rows.push(Row {
                key: (p.pid, p.start),
                group: None,
                idx: i,
                depth,
                has_kids,
                expanded: open,
                cpu: c,
                mem_kb: r,
                hidden_warn: if has_kids && !open { bad[i] } else { None },
            });
            if open {
                stack.extend(sorted(&kids[i]).into_iter().rev().map(|k| (k, depth + 1)));
            }
        }
    }
    Tree { rows, parent }
}

/// Where a selection lands after the table was rebuilt.
#[derive(Debug, PartialEq, Eq)]
pub struct Followed {
    /// Index into `shown`.
    pub row: usize,
    /// Index into `procs` of the shown ancestor that took the selection over; `None` when the selected process is
    /// itself still shown.
    pub moved_to: Option<usize>,
}

/// Follows the selected `(pid, start)` across a rebuild. `shown` is the key of every row, `parent` is `Tree::parent`.
/// A selection that is still shown stays. With `walk` (tree mode, no filter) one hidden inside a collapsed parent moves
/// to the nearest shown ancestor, so End, Kill and Properties keep a target. A selection hidden by a filter, or gone,
/// gives `None`. `parent` is a forest (see `build`), so the walk ends.
pub fn follow_selection(
    shown: &[(u32, u64)],
    procs: &[Proc],
    parent: &[Option<usize>],
    selected: (u32, u64),
    walk: bool,
) -> Option<Followed> {
    if let Some(row) = shown.iter().position(|k| *k == selected) {
        return Some(Followed { row, moved_to: None });
    }
    if !walk {
        return None;
    }
    let mut at = procs.iter().position(|p| (p.pid, p.start) == selected);
    while let Some(i) = at {
        at = parent.get(i).copied().flatten();
        if let Some(a) = at
            && let Some(row) = shown.iter().position(|k| *k == (procs[a].pid, procs[a].start))
        {
            return Some(Followed { row, moved_to: Some(a) });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `super::build` with no applications, so the plain-forest tests stay as they were.
    fn build(
        procs: &[Proc],
        is_open: &dyn Fn(&Proc) -> bool,
        filter: Option<&dyn Fn(&Proc) -> bool>,
        cmp: &dyn Fn(&Node, &Node) -> Ordering,
    ) -> Tree {
        super::build(procs, &|_| None, &|_| true, is_open, filter, cmp)
    }

    fn p(pid: u32, ppid: u32, cpu: f32, rss: u64) -> Proc {
        Proc {
            pid,
            start: pid as u64,
            name: format!("p{pid}"),
            user: "u".into(),
            cpu_pct: cpu,
            mem_kb: rss,
            warn: None,
            state: 'S',
            ppid,
            exe: None,
        }
    }

    /// Memory descending, like the app's default.
    fn by_mem(a: &Node, b: &Node) -> Ordering {
        b.mem_kb.cmp(&a.mem_kb).then(a.p.pid.cmp(&b.p.pid))
    }

    fn pids(t: &Tree, procs: &[Proc]) -> Vec<u32> {
        t.rows.iter().map(|r| procs[r.idx].pid).collect()
    }

    fn closed(_: &Proc) -> bool {
        false
    }

    fn all_open(_: &Proc) -> bool {
        true
    }

    /// Only these PIDs are expanded.
    fn open(pids: &[u32]) -> impl Fn(&Proc) -> bool {
        let pids = pids.to_vec();
        move |p| pids.contains(&p.pid)
    }

    // 1 -> {2 -> {4}, 3}
    fn sample() -> Vec<Proc> {
        vec![p(1, 0, 1.0, 10), p(2, 1, 2.0, 20), p(3, 1, 3.0, 5), p(4, 2, 4.0, 40)]
    }

    #[test]
    fn all_collapsed_shows_the_roots_with_subtree_totals() {
        let procs = sample();
        let t = build(&procs, &closed, None, &by_mem);
        assert_eq!(pids(&t, &procs), [1]);
        let r = &t.rows[0];
        assert!(r.has_kids && !r.expanded);
        assert_eq!((r.mem_kb, r.cpu), (75, 10.0));
    }

    #[test]
    fn expanding_shows_children_and_the_parent_goes_back_to_its_own_values() {
        let procs = sample();
        let t = build(&procs, &open(&[1]), None, &by_mem);
        // Child 2 (subtree 60 kB) sorts before child 3 (5 kB).
        assert_eq!(pids(&t, &procs), [1, 2, 3]);
        assert_eq!(t.rows[0].mem_kb, 10);
        assert_eq!(t.rows[1].mem_kb, 60);
        assert_eq!((t.rows[1].depth, t.rows[2].depth), (1, 1));
    }

    #[test]
    fn all_open_lists_every_process_and_parents_show_their_own_values() {
        let procs = sample();
        let t = build(&procs, &all_open, None, &by_mem);
        // 1, then its children by subtree memory (2 holds 60 kB, 3 holds 5 kB), then 2's child.
        assert_eq!(pids(&t, &procs), [1, 2, 4, 3]);
        assert_eq!(t.rows.iter().map(|r| r.depth).collect::<Vec<_>>(), [0, 1, 2, 1]);
        assert_eq!(t.rows.iter().map(|r| r.mem_kb).collect::<Vec<_>>(), [10, 20, 40, 5]);
        assert!(t.rows.iter().all(|r| r.has_kids == r.expanded));
    }

    #[test]
    fn expanding_a_row_does_not_change_its_place() {
        let procs = vec![p(1, 0, 0.0, 1), p(2, 0, 0.0, 50), p(3, 2, 0.0, 1), p(4, 1, 0.0, 100)];
        // Root 1 holds 101 kB in total, root 2 holds 51: 1 comes first whether or not it is expanded.
        assert_eq!(pids(&build(&procs, &closed, None, &by_mem), &procs), [1, 2]);
        assert_eq!(pids(&build(&procs, &open(&[1]), None, &by_mem), &procs), [1, 4, 2]);
    }

    #[test]
    fn a_match_deep_down_shows_its_ancestors_expanded() {
        let procs = sample();
        let only4 = |p: &Proc| p.pid == 4;
        let t = build(&procs, &closed, Some(&only4), &by_mem);
        assert_eq!(pids(&t, &procs), [1, 2, 4]);
        assert!(t.rows[0].expanded && t.rows[1].expanded && !t.rows[2].has_kids);
        // Ancestors show their own values, not totals.
        assert_eq!(t.rows[0].mem_kb, 10);
    }

    #[test]
    fn nothing_matching_gives_no_rows() {
        let procs = sample();
        let none = |_: &Proc| false;
        assert!(build(&procs, &closed, Some(&none), &by_mem).rows.is_empty());
    }

    #[test]
    fn an_orphan_becomes_a_root() {
        let procs = vec![p(1, 0, 0.0, 1), p(7, 999, 0.0, 2)];
        let t = build(&procs, &closed, None, &by_mem);
        assert_eq!(pids(&t, &procs), [7, 1]);
    }

    #[test]
    fn a_parent_cycle_ends_and_keeps_both_processes() {
        let procs = vec![p(1, 2, 0.0, 1), p(2, 1, 0.0, 2)];
        let t = build(&procs, &open(&[1, 2]), None, &by_mem);
        let mut shown = pids(&t, &procs);
        shown.sort();
        assert_eq!(shown, [1, 2]);
    }

    #[test]
    fn a_process_that_is_its_own_parent_is_a_root() {
        let procs = vec![p(5, 5, 0.0, 1)];
        assert_eq!(pids(&build(&procs, &closed, None, &by_mem), &procs), [5]);
    }

    // 1 -> {2 (A) -> {3, 5 (B)}, 4 (A)}: two instances of A, and B started from inside A.
    fn apps() -> Vec<Proc> {
        vec![
            p(1, 0, 1.0, 10),
            p(2, 1, 2.0, 20),
            p(3, 2, 3.0, 30),
            p(4, 1, 4.0, 40),
            p(5, 2, 5.0, 50),
        ]
    }

    fn app_of(p: &Proc) -> Option<&'static str> {
        match p.pid {
            2 | 4 => Some("A"),
            5 => Some("B"),
            _ => None,
        }
    }

    fn grouped<'a>(procs: &'a [Proc], open: bool, filter: Option<&dyn Fn(&Proc) -> bool>) -> Tree {
        super::build(procs, &app_of, &|_| open, &|_| true, filter, &by_mem)
    }

    /// Group rows by name, process rows by PID.
    fn labels(t: &Tree, procs: &[Proc]) -> Vec<String> {
        let name = |r: &Row| r.group.clone().unwrap_or_else(|| procs[r.idx].pid.to_string());
        t.rows.iter().map(name).collect()
    }

    #[test]
    fn applications_become_groups_with_the_rest_under_processes() {
        let procs = apps();
        let t = grouped(&procs, true, None);
        // A holds 2+3 and 4 (90 kB), B 50 kB, and Processes (last) only process 1: B is cut out of A, A out of 1.
        // (The Processes group's name is empty; the UI supplies its label.)
        assert_eq!(labels(&t, &procs), ["A", "2", "3", "4", "B", "5", "", "1"]);
        assert_eq!(
            t.rows.iter().map(|r| r.depth).collect::<Vec<_>>(),
            [0, 1, 2, 1, 0, 1, 0, 1]
        );
        assert_eq!((t.rows[0].cpu, t.rows[0].mem_kb), (9.0, 90));
        assert_eq!(t.rows[0].key, group_key("A"));
        assert_ne!(t.rows[0].key, t.rows[6].key);
        assert_eq!(t.rows[1].key, (2, 2));
    }

    #[test]
    fn a_program_started_from_another_has_no_parent_for_the_selection_walk() {
        let procs = apps();
        let t = grouped(&procs, false, None);
        // Only the group rows when closed, and nothing in a group leads out of it: 2, 4 and 5 start groups.
        assert_eq!(labels(&t, &procs), ["A", "B", ""]);
        assert_eq!(t.parent, [None, None, Some(1), None, None]);
    }

    #[test]
    fn a_match_shows_only_its_group_open() {
        let procs = apps();
        let only3 = |p: &Proc| p.pid == 3;
        let t = grouped(&procs, false, Some(&only3));
        assert_eq!(labels(&t, &procs), ["A", "2", "3"]);
        assert!(t.rows[0].expanded);
    }

    #[test]
    fn without_any_application_there_are_no_group_rows() {
        let procs = sample();
        let t = super::build(&procs, &|_| None, &|_| false, &all_open, None, &by_mem);
        assert!(t.rows.iter().all(|r| r.group.is_none()));
        assert_eq!(pids(&t, &procs), [1, 2, 4, 3]);
    }

    #[test]
    fn a_strong_warning_shows_on_the_collapsed_row_and_closed_group_that_hide_it() {
        let mut procs = apps();
        procs[2].warn = Some(Reason::TempDir); // 3, inside A under 2
        procs[3].warn = Some(Reason::NameMismatch); // 4, in A: weak, never reported
        let warns = |t: &Tree| t.rows.iter().map(|r| r.hidden_warn).collect::<Vec<_>>();
        // Group closed: A reports it, the other groups do not.
        let t = grouped(&procs, false, None);
        assert_eq!(warns(&t), [Some(Reason::TempDir), None, None]);
        // Open, but 2 collapsed: the group says nothing any more, row 2 reports its child.
        let t = super::build(&procs, &app_of, &|_| true, &closed, None, &by_mem);
        assert_eq!(labels(&t, &procs), ["A", "2", "4", "B", "5", "", "1"]);
        assert_eq!(warns(&t)[..3], [None, Some(Reason::TempDir), None]);
        // Everything open: nothing is hidden.
        assert!(warns(&grouped(&procs, true, None)).iter().all(Option::is_none));
    }

    #[test]
    fn parents_are_reported_for_hidden_rows() {
        let procs = sample();
        let t = build(&procs, &closed, None, &by_mem);
        assert_eq!(t.parent, [None, Some(0), Some(0), Some(1)]);
    }

    #[test]
    fn a_selection_hidden_in_a_collapsed_parent_moves_to_the_nearest_shown_ancestor() {
        let procs = sample(); // 1 -> {2 -> {4}, 3}
        let keys = |t: &Tree| t.rows.iter().map(|r| r.key).collect::<Vec<_>>();
        let key = |pid: u32| (pid, pid as u64);
        // Only 1 is open: rows 1, 2, 3; 4 sits inside the collapsed 2, which is the nearest shown ancestor.
        let t = build(&procs, &open(&[1]), None, &by_mem);
        let shown = keys(&t);
        assert_eq!(
            follow_selection(&shown, &procs, &t.parent, key(4), true),
            Some(Followed {
                row: 1,
                moved_to: Some(1)
            })
        );
        // Everything closed: only 1 is left, two levels up.
        let t = build(&procs, &closed, None, &by_mem);
        assert_eq!(
            follow_selection(&keys(&t), &procs, &t.parent, key(4), true),
            Some(Followed {
                row: 0,
                moved_to: Some(0)
            })
        );
        // Still shown: stays, nothing moves.
        let t = build(&procs, &all_open, None, &by_mem);
        let row = pids(&t, &procs).iter().position(|p| *p == 4).unwrap();
        assert_eq!(
            follow_selection(&keys(&t), &procs, &t.parent, key(4), true),
            Some(Followed { row, moved_to: None })
        );
    }

    #[test]
    fn a_selection_hidden_by_a_filter_or_gone_is_dropped() {
        let procs = sample();
        let only3 = |p: &Proc| p.pid == 3;
        let t = build(&procs, &all_open, Some(&only3), &by_mem);
        let shown: Vec<_> = t.rows.iter().map(|r| r.key).collect();
        // The caller passes `walk = false` while filtering: 4 must not jump to a parent.
        assert_eq!(follow_selection(&shown, &procs, &t.parent, (4, 4), false), None);
        // A process that exited (or whose PID was reused: other start time) has no row and no parent.
        assert_eq!(follow_selection(&shown, &procs, &t.parent, (99, 99), true), None);
        assert_eq!(follow_selection(&shown, &procs, &t.parent, (3, 0), true), None);
    }
}
