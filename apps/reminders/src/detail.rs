//! The selected task's details, in the inspector at the right. Every field
//! shows all the time (greyed, with the reason, where the list can't keep it
//! or a date comes first); every change saves at once.

use crate::actions::{self, Quick};
use crate::widgets;
use crate::window;
use chrono::{Datelike, NaiveDate, NaiveTime, Timelike};
use gtk::prelude::*;
use nyx_eds::Draft;
use nyx_reminders_core::{Due, Level, Meta, Task};
use std::rc::Rc;

pub const WIDTH: i32 = 300;

pub const REPEATS: &[(&str, &str)] = &[
    ("", "Never"),
    ("FREQ=DAILY", "Every day"),
    ("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", "Every weekday"),
    ("FREQ=WEEKLY", "Every week"),
    ("FREQ=WEEKLY;INTERVAL=2", "Every 2 weeks"),
    ("FREQ=MONTHLY", "Every month"),
    ("FREQ=YEARLY", "Every year"),
];

/// A group's heading over its card.
fn heading(panel: &gtk::Box, title: &str, count: Option<String>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("detail-heading");
    let label = widgets::label(title, "");
    label.set_xalign(0.0);
    row.append(&label);
    if let Some(c) = count {
        row.append(&widgets::label(&c, "detail-count"));
    }
    panel.append(&row);
}

fn card(panel: &gtk::Box) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("detail-card");
    panel.append(&card);
    card
}

/// A row in a card: an icon, what it is, and its control at the right.
fn row(card: &gtk::Box, icon: &str, text: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("detail-crow");
    row.append(&gtk::Image::from_icon_name(icon));
    let label = widgets::label(text, "detail-label");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    control.set_valign(gtk::Align::Center);
    control.as_ref().update_relation(&[gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])]);
    row.append(control);
    card.append(&row);
    row
}

/// Greys a row out and says why, or brings it back.
fn why_not(row: &gtk::Box, reason: Option<&str>) {
    row.set_sensitive(reason.is_none());
    row.set_tooltip_text(reason);
}

fn day_text(day: NaiveDate) -> String {
    actions::due_text(Due::Day(day), actions::now())
}

/// Saves an entry's text when Enter is pressed or it loses focus.
fn on_commit(entry: &gtk::Entry, save: impl Fn(&gtk::Entry) + 'static) {
    let save = Rc::new(save);
    let s = save.clone();
    entry.connect_activate(move |e| s(e));
    let focus = gtk::EventControllerFocus::new();
    let e = entry.clone();
    focus.connect_leave(move |_| save(&e));
    entry.add_controller(focus);
}

