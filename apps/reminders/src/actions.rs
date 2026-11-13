//! Changes to tasks, run on the EDS worker: add, complete (with Undo), change,
//! delete (with Undo), and the parse of a quick-add line.

use crate::prefs;
use crate::window;
use crate::worker;
use chrono::{Local, NaiveDateTime, NaiveTime};
use nyx_eds::{Draft, Scope};
use nyx_reminders_core::{Caps, Due, Task};

/// What a quick-add line says, fitted to what the list can keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quick {
    pub title: String,
    pub due: Option<Due>,
    pub repeat: Option<String>,
    /// `#tag` words.
    pub tags: Vec<String>,
    /// Parts the list can't keep (shown as a note).
    pub dropped: Vec<&'static str>,
}

pub fn parse_quick(text: &str, now: NaiveDateTime, caps: Caps, default_day: Option<chrono::NaiveDate>) -> Quick {
    let (text, mut tags) = nyx_reminders_core::hashtags(text);
    let text = text.as_str();
    let parse = nyx_when::parse(text, now);
    let mut dropped = Vec::new();
    if !tags.is_empty() && !caps.tags {
        tags.clear();
        dropped.push("tag");
    }
    let date = parse.date.or(default_day);
    let mut due = match (date, parse.start) {
        (Some(day), Some(time)) => Some(Due::At(day.and_time(time))),
        (Some(day), None) => Some(Due::Day(day)),
        (None, _) => None,
    };
    if caps.date_only
        && let Some(Due::At(t)) = due
    {
        due = Some(Due::Day(t.date()));
        dropped.push("time");
    }
    let mut repeat = parse.repeat.clone();
    if repeat.is_some() && !caps.repeat {
        repeat = None;
        dropped.push("repeat");
    }
    let title = if parse.title.trim().is_empty() { text.trim().to_string() } else { parse.title.trim().to_string() };
    Quick { title, due, repeat, tags, dropped }
}

/// Adds a task. `parent` makes it a subtask.
pub fn add(
    list: String,
    quick: Quick,
    parent: Option<String>,
    notes: Option<String>,
    after: impl FnOnce(Result<String, String>) + 'static,
) {
    // A due time reminds at (or before) that time; a due day at the usual hour.
    let p = prefs::get();
    let alarms = quick.due.and_then(|due| match due {
        Due::At(_) => p.timed_alert.map(|m| vec![m]),
        Due::Day(_) => p.day_alerts.then(|| vec![-(p.default_hour as i64 * 60)]),
    });
    let caps = window::list_caps(&list);
    let draft = Draft {
        summary: Some(quick.title),
        due: quick.due.map(|d| Some(d.when())),
        repeat: quick.repeat.map(Some),
        parent: parent.map(Some),
        description: notes,
        alarms: alarms.filter(|_| caps.alerts),
        categories: Some(quick.tags).filter(|t| !t.is_empty() && caps.tags),
        ..Draft::default()
    };
    worker::ask(move || worker::with_list(&list, |l| l.create(&draft)), after);
}

/// Marks a task done (a repeating one moves to its next due date) or not done.
pub fn set_done(task: &Task, done: bool) {
    let task = task.clone();
    let title = task.title.clone();
    let repeats = task.rule.is_some();
    worker::ask(
        move || {
            worker::with_list(&task.list, |l| {
                let backup = l.export(&task.uid)?;
                if done {
                    l.complete(&task.uid)?;
                } else {
                    l.modify(&task.uid, None, Scope::All, &Draft { done: Some(false), ..Draft::default() })?;
                }
                Ok((task.list.clone(), backup))
            })
        },
        move |result| match result {
            Ok((list, backup)) if done => {
                let message =
                    if repeats { format!("“{title}” moved to its next date.") } else { format!("Completed “{title}”.") };
                window::toast_undo(&message, move || restore(list.clone(), backup.clone()));
            }
            Ok(_) => {}
            Err(err) => window::toast(&err),
        },
    );
}

