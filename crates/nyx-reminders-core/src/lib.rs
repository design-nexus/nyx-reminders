//! Tasks, smart lists and local metadata for Nyx Reminders. No GTK.
//!
//! Tasks live in nyx-pim's SQLite task store (VTODO; the crate is imported as `nyx-eds`,
//! its older name), synced with the accounts' task lists.
//! What VTODO can't hold (flagged, manual order, a link back to a Mail message)
//! is kept in a small sidecar database, keyed by list and task uid.

mod sidecar;

pub use nyx_eds::When;
pub use sidecar::{Meta, Sidecar};

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use nyx_eds::Item;
use std::collections::HashMap;

/// What a list's backend can store (EDS capabilities and known limits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Due dates only, no times (Google Tasks).
    pub date_only: bool,
    pub alerts: bool,
    pub repeat: bool,
    pub priority: bool,
    /// Tags (VTODO CATEGORIES).
    pub tags: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self { date_only: false, alerts: true, repeat: true, priority: true, tags: true }
    }
}

/// Splits `#tag` words out of a line: the rest of the line, and the tags
/// (without `#`, in order, without repeats).
pub fn hashtags(text: &str) -> (String, Vec<String>) {
    let mut tags: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    for word in text.split_whitespace() {
        match word.strip_prefix('#').map(|t| t.trim_end_matches([',', '.', ';', ':', '!', '?'])) {
            Some(tag) if !tag.is_empty() && !tag.starts_with('#') => {
                if !tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                    tags.push(tag.to_string());
                }
            }
            _ => rest.push(word),
        }
    }
    (rest.join(" "), tags)
}

/// A task list in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListInfo {
    pub uid: String,
    pub name: String,
    pub color: Option<String>,
    pub selected: bool,
    /// The account or collection; `None` for this computer.
    pub group: Option<String>,
    pub account: Option<String>,
    pub needs_sign_in: bool,
    pub offline: bool,
    pub read_only: bool,
    pub caps: Caps,
}

impl ListInfo {
    pub fn group_label(&self) -> &str {
        self.group.as_deref().unwrap_or("On this computer")
    }
}

/// When a task is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Due {
    Day(NaiveDate),
    At(NaiveDateTime),
}

impl Due {
    pub fn day(self) -> NaiveDate {
        match self {
            Self::Day(d) => d,
            Self::At(t) => t.date(),
        }
    }

    pub fn when(self) -> When {
        match self {
            Self::Day(d) => When::Date { year: d.year(), month: d.month(), day: d.day() },
            Self::At(t) => When::Time(timestamp(t)),
        }
    }
}

/// One task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub list: String,
    pub uid: String,
    pub title: String,
    pub notes: String,
    pub url: Option<String>,
    pub due: Option<Due>,
    pub done: bool,
    /// 1 (highest) to 9; `None` for no priority.
    pub priority: Option<u8>,
    /// The parent task's uid, for subtasks.
    pub parent: Option<String>,
    pub rule: Option<String>,
    pub alarms: Vec<i64>,
    pub tags: Vec<String>,
}

impl Task {
    pub fn from_item(list: &str, item: &Item) -> Self {
        Self::from_item_in(list, item, &Local)
    }

    pub fn from_item_in<Tz: TimeZone>(list: &str, item: &Item, tz: &Tz) -> Self {
        let due = item.due.or(item.start).and_then(|w| match w {
            When::Date { year, month, day } => NaiveDate::from_ymd_opt(year, month, day).map(Due::Day),
            When::Time(secs) => DateTime::from_timestamp(secs, 0).map(|t| Due::At(t.with_timezone(tz).naive_local())),
        });
        Self {
            list: list.to_string(),
            uid: item.uid.clone(),
            title: if item.summary.trim().is_empty() { "Untitled".into() } else { item.summary.clone() },
            notes: item.description.clone(),
            url: item.url.clone(),
            due,
            done: item.is_done(),
            priority: item.priority,
            parent: item.parent.clone(),
            rule: item.rule.clone(),
            alarms: item.alarms.clone(),
            tags: item.categories.clone(),
        }
    }

    pub fn key(&self) -> (String, String) {
        (self.list.clone(), self.uid.clone())
    }

    pub fn overdue(&self, now: NaiveDateTime) -> bool {
        !self.done
            && match self.due {
                Some(Due::Day(d)) => d < now.date(),
                Some(Due::At(t)) => t < now,
                None => false,
            }
    }