/// Builds the details for `task` into `panel` and its foot into `foot` (both emptied first).
pub fn fill(panel: &gtk::Box, foot: &gtk::Box, task: &Task, meta: &Meta, subtasks: &[Task]) {
    for b in [panel, foot] {
        while let Some(child) = b.first_child() {
            b.remove(&child);
        }
    }
    let caps = window::list_caps(&task.list);
    let lists = window::lists();
    let read_only = lists.iter().find(|l| l.uid == task.list).is_none_or(|l| l.read_only);
    let task = Rc::new(task.clone());

    // The title with its checkbox, then the notes.
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    top.add_css_class("detail-top");
    let check = gtk::CheckButton::new();
    check.add_css_class("task-check");
    check.set_active(task.done);
    check.set_valign(gtk::Align::Center);
    check.set_sensitive(!read_only);
    check.set_tooltip_text(Some(if task.done { "Reopen (Space)" } else { "Complete (Space)" }));
    {
        let t = task.clone();
        check.connect_toggled(move |c| actions::set_done(&t, c.is_active()));
    }
    top.append(&check);
    let title = gtk::Entry::new();
    title.add_css_class("detail-title");
    title.set_hexpand(true);
    title.update_property(&[gtk::accessible::Property::Label("Title")]);
    title.set_text(&task.title);
    title.set_sensitive(!read_only);
    {
        let t = task.clone();
        on_commit(&title, move |e| {
            let text = e.text().trim().to_string();
            if !text.is_empty() && text != t.title {
                actions::save(&t, Draft { summary: Some(text), ..Draft::default() });
            }
        });
    }
    top.append(&title);
    panel.append(&top);

    let notes = gtk::TextView::new();
    notes.add_css_class("detail-notes");
    notes.update_property(&[gtk::accessible::Property::Label("Notes")]);
    notes.set_wrap_mode(gtk::WrapMode::WordChar);
    notes.buffer().set_text(&task.notes);
    notes.set_editable(!read_only);
    {
        let t = task.clone();
        let focus = gtk::EventControllerFocus::new();
        let n = notes.clone();
        focus.connect_leave(move |_| {
            let b = n.buffer();
            let text = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
            if text != t.notes {
                actions::save(&t, Draft { description: Some(text), ..Draft::default() });
            }
        });
        notes.add_controller(focus);
    }
    let notes_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .min_content_height(56)
        .max_content_height(200)
        .propagate_natural_height(true)
        .child(&notes)
        .build();
    notes_scroll.add_css_class("detail-notes-box");
    panel.append(&notes_scroll);

    if read_only {
        let note = widgets::label("This list is read only.", "dim");
        note.set_xalign(0.0);
        panel.append(&note);
        return;
    }
    let no_date = task.due.is_none().then_some("Add a date first");

    // When: date, time, repeat, alert.
    heading(panel, "When", None);
    let when = card(panel);
    let day = task.due.map(|d| d.day()).unwrap_or_else(|| actions::now().date());
    let date_button = gtk::MenuButton::new();
    date_button.add_css_class("detail-pick");
    date_button.set_label(&task.due.map(|d| day_text(d.day())).unwrap_or_else(|| "Add a date".into()));
    let calendar = gtk::Calendar::new();
    if let Ok(dt) = glib::DateTime::from_local(day.year(), day.month() as i32, day.day() as i32, 0, 0, 0.0) {
        calendar.select_day(&dt);
    }
    let pop = gtk::Popover::new();
    let pop_box = widgets::vbox(6);
    pop_box.append(&calendar);
    let clear = gtk::Button::with_label("No date");
    clear.set_visible(task.due.is_some());
    pop_box.append(&clear);
    pop.set_child(Some(&pop_box));
    date_button.set_popover(Some(&pop));
    row(&when, "x-office-calendar-symbolic", "Date", &date_button);
    {
        let (t, p) = (task.clone(), pop.clone());
        calendar.connect_day_selected(move |cal| {
            let picked = cal.date();
            let Some(day) = NaiveDate::from_ymd_opt(picked.year(), picked.month() as u32, picked.day_of_month() as u32) else {
                return;
            };
            let due = match t.due {
                Some(Due::At(at)) if !caps.date_only => Due::At(day.and_time(at.time())),
                _ => Due::Day(day),
            };
            p.popdown();
            actions::save(&t, Draft { due: Some(Some(due.when())), ..Draft::default() });
        });
        let (t, p) = (task.clone(), pop.clone());
        clear.connect_clicked(move |_| {
            p.popdown();
            actions::save(&t, Draft { due: Some(None), alarms: Some(Vec::new()), ..Draft::default() });
        });
    }

    // Time: all day, or a time on the half hour (the current one kept if it's another).
    let mut times: Vec<(String, String)> = vec![("".into(), "All day".into())];
    for half in 12..46 {
        let t = NaiveTime::from_hms_opt(half / 2, (half % 2) * 30, 0).unwrap_or_default();
        times.push((t.format("%H:%M").to_string(), actions::time_text(t)));
    }
    let current_time = match task.due {
        Some(Due::At(at)) => at.time().format("%H:%M").to_string(),
        _ => String::new(),
    };
    if !times.iter().any(|(id, _)| *id == current_time)
        && let Some(Due::At(at)) = task.due
    {
        let pos = times.iter().position(|(id, _)| !id.is_empty() && *id > current_time).unwrap_or(times.len());
        times.insert(pos, (current_time.clone(), actions::time_text(at.time())));
    }
    let time_dd = widgets::dropdown(&times, &current_time);
    let time_row = row(&when, "preferences-system-time-symbolic", "Time", &time_dd);
    why_not(&time_row, if caps.date_only { Some("This list keeps dates only") } else { no_date });
    {
        let t = task.clone();
        time_dd.connect_selected_notify(move |d| {
            let Some(day) = t.due.map(|d| d.day()) else { return };
            let Some((id, _)) = times.get(d.selected() as usize).cloned() else { return };
            let due = match NaiveTime::parse_from_str(&id, "%H:%M") {
                Ok(at) => Due::At(day.and_hms_opt(at.hour(), at.minute(), 0).unwrap_or(day.and_time(at))),
                Err(_) => Due::Day(day),
            };
            if Some(due) != t.due {
                actions::save(&t, Draft { due: Some(Some(due.when())), ..Draft::default() });
            }
        });
    }

    let mut repeats: Vec<(String, String)> = REPEATS.iter().map(|(r, l)| (r.to_string(), l.to_string())).collect();
    let current_rule = task.rule.clone().unwrap_or_default();
    if !current_rule.is_empty() && !repeats.iter().any(|(r, _)| *r == current_rule) {
        repeats.push((current_rule.clone(), "Custom".into()));
    }
    let repeat_dd = widgets::dropdown(&repeats, &current_rule);
    let repeat_row = row(&when, "media-playlist-repeat-symbolic", "Repeat", &repeat_dd);
    why_not(&repeat_row, if !caps.repeat { Some("This list can't keep a repeat") } else { no_date });
    {
        let t = task.clone();
        repeat_dd.connect_selected_notify(move |d| {
            let Some((rule, _)) = repeats.get(d.selected() as usize).cloned() else { return };
            if rule != t.rule.clone().unwrap_or_default() {
                actions::save(&t, Draft { repeat: Some(Some(rule).filter(|r| !r.is_empty())), ..Draft::default() });
            }
        });
    }

    let date_only = !matches!(task.due, Some(Due::At(_)));
    let mut alerts = vec![("none".to_string(), "None".to_string())];
    if date_only {
        for hour in [8i64, 9, 12, 17, 20] {
            let at = actions::time_text(NaiveTime::from_hms_opt(hour as u32, 0, 0).unwrap_or_default());
            alerts.push(((-hour * 60).to_string(), format!("On the day at {at}")));
        }
    } else {
        for (m, l) in [(0i64, "At the time"), (10, "10 min before"), (60, "1 hour before"), (1440, "1 day before")] {
            alerts.push((m.to_string(), l.to_string()));
        }
    }
    let current_alert = task.alarms.first().map(|m| m.to_string()).unwrap_or_else(|| "none".into());
    if !alerts.iter().any(|(id, _)| *id == current_alert) {
        alerts.push((current_alert.clone(), "Custom".into()));
    }
    let alert_dd = widgets::dropdown(&alerts, &current_alert);
    let alert_row = row(&when, "alarm-symbolic", "Alert", &alert_dd);
    why_not(&alert_row, if !caps.alerts { Some("This list can't keep alerts") } else { no_date });
    {
        let t = task.clone();
        alert_dd.connect_selected_notify(move |d| {
            let Some((id, _)) = alerts.get(d.selected() as usize).cloned() else { return };
            let alarms: Vec<i64> = id.parse().ok().into_iter().collect();
            if alarms != t.alarms {
                actions::save(&t, Draft { alarms: Some(alarms), ..Draft::default() });
            }
        });
    }

    // Details: list (moving it), priority, flag, tags, link.
    heading(panel, "Details", None);
    let details = card(panel);
    let writable: Vec<(String, String)> =
        lists.iter().filter(|l| !l.read_only).map(|l| (l.uid.clone(), l.name.clone())).collect();
    let list_dd = widgets::dropdown(&writable, &task.list);
    let list_row = row(&details, "view-list-symbolic", "List", &list_dd);
    why_not(&list_row, task.parent.is_some().then_some("A subtask moves with its reminder"));
    {
        let (t, subs) = (task.clone(), subtasks.to_vec());
        list_dd.connect_selected_notify(move |d| {
            let Some((uid, _)) = writable.get(d.selected() as usize).cloned() else { return };
            if uid != t.list {
                window::select(None);
                actions::move_to(&t, subs.clone(), uid);
            }
        });
    }

    let current = match task.priority_level() {
        Some(Level::High) => "high",
        Some(Level::Medium) => "medium",
        Some(Level::Low) => "low",
        None => "none",
    };
    let t = task.clone();
    let seg = widgets::segmented(
        &widgets::opts(&[("none", "None"), ("low", "Low"), ("medium", "Med"), ("high", "High")]),
        current,
        move |id| {
            let value = match id.as_str() {
                "high" => Some(Level::High.value()),
                "medium" => Some(Level::Medium.value()),
                "low" => Some(Level::Low.value()),
                _ => None,
            };
            if value != t.priority {
                actions::save(&t, Draft { priority: Some(value), ..Draft::default() });
            }
        },
    );
    seg.add_css_class("detail-seg");
    let prio_row = row(&details, "emblem-important-symbolic", "Priority", &seg);
    why_not(&prio_row, (!caps.priority).then_some("This list can't keep a priority"));

    let flag = gtk::Switch::new();
    flag.set_active(meta.flagged);
    {
        let t = task.clone();
        flag.connect_active_notify(move |s| window::set_flag(&t, s.is_active()));
    }
    row(&details, "nyx-reminders-flag-symbolic", "Flagged", &flag);

    let tags = gtk::Entry::new();
    tags.add_css_class("detail-inline");
    tags.set_placeholder_text(Some("Add a tag"));
    tags.set_text(&task.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
    tags.set_tooltip_text(Some("Tags, separated by spaces or commas (also #tag when adding)"));
    tags.set_width_chars(12);
    {
        let t = task.clone();
        on_commit(&tags, move |e| {
            let mut picked: Vec<String> = Vec::new();
            for tag in e.text().split([',', ' ']).map(|t| t.trim().trim_start_matches('#')).filter(|t| !t.is_empty()) {
                if !picked.iter().any(|p| p.eq_ignore_ascii_case(tag)) {
                    picked.push(tag.to_string());
                }
            }
            if picked != t.tags {
                actions::save(&t, Draft { categories: Some(picked), ..Draft::default() });
            }
        });
    }
    let tags_row = row(&details, "tag-symbolic", "Tags", &tags);
    why_not(&tags_row, (!caps.tags).then_some("This list can't keep tags"));

    let link = gtk::Entry::new();
    link.add_css_class("detail-inline");
    link.set_placeholder_text(Some("Add a link"));
    link.set_width_chars(12);
    link.set_text(task.url.as_deref().unwrap_or(""));
    {
        let t = task.clone();
        on_commit(&link, move |e| {
            let text = e.text().trim().to_string();
            if Some(text.as_str()) != t.url.as_deref().or(Some("")) {
                actions::save(&t, Draft { url: Some(Some(text).filter(|u| !u.is_empty())), ..Draft::default() });
            }
        });
    }
    row(&details, "insert-link-symbolic", "Link", &link);

    // Subtasks, each opening; a subtask names its reminder instead.
    if let Some(parent) = task.parent.as_deref() {
        let parent_task = window::tasks().into_iter().find(|p| p.uid == parent && p.list == task.list);
        if let Some(p) = parent_task {
            heading(panel, "Subtask of", None);
            let c = card(panel);
            let open = gtk::Button::with_label(&p.title);
            open.add_css_class("flat");
            open.add_css_class("detail-link");
            let key = p.key();
            open.connect_clicked(move |_| window::select(Some(key.clone())));
            c.append(&open);
        }
    } else {
        let done = subtasks.iter().filter(|s| s.done).count();
        heading(panel, "Subtasks", (!subtasks.is_empty()).then(|| format!("{done} of {}", subtasks.len())));
        let c = card(panel);
        for sub in subtasks {
            let r = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            r.add_css_class("detail-subrow");
            let check = gtk::CheckButton::new();
            check.add_css_class("task-check");
            check.set_active(sub.done);
            let s = sub.clone();
            check.connect_toggled(move |c| actions::set_done(&s, c.is_active()));
            r.append(&check);
            let open = gtk::Button::new();
            open.add_css_class("flat");
            open.add_css_class("detail-subopen");
            open.set_hexpand(true);
            let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = widgets::label(&sub.title, if sub.done { "done" } else { "" });
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            inner.append(&label);
            inner.append(&gtk::Image::from_icon_name("go-next-symbolic"));
            open.set_child(Some(&inner));
            open.set_tooltip_text(Some("Open this subtask"));
            let key = sub.key();
            open.connect_clicked(move |_| window::select(Some(key.clone())));
            r.append(&open);
            c.append(&r);
        }
        let add = gtk::Entry::new();
        add.add_css_class("detail-subadd");
        add.set_placeholder_text(Some("Add a subtask"));
        add.set_primary_icon_name(Some("list-add-symbolic"));
        add.update_property(&[gtk::accessible::Property::Label("Add a subtask")]);
        let t = task.clone();
        add.connect_activate(move |e| {
            let text = e.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            e.set_text("");
            let quick = actions::parse_quick(&text, actions::now(), caps, None);
            let quick = Quick { title: quick.title, due: quick.due, repeat: None, tags: quick.tags, dropped: Vec::new() };
            actions::add(t.list.clone(), quick, Some(t.uid.clone()), None, |result| {
                if let Err(err) = result {
                    window::toast(&err);
                }
            });
        });
        c.append(&add);
    }

    // From: the Mail message it was made from.
    if let Some(source) = meta.source.as_deref() {
        heading(panel, "From", None);
        let open = gtk::Button::new();
        open.add_css_class("flat");
        open.add_css_class("detail-link");
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        inner.append(&gtk::Image::from_icon_name("mail-unread-symbolic"));
        inner.append(&gtk::Label::new(Some("Open the message in Mail")));
        open.set_child(Some(&inner));
        let uri = source.to_string();
        open.connect_clicked(move |_| {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(&uri, gtk::gio::AppLaunchContext::NONE);
        });
        panel.append(&open);
    }

    // The foot: saving is automatic; Delete is named.
    let saved = widgets::label("Saved as you type", "detail-saved");
    saved.set_hexpand(true);
    saved.set_xalign(0.0);
    foot.append(&saved);
    let delete = gtk::Button::new();
    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    inner.append(&gtk::Image::from_icon_name("user-trash-symbolic"));
    inner.append(&gtk::Label::new(Some("Delete")));
    delete.set_child(Some(&inner));
    delete.add_css_class("destructive");
    delete.add_css_class("detail-delete");
    delete.set_tooltip_text(Some("Delete (Delete)"));
    let t = task.clone();
    let subs: Vec<String> = subtasks.iter().map(|s| s.uid.clone()).collect();
    delete.connect_clicked(move |_| {
        window::select(None);
        actions::delete(&t, subs.clone());
    });
    foot.append(&delete);
}
