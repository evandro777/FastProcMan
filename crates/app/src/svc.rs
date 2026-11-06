//! The Services tab: filtering, sorting and selection of the unit list, and start/stop/restart.
use crate::{APP, Cmd, Main, ServiceRow, State, Tr, fixtures, size};
use fastprocman_core::{
    services::{self, Service, Status},
    sys::KillError,
};
use slint::{ComponentHandle, SharedString, VecModel};
use std::{cmp::Ordering, rc::Rc, sync::mpsc, thread};

pub struct Svc {
    list: Vec<Service>,
    /// Unit name: stable across re-sorts, and a unit has no PID to reuse.
    selected: Option<String>,
    /// Column and ascending; Name ascending to start.
    sort: (usize, bool),
    filter: String,
    /// 0 All, 1 Running, 2 Failed, 3 Stopped.
    status: usize,
    /// 0 All, 1 Enabled, 2 Disabled, 3 Static, 4 Masked.
    startup: usize,
    pub model: Rc<VecModel<ServiceRow>>,
    /// Unit names of the rows now in the table, in order.
    shown: Vec<String>,
}

impl Svc {
    pub fn new() -> Self {
        Svc {
            list: Vec::new(),
            selected: None,
            sort: (0, true),
            filter: String::new(),
            status: 0,
            startup: 0,
            model: Rc::new(VecModel::default()),
            shown: Vec::new(),
        }
    }
}

fn rank(s: Status) -> u8 {
    match s {
        Status::Failed => 0,
        Status::Running => 1,
        Status::Exited => 2,
        Status::Stopped => 3,
    }
}

fn compare((col, asc): (usize, bool), a: &Service, b: &Service) -> Ordering {
    let o = match col {
        0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        1 => a.pid.cmp(&b.pid),
        2 => a.user.cmp(&b.user),
        3 => a.cpu_pct.total_cmp(&b.cpu_pct),
        4 => a.mem_kb.cmp(&b.mem_kb),
        5 => rank(a.status).cmp(&rank(b.status)),
        _ => a.startup.cmp(&b.startup),
    };
    (if asc { o } else { o.reverse() }).then(a.name.cmp(&b.name))
}

/// Whether a service passes the name filter (`needle`, lowercased) and the two combo boxes. The numbers are the
/// positions of the `ComboBox` models in `main.slint`: change them together.
fn keep(v: &Service, needle: &str, status: usize, startup: usize) -> bool {
    (needle.is_empty() || v.name.to_lowercase().contains(needle))
        && match status {
            1 => v.status == Status::Running,
            2 => v.status == Status::Failed,
            3 => matches!(v.status, Status::Stopped | Status::Exited),
            _ => true,
        }
        && match startup {
            1 => v.enabled(),
            2 => v.startup == "disabled",
            3 => v.startup == "static",
            4 => v.startup == "masked",
            _ => true,
        }
}

struct View {
    model: Rc<VecModel<ServiceRow>>,
    rows: Vec<ServiceRow>,
    row: i32,
    label: SharedString,
    active: bool,
    count: SharedString,
}

fn view(s: &mut State, tr: &Tr) -> View {
    let needle = s.svc.filter.trim().to_lowercase();
    let (status, startup) = (s.svc.status, s.svc.startup);
    let mut list: Vec<&Service> = s
        .svc
        .list
        .iter()
        .filter(|v| keep(v, &needle, status, startup))
        .collect();
    let sort = s.svc.sort;
    list.sort_by(|a, b| compare(sort, a, b));

    let rows = list
        .iter()
        .map(|v| {
            let has_process = v.pid != 0;
            ServiceRow {
                // Every row is a .service: the suffix would only crowd the column.
                name: v.name.strip_suffix(".service").unwrap_or(&v.name).into(),
                pid: if has_process {
                    v.pid.to_string().into()
                } else {
                    SharedString::new()
                },
                user: v.user.as_str().into(),
                cpu: if has_process {
                    format!("{:.1}", s.cpu_shown(v.cpu_pct)).into()
                } else {
                    SharedString::new()
                },
                memory: if has_process {
                    size(tr, v.mem_kb as f64 * 1024.0)
                } else {
                    SharedString::new()
                },
                status: match v.status {
                    Status::Running => tr.invoke_state_running(),
                    Status::Exited => tr.invoke_state_exited(),
                    Status::Failed => tr.invoke_state_failed(),
                    Status::Stopped => tr.invoke_state_stopped(),
                },
                startup: match v.startup.as_str() {
                    st if st.starts_with("enabled") => tr.invoke_startup_enabled(),
                    "disabled" => tr.invoke_startup_disabled(),
                    "static" => tr.invoke_startup_static(),
                    "masked" => tr.invoke_startup_masked(),
                    other => other.into(),
                },
            }
        })
        .collect();
    let shown: Vec<String> = list.iter().map(|v| v.name.clone()).collect();
    let count = tr.invoke_count_services(shown.len() as i32);
    let pos = s.svc.selected.as_ref().and_then(|n| shown.iter().position(|x| x == n));
    if pos.is_none() {
        s.svc.selected = None;
    }
    let active = s
        .svc
        .selected
        .as_ref()
        .and_then(|n| s.svc.list.iter().find(|v| &v.name == n))
        .is_some_and(|v| matches!(v.status, Status::Running | Status::Exited));
    s.svc.shown = shown;
    View {
        model: s.svc.model.clone(),
        rows,
        row: pos.map_or(-1, |i| i as i32),
        label: s.svc.selected.clone().unwrap_or_default().into(),
        active,
        count,
    }
}