/// Applies changes to a task.
pub fn save(task: &Task, draft: Draft) {
    if draft == Draft::default() {
        return;
    }
    let task = task.clone();
    worker::ask(
        move || worker::with_list(&task.list, |l| l.modify(&task.uid, None, Scope::All, &draft)),
        |result| {
            if let Err(err) = result {
                window::toast(&err);
            }
        },
    );
}

/// Deletes a task and its subtasks, with Undo.
pub fn delete(task: &Task, subtasks: Vec<String>) {
    let task = task.clone();
    let title = task.title.clone();
    worker::ask(
        move || {
            worker::with_list(&task.list, |l| {
                let mut backups = Vec::new();
                for uid in subtasks.iter().chain(std::iter::once(&task.uid)) {
                    backups.push(l.export(uid)?);
                    l.remove(uid, None, Scope::All)?;
                }
                Ok((task.list.clone(), backups))
            })
        },
        move |result| match result {
            Ok((list, backups)) => window::toast_undo(&format!("Deleted “{title}”."), move || {
                for backup in backups.iter().rev() {
                    restore(list.clone(), backup.clone());
                }
            }),
            Err(err) => window::toast(&err),
        },
    );
}

/// What `task` is, as a draft for another list: only the parts `caps` can keep.
fn draft_for(task: &Task, caps: Caps, parent: Option<String>) -> Draft {
    let due = task.due.map(|d| match d {
        Due::At(t) if caps.date_only => Due::Day(t.date()),
        other => other,
    });
    Draft {
        summary: Some(task.title.clone()),
        description: Some(task.notes.clone()).filter(|n| !n.is_empty()),
        due: due.map(|d| Some(d.when())),
        repeat: task.rule.clone().filter(|_| caps.repeat).map(Some),
        priority: task.priority.filter(|_| caps.priority).map(Some),
        url: task.url.clone().map(Some),
        categories: Some(task.tags.clone()).filter(|t| !t.is_empty() && caps.tags),
        alarms: Some(task.alarms.clone()).filter(|a| !a.is_empty() && caps.alerts),
        parent: parent.map(Some),
        ..Draft::default()
    }
}

/// Moves a reminder (and its subtasks) to another list: made there, then
/// removed here. Parts the other list can't keep are left behind, and said.
pub fn move_to(task: &Task, subtasks: Vec<Task>, target: String) {
    if target == task.list {
        return;
    }
    let (task, caps) = (task.clone(), window::list_caps(&target));
    let name = window::lists().iter().find(|l| l.uid == target).map(|l| l.name.clone()).unwrap_or_default();
    let lost: Vec<&str> = [
        (task.rule.is_some() && !caps.repeat, "repeat"),
        (task.priority.is_some() && !caps.priority, "priority"),
        (!task.tags.is_empty() && !caps.tags, "tags"),
        (matches!(task.due, Some(Due::At(_))) && caps.date_only, "time"),
    ]
    .into_iter()
    .filter_map(|(lost, what)| lost.then_some(what))
    .collect();
    worker::ask(
        move || {
            let uid = worker::with_list(&target, |l| l.create(&draft_for(&task, caps, None)))?;
            for sub in &subtasks {
                worker::with_list(&target, |l| l.create(&draft_for(sub, caps, Some(uid.clone()))))?;
            }
            worker::with_list(&task.list, |l| {
                for s in subtasks.iter().map(|s| &s.uid).chain(std::iter::once(&task.uid)) {
                    l.remove(s, None, Scope::All)?;
                }
                Ok(())
            })
        },
        move |result: Result<(), String>| match result {
            Ok(()) if lost.is_empty() => window::toast(&format!("Moved to {name}.")),
            Ok(()) => window::toast(&format!("Moved to {name}, without its {} ({name} can't keep them).", lost.join(" and "))),
            Err(err) => window::toast(&err),
        },
    );
}

/// "Every week", "Every weekday", "Every 3 months"…: a rule in words.
pub fn repeat_text(rule: &str) -> String {
    if let Some((_, l)) = crate::detail::REPEATS.iter().find(|(r, _)| *r == rule) {
        return l.to_string();
    }
    let part = |k: &str| rule.split(';').find_map(|p| p.strip_prefix(&format!("{k}=")).map(str::to_string));
    let every = part("INTERVAL").and_then(|n| n.parse::<u32>().ok()).unwrap_or(1);
    let unit = match part("FREQ").as_deref() {
        Some("DAILY") => "day",
        Some("WEEKLY") => "week",
        Some("MONTHLY") => "month",
        Some("YEARLY") => "year",
        _ => return "Repeats".into(),
    };
    if every == 1 { format!("Every {unit}") } else { format!("Every {every} {unit}s") }
}

