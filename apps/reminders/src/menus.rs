//! Right-click menus: reminders and task lists. Each item does what its key,
//! the detail panel or the sidebar already do; controls a list can't store
//! are left out, as in the detail panel.

use crate::widgets::{self, MenuEntry, danger, item, keyed, separator, sub};
use crate::{actions, prefs, window, worker};
use chrono::{Datelike, Duration};
use gtk::prelude::*;
use nyx_eds::Draft;
use nyx_reminders_core::{Due, ListInfo, Task};

/// Moves a reminder to another day, keeping its time when it has one.
fn due_on(task: &Task, day: chrono::NaiveDate) -> Due {
    match task.due {
        Some(Due::At(at)) if !window::list_caps(&task.list).date_only => Due::At(day.and_time(at.time())),
        _ => Due::Day(day),
    }
}

/// A reminder's menu (it was just selected).
pub fn task(widget: &gtk::Widget, x: f64, y: f64, task: &Task) {
    let read_only = window::lists().iter().find(|l| l.uid == task.list).is_none_or(|l| l.read_only);
    let caps = window::list_caps(&task.list);
    let mut out: Vec<MenuEntry> = Vec::new();
    if !read_only {
        let t = task.clone();
        out.push(keyed(if task.done { "Reopen" } else { "Complete" }, "Space", move || actions::set_done(&t, !t.done)).with_icon("emblem-ok-symbolic"));
        let (t, flagged) = (task.clone(), window::meta_of(&task.key()).flagged);
        out.push(keyed(if flagged { "Unflag" } else { "Flag" }, "S", move || window::set_flag(&t, !flagged)).with_icon("nyx-reminders-flag-symbolic"));
        out.push(separator());

        let today = actions::now().date();
        let monday = today + Duration::days((7 - today.weekday().num_days_from_monday()) as i64);
        let mut due: Vec<MenuEntry> = [("Today", today), ("Tomorrow", today + Duration::days(1)), ("Next Monday", monday)]
            .into_iter()
            .map(|(label, day)| {
                let t = task.clone();
                item(label, move || {
                    actions::save(&t, Draft { due: Some(Some(due_on(&t, day).when())), ..Draft::default() });
                })
            })
            .collect();
        if task.due.is_some() {
            let t = task.clone();
            due.push(separator());
            due.push(item("No date", move || actions::save(&t, Draft { due: Some(None), ..Draft::default() })));
        }
        out.push(sub("Due", due).with_icon("x-office-calendar-symbolic"));
        if caps.priority {
            out.push(sub(
                "Priority",
                [("High", Some(1u8)), ("Medium", Some(5)), ("Low", Some(9)), ("None", None)]
                    .into_iter()
                    .map(|(label, value)| {
                        let t = task.clone();
                        item(label, move || actions::save(&t, Draft { priority: Some(value), ..Draft::default() }))
                    })
                    .collect(),
            ).with_icon("emblem-important-symbolic"));
        }
        // Another list it can go to (a subtask moves with its reminder).
        let others: Vec<ListInfo> =
            window::lists().into_iter().filter(|l| !l.read_only && l.uid != task.list).collect();
        if task.parent.is_none() && !others.is_empty() {
            let moves = others
                .into_iter()
                .map(|l| {
                    let t = task.clone();
                    item(&l.name, move || {
                        let subs: Vec<Task> = window::tasks()
                            .into_iter()
                            .filter(|s| s.parent.as_deref() == Some(t.uid.as_str()) && s.list == t.list)
                            .collect();
                        window::select(None);
                        actions::move_to(&t, subs, l.uid.clone());
                    })
                })
                .collect();
            out.push(sub("Move to list", moves).with_icon("view-list-symbolic"));
        }
    }
    if let Some(url) = task.url.clone().filter(|u| !u.trim().is_empty()) {
        out.push(separator());
        out.push(item("Open link", move || {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(&url, gtk::gio::AppLaunchContext::NONE);
        }));
    }
    let title = task.title.clone();
    out.push(separator());
    out.push(
        keyed("Copy", "Ctrl C", move || {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&title);
                window::toast("Copied.");
            }
        })
        .with_icon("edit-copy-symbolic"),
    );
    if !read_only {
        let t = task.clone();
        out.push(danger("Delete", Some("Del"), move || {
            let subs: Vec<String> = window::tasks()
                .iter()
                .filter(|s| s.parent.as_deref() == Some(t.uid.as_str()) && s.list == t.list)
                .map(|s| s.uid.clone())
                .collect();
            window::select(None);
            actions::delete(&t, subs);
        }).with_icon("user-trash-symbolic"));
    }
    widgets::popup_menu(widget, x, y, out);
}

/// Colors offered for a list (EDS saves them; providers that keep colors sync them).
const COLORS: &[(&str, &str)] = &[
    ("Red", "#e5484d"),
    ("Orange", "#f76b15"),
    ("Yellow", "#ffc53d"),
    ("Green", "#30a46c"),
    ("Teal", "#12a594"),
    ("Blue", "#3e63dd"),
    ("Purple", "#8e4ec6"),
    ("Pink", "#d6409f"),
    ("Gray", "#8b8d98"),
];

/// A task list in the sidebar.
pub fn attach_list(button: &gtk::Button, list: &ListInfo) {
    let list = list.clone();
    widgets::on_right_click(button, move |w, x, y| {
        let mut out: Vec<MenuEntry> = Vec::new();
        let uid = list.uid.clone();
        out.push(item("Open", move || window::show_page(window::Page::List(uid.clone()))));
        if !list.read_only {
            let (uid, name) = (list.uid.clone(), list.name.clone());
            let is_default = window::default_list() == list.uid;
            if !is_default {
                out.push(item("New reminders go here", move || {
                    prefs::update(|p| p.default_list = uid.clone());
                    window::toast(&format!("New reminders from smart lists go to {name}."));
                }));
            }
        }
        out.push(sub(
            "Color",
            COLORS
                .iter()
                .map(|(name, hex)| {
                    let (uid, hex) = (list.uid.clone(), hex.to_string());
                    item(name, move || {
                        worker::set_color(uid.clone(), hex.clone(), |r| {
                            if let Err(e) = r {
                                window::toast(&e);
                            }
                        })
                    })
                })
                .collect(),
        ));
        out.push(separator());
        let (uid, name) = (list.uid.clone(), list.name.clone());
        out.push(item("Turn off", move || {
            let (uid, name) = (uid.clone(), name.clone());
            worker::set_enabled(uid.clone(), false, move |r| match r {
                Ok(()) => window::toast_undo(&format!("{name} is off. Turn it back on in Settings → Task lists."), move || {
                    worker::set_enabled(uid.clone(), true, |_| {})
                }),
                Err(e) => window::toast(&e),
            });
        }));
        widgets::popup_menu(w, x, y, out);
    });
}