pub fn refresh(ui: &Main) {
    let v = APP.with_borrow_mut(|s| view(s, &ui.global::<Tr>()));
    v.model.set_vec(v.rows);
    // After the model, like the process table.
    ui.set_svc_selected_row(v.row);
    ui.set_svc_selected_label(v.label);
    ui.set_svc_selected_active(v.active);
    ui.set_svc_count_text(v.count);
}

pub fn on_services(ui: &Main, list: Vec<Service>) {
    APP.with_borrow_mut(|s| s.svc.list = list);
    refresh(ui);
}

/// Typing on the Services tab goes into its filter box, like `type_char` in `main.rs` does for the Processes tab.
pub fn type_char(ui: &Main, text: &str) -> bool {
    let Some(filter) = crate::push_typed(text, |s| &mut s.svc.filter) else {
        return false;
    };
    ui.set_svc_filter(filter.as_str().into());
    ui.invoke_svc_focus_filter(filter.len() as i32);
    refresh(ui);
    true
}

/// Wires the tab's callbacks. `tx` tells the sampler when the tab is shown, so it lists services only then.
pub fn connect(ui: &Main, tx: &mpsc::Sender<Cmd>) {
    ui.set_services(slint::ModelRc::from(APP.with_borrow(|s| s.svc.model.clone())));
    let weak = ui.as_weak();
    ui.on_svc_sort_ascending({
        let weak = weak.clone();
        move |col| {
            APP.with_borrow_mut(|s| s.svc.sort = (col as usize, true));
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_svc_sort_descending({
        let weak = weak.clone();
        move |col| {
            APP.with_borrow_mut(|s| s.svc.sort = (col as usize, false));
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_svc_filter_changed({
        let weak = weak.clone();
        move |t| {
            APP.with_borrow_mut(|s| s.svc.filter = t.to_string());
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_svc_status_changed({
        let weak = weak.clone();
        move |n| {
            APP.with_borrow_mut(|s| s.svc.status = n as usize);
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_svc_startup_changed({
        let weak = weak.clone();
        move |n| {
            APP.with_borrow_mut(|s| s.svc.startup = n as usize);
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_svc_row_selected({
        let weak = weak.clone();
        move |row| {
            APP.with_borrow_mut(|s| {
                s.svc.selected = usize::try_from(row).ok().and_then(|i| s.svc.shown.get(i).cloned());
            });
            weak.upgrade().iter().for_each(refresh);
        }
    });
    ui.on_services_shown({
        let tx = tx.clone();
        move |shown| _ = tx.send(Cmd::Services(shown))
    });
    for verb in ["start", "stop", "restart"] {
        let (weak, tx) = (weak.clone(), tx.clone());
        let f = move || weak.upgrade().iter().for_each(|ui| act(ui, &tx, verb));
        match verb {
            "start" => ui.on_start_service(f),
            "stop" => ui.on_stop_service(f),
            _ => ui.on_restart_service(f),
        }
    }
}

/// Start, stop or restart the selected unit. Stop and restart arrive here only after the confirmation dialog.
fn act(ui: &Main, tx: &mpsc::Sender<Cmd>, verb: &'static str) {
    // Fixtures use made-up units; never touch the real system for them.
    if fixtures::mode().is_some() {
        return;
    }
    let Some(unit) = APP.with_borrow(|s| s.svc.selected.clone()) else {
        // The unit left the list while the dialog was open: say so instead of doing nothing.
        ui.set_status(ui.global::<Tr>().invoke_status_exited());
        return;
    };
    let (weak, tx) = (ui.as_weak(), tx.clone());
    // systemctl waits for the polkit dialog, so this must never run on the UI thread.
    thread::spawn(move || {
        let result = services::control(verb, &unit);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let tr = ui.global::<Tr>();
            let label: SharedString = unit.as_str().into();
            ui.set_status(match result {
                Ok(()) => match verb {
                    "start" => tr.invoke_status_service_started(label),
                    "stop" => tr.invoke_status_service_stopped(label),
                    _ => tr.invoke_status_service_restarted(label),
                },
                Err(KillError::Cancelled) => tr.invoke_status_cancelled(),
                Err(KillError::Failed(e)) => tr.invoke_status_failed(e.into()),
                Err(_) => tr.invoke_status_exited(),
            });
        });
        let _ = tx.send(Cmd::Now);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, status: Status, startup: &str) -> Service {
        Service {
            name: name.into(),
            status,
            startup: startup.into(),
            pid: 0,
            user: String::new(),
            cpu_pct: 0.0,
            mem_kb: 0,
        }
    }

    #[test]
    fn combo_positions_pick_the_right_services() {
        let run = svc("Cron.service", Status::Running, "enabled");
        let done = svc("boot.service", Status::Exited, "static");
        let bad = svc("bad.service", Status::Failed, "disabled");
        let off = svc("off.service", Status::Stopped, "masked");
        let pick = |needle: &str, status, startup| {
            [&run, &done, &bad, &off]
                .into_iter()
                .filter(|v| keep(v, needle, status, startup))
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(pick("", 0, 0).len(), 4);
        assert_eq!(pick("", 1, 0), ["Cron.service"]);
        assert_eq!(pick("", 2, 0), ["bad.service"]);
        // Stopped includes Exited.
        assert_eq!(pick("", 3, 0), ["boot.service", "off.service"]);
        assert_eq!(pick("", 0, 1), ["Cron.service"]);
        assert_eq!(pick("", 0, 2), ["bad.service"]);
        assert_eq!(pick("", 0, 3), ["boot.service"]);
        assert_eq!(pick("", 0, 4), ["off.service"]);
        // The name filter is case-insensitive (the needle arrives lowercased) and combines with the boxes.
        assert_eq!(pick("cron", 0, 0), ["Cron.service"]);
        assert!(pick("cron", 2, 0).is_empty());
    }
}