fn restore(list: String, ics: String) {
    worker::ask(
        move || worker::with_list(&list, |l| l.import_all(&ics)),
        |result| {
            if let Err(err) = result {
                window::toast(&err);
            }
        },
    );
}

/// "Today", "Tomorrow 9 AM", "Fri", "Oct 30", "Oct 30, 2027".
pub fn due_text(due: Due, now: NaiveDateTime) -> String {
    let today = now.date();
    let day = due.day();
    let days = (day - today).num_days();
    let date = match days {
        0 => "Today".to_string(),
        1 => "Tomorrow".to_string(),
        -1 => "Yesterday".to_string(),
        2..=6 => day.format("%a").to_string(),
        _ if day.format("%Y").to_string() == today.format("%Y").to_string() => day.format("%b %-d").to_string(),
        _ => day.format("%b %-d, %Y").to_string(),
    };
    match due {
        Due::Day(_) => date,
        Due::At(t) => format!("{date} {}", time_text(t.time())),
    }
}

pub fn time_text(t: NaiveTime) -> String {
    if prefs::twelve_hour() {
        let h = chrono::Timelike::hour(&t) % 12;
        let h = if h == 0 { 12 } else { h };
        let m = chrono::Timelike::minute(&t);
        let m = if m == 0 { String::new() } else { format!(":{m:02}") };
        format!("{h}{m} {}", if chrono::Timelike::hour(&t) < 12 { "AM" } else { "PM" })
    } else {
        t.format("%H:%M").to_string()
    }
}

pub fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(day: u32, h: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, 0, 0).unwrap()
    }

    #[test]
    fn quick_add_fits_the_list() {
        let full = Caps::default();
        let q = parse_quick("Call Sam tomorrow at 9", at(21, 10), full, None);
        assert_eq!((q.title.as_str(), q.due), ("Call Sam", Some(Due::At(at(22, 9)))));
        let google = Caps { date_only: true, alerts: false, repeat: false, priority: false, tags: false };
        let q = parse_quick("Water plants every monday at 8", at(21, 10), google, None);
        assert_eq!(q.due, Some(Due::Day(NaiveDate::from_ymd_opt(2026, 10, 26).unwrap())));
        assert_eq!(q.repeat, None);
        assert_eq!(q.dropped, vec!["time", "repeat"]);
        // In Today, an undated line is due today.
        let q = parse_quick("Buy milk", at(21, 10), full, Some(at(21, 0).date()));
        assert_eq!(q.due, Some(Due::Day(at(21, 0).date())));
        assert_eq!(q.title, "Buy milk");
    }

    #[test]
    fn quick_add_takes_tags() {
        let q = parse_quick("Pay rent #bills #Home tomorrow", at(21, 10), Caps::default(), None);
        assert_eq!(q.title, "Pay rent");
        assert_eq!(q.tags, ["bills", "Home"]);
        let google = Caps { tags: false, ..Caps::default() };
        let q = parse_quick("Pay rent #bills", at(21, 10), google, None);
        assert!(q.tags.is_empty());
        assert_eq!(q.dropped, vec!["tag"]);
    }

    #[test]
    fn due_texts() {
        let now = at(21, 10);
        let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
        assert_eq!(due_text(Due::Day(d(21)), now), "Today");
        assert_eq!(due_text(Due::Day(d(22)), now), "Tomorrow");
        assert_eq!(due_text(Due::Day(d(20)), now), "Yesterday");
        assert_eq!(due_text(Due::Day(d(23)), now), "Fri");
        assert_eq!(due_text(Due::Day(d(30)), now), "Oct 30");
        assert_eq!(due_text(Due::Day(NaiveDate::from_ymd_opt(2027, 1, 2).unwrap()), now), "Jan 2, 2027");
    }
}