    /// Priority as a mark: 1–4 high, 5 medium, 6–9 low.
    pub fn priority_level(&self) -> Option<Level> {
        match self.priority? {
            1..=4 => Some(Level::High),
            5 => Some(Level::Medium),
            _ => Some(Level::Low),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    High,
    Medium,
    Low,
}

impl Level {
    /// The VTODO PRIORITY value written for a level.
    pub fn value(self) -> u8 {
        match self {
            Self::High => 1,
            Self::Medium => 5,
            Self::Low => 9,
        }
    }
}

/// The lists every account's tasks are gathered into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Smart {
    /// Due today or overdue.
    Today,
    /// Has a due date.
    Scheduled,
    Flagged,
    /// Not done.
    All,
    Completed,
}

impl Smart {
    pub const ALL: [Smart; 5] = [Smart::Today, Smart::Scheduled, Smart::Flagged, Smart::All, Smart::Completed];

    pub fn id(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Scheduled => "scheduled",
            Self::Flagged => "flagged",
            Self::All => "all",
            Self::Completed => "completed",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Scheduled => "Scheduled",
            Self::Flagged => "Flagged",
            Self::All => "All",
            Self::Completed => "Completed",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn contains(self, task: &Task, meta: Option<&Meta>, today: NaiveDate) -> bool {
        match self {
            Self::Today => !task.done && task.due.is_some_and(|d| d.day() <= today),
            Self::Scheduled => !task.done && task.due.is_some(),
            Self::Flagged => !task.done && meta.is_some_and(|m| m.flagged),
            Self::All => !task.done,
            Self::Completed => task.done,
        }
    }
}

/// Orders tasks for a list: manual position first where set, then by due
/// date (undated last), priority and title; done tasks after open ones.
pub fn sort(tasks: &[Task], indices: &mut [usize], meta: &HashMap<(String, String), Meta>) {
    indices.sort_by(|a, b| {
        let (x, y) = (&tasks[*a], &tasks[*b]);
        let key = |t: &Task| {
            let m = meta.get(&t.key());
            (
                t.done,
                m.and_then(|m| m.position).unwrap_or(i64::MAX),
                t.due.is_none(),
                t.due,
                t.priority.unwrap_or(10),
                t.title.to_lowercase(),
            )
        };
        key(x).cmp(&key(y))
    });
}

/// Puts subtasks right after their parents, with their depth. Tasks whose
/// parent isn't in `indices` stay at the top level.
pub fn tree(tasks: &[Task], indices: &[usize]) -> Vec<(usize, usize)> {
    let present: std::collections::HashSet<&str> = indices.iter().map(|i| tasks[*i].uid.as_str()).collect();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for &i in indices {
        match tasks[i].parent.as_deref().filter(|p| present.contains(p) && *p != tasks[i].uid) {
            Some(parent) => children.entry(parent).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    fn walk<'a>(
        tasks: &'a [Task],
        i: usize,
        depth: usize,
        children: &HashMap<&'a str, Vec<usize>>,
        seen: &mut std::collections::HashSet<usize>,
        out: &mut Vec<(usize, usize)>,
    ) {
        if !seen.insert(i) {
            return;
        }
        out.push((i, depth));
        for &c in children.get(tasks[i].uid.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
            walk(tasks, c, depth + 1, children, seen, out);
        }
    }
    for r in roots {
        walk(tasks, r, 0, &children, &mut seen, &mut out);
    }
    // A cycle of parents leaves tasks unreached; list them at the top level.
    for &i in indices {
        if !seen.contains(&i) {
            walk(tasks, i, 0, &children, &mut seen, &mut out);
        }
    }
    out
}

/// Unix seconds for a local wall-clock time.
pub fn timestamp(at: NaiveDateTime) -> i64 {
    match Local.from_local_datetime(&at) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.timestamp(),
        chrono::LocalResult::None => {
            Local.from_local_datetime(&(at + chrono::Duration::hours(1))).earliest().map_or(0, |t| t.timestamp())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    pub(crate) fn task(uid: &str, due: Option<Due>, done: bool) -> Task {
        Task {
            list: "l".into(),
            uid: uid.into(),
            title: uid.into(),
            notes: String::new(),
            url: None,
            due,
            done,
            priority: None,
            parent: None,
            rule: None,
            alarms: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn smart_lists() {
        let today = d(21);
        let flagged = Meta { flagged: true, ..Meta::default() };
        let t = |due, done| task("x", due, done);
        assert!(Smart::Today.contains(&t(Some(Due::Day(d(20))), false), None, today));
        assert!(Smart::Today.contains(&t(Some(Due::At(d(21).and_hms_opt(18, 0, 0).unwrap())), false), None, today));
        assert!(!Smart::Today.contains(&t(Some(Due::Day(d(22))), false), None, today));
        assert!(Smart::Scheduled.contains(&t(Some(Due::Day(d(30))), false), None, today));
        assert!(!Smart::Scheduled.contains(&t(None, false), None, today));
        assert!(Smart::Flagged.contains(&t(None, false), Some(&flagged), today));
        assert!(!Smart::Flagged.contains(&t(None, true), Some(&flagged), today));
        assert!(Smart::All.contains(&t(None, false), None, today));
        assert!(Smart::Completed.contains(&t(None, true), None, today));
        assert!(!Smart::All.contains(&t(None, true), None, today));
    }

    #[test]
    fn overdue_and_priority() {
        let now = d(21).and_hms_opt(12, 0, 0).unwrap();
        assert!(task("a", Some(Due::Day(d(20))), false).overdue(now));
        assert!(!task("a", Some(Due::Day(d(21))), false).overdue(now));
        assert!(task("a", Some(Due::At(d(21).and_hms_opt(9, 0, 0).unwrap())), false).overdue(now));
        assert!(!task("a", Some(Due::Day(d(20))), true).overdue(now));
        let mut t = task("p", None, false);
        t.priority = Some(1);
        assert_eq!(t.priority_level(), Some(Level::High));
        t.priority = Some(5);
        assert_eq!(t.priority_level(), Some(Level::Medium));
        t.priority = Some(9);
        assert_eq!(t.priority_level(), Some(Level::Low));
    }

    #[test]
    fn sorting_and_tree() {
        let mut tasks = vec![
            task("undated", None, false),
            task("later", Some(Due::Day(d(25))), false),
            task("soon", Some(Due::Day(d(22))), false),
            task("done", Some(Due::Day(d(1))), true),
            task("child", None, false),
            task("pinned", None, false),
        ];
        tasks[4].parent = Some("later".into());
        let mut meta = HashMap::new();
        meta.insert(("l".to_string(), "pinned".to_string()), Meta { position: Some(0), ..Meta::default() });
        let mut idx: Vec<usize> = (0..tasks.len()).collect();
        sort(&tasks, &mut idx, &meta);
        let names: Vec<&str> = idx.iter().map(|i| tasks[*i].uid.as_str()).collect();
        assert_eq!(names, ["pinned", "soon", "later", "child", "undated", "done"]);
        let t: Vec<(&str, usize)> = tree(&tasks, &idx).into_iter().map(|(i, d)| (tasks[i].uid.as_str(), d)).collect();
        assert_eq!(t, [("pinned", 0), ("soon", 0), ("later", 0), ("child", 1), ("undated", 0), ("done", 0)]);
    }

    #[test]
    fn parent_cycles_still_list_everything() {
        let mut tasks = vec![task("a", None, false), task("b", None, false)];
        tasks[0].parent = Some("b".into());
        tasks[1].parent = Some("a".into());
        assert_eq!(tree(&tasks, &[0, 1]).len(), 2);
    }

    #[test]
    fn reads_items() {
        let item = Item {
            kind: nyx_eds::ItemKind::Task,
            uid: "u".into(),
            rid: None,
            summary: " ".into(),
            location: String::new(),
            description: "n".into(),
            start: None,
            end: None,
            due: Some(When::Date { year: 2026, month: 10, day: 30 }),
            status: nyx_eds::Status::NeedsAction,
            completed: None,
            priority: Some(1),
            url: None,
            parent: Some("p".into()),
            repeats: false,
            rule: None,
            has_alarms: false,
            alarms: Vec::new(),
            categories: Vec::new(),
            organizer: None,
            attendees: Vec::new(),
        };
        let t = Task::from_item_in("l", &item, &chrono::Utc);
        assert_eq!(t.title, "Untitled");
        assert_eq!(t.due, Some(Due::Day(d(30))));
        assert_eq!(t.parent.as_deref(), Some("p"));
        assert!(!t.done);
    }

    #[test]
    fn hashtags_split_out() {
        assert_eq!(hashtags("Call #work Sam #Work, #x"), ("Call Sam".into(), vec!["work".to_string(), "x".to_string()]));
        assert_eq!(hashtags("Room # 5 and ##double"), ("Room # 5 and ##double".into(), Vec::<String>::new()));
    }
}
