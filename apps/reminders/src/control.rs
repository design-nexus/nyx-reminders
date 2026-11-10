//! Reminders' tools on the control interface (`nyx_core::control`), for Nyx
//! Assistant and other Nyx apps (Mail's "Remind me"): list, read and search
//! reminders, and add, complete, change or delete them. Assistant asks before
//! anything that changes a list; task text is returned as untrusted content.

use crate::actions::{self, Quick};
use crate::window;
use crate::worker;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use nyx_core::control::{Reply, text};
use nyx_eds::{Draft, Scope};
use nyx_reminders_core::{Due, Smart, Task};
use serde_json::{Value, json};

const MAX_TASKS: usize = 200;

pub fn tools() -> Value {
    let due =
        json!({"type": "string", "description": "YYYY-MM-DD, local YYYY-MM-DDTHH:MM, RFC 3339, or words like \"tomorrow at 9\""});
    json!([
        {"name": "reminders.lists", "description": "The task lists: name, account, how many are open, read-only.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "reminders.today", "description": "Reminders due today or overdue.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "reminders.list", "description": "Open reminders in a list (by uid from reminders.lists), or every open one; done ones too when asked.", "inputSchema": {"type": "object", "properties": {"list": {"type": "string"}, "include_done": {"type": "boolean"}}}},
        {"name": "reminders.search", "description": "Reminders whose title or notes contain the words.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "reminders.create", "description": "Add a reminder. The title may say when (\"Call Sam tomorrow at 9\"); due overrides it.", "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}, "due": due, "notes": {"type": "string"}, "list": {"type": "string"}, "source": {"type": "string", "description": "A link back to what it's about, such as a nyx-mail: message"}}, "required": ["title"]}},
        {"name": "reminders.complete", "description": "Mark a reminder done (a repeating one moves to its next date), or not done with done=false.", "inputSchema": {"type": "object", "properties": {"list": {"type": "string"}, "uid": {"type": "string"}, "done": {"type": "boolean"}}, "required": ["list", "uid"]}},
        {"name": "reminders.edit", "description": "Change a reminder's title, notes or due date.", "inputSchema": {"type": "object", "properties": {"list": {"type": "string"}, "uid": {"type": "string"}, "title": {"type": "string"}, "notes": {"type": "string"}, "due": due}, "required": ["list", "uid"]}},
        {"name": "reminders.delete", "description": "Delete a reminder.", "inputSchema": {"type": "object", "properties": {"list": {"type": "string"}, "uid": {"type": "string"}}, "required": ["list", "uid"]}},
    ])
}

/// API 2: the tools with what they risk and how they're typed, the app, and the
/// `today` topic Glimpse's Reminders panel shows. D-Bus starts Reminders without its
/// window for a call.
pub fn description(app_id: &str) -> Value {
    let meta = [
        ("reminders.lists", json!({"risk": "read_only", "result": "object"})),
        ("reminders.today", json!({"risk": "read_only", "aliases": ["due", "overdue"], "result": "object"})),
        ("reminders.list", json!({"risk": "read_only", "aliases": ["all"], "result": "object"})),
        ("reminders.search", json!({"risk": "read_only", "aliases": ["find"], "positional": ["query"], "result": "object"})),
        ("reminders.create", json!({"risk": "writes", "aliases": ["add", "new"], "positional": ["title"], "result": "object"})),
        ("reminders.complete", json!({"risk": "writes", "aliases": ["done"], "result": "object"})),
        ("reminders.edit", json!({"risk": "writes", "result": "object"})),
        ("reminders.delete", json!({"risk": "destructive", "result": "object"})),
    ];
    let mut list = tools();
    for t in list.as_array_mut().into_iter().flatten() {
        if let Some((_, m)) = meta.iter().find(|(n, _)| t["name"] == *n) {
            t["x-nyx"] = m.clone();
        }
    }
    if let Some(a) = list.as_array_mut() {
        a.push(json!({"name": "reminders.open", "description": "Open Reminders' window.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"title": "Open Reminders"}, "x-nyx": {"risk": "writes", "confirmation": "never", "result": "none"}}));
    }
    json!({
        "tools": list,
        "nyx": {"api": 2, "app": app_id, "name": "Reminders", "icon": crate::APP_ID, "namespaces": ["reminders"], "aliases": ["todo", "tasks"],
                "bus_name": app_id, "activation": "dbus", "default": "reminders.search", "open": {"action": "reminders.open"},
                "topics": [{"name": "today", "sensitive": true}],
                "panel": {"id": "reminders", "topics": ["today"], "size": {"min": [420, 320], "preferred": [500, 400]}}}
    })
}

thread_local! {
    static SERVICE: std::cell::RefCell<Option<nyx_core::control::Service>> = const { std::cell::RefCell::new(None) };
    static TIMER: std::cell::RefCell<Option<gtk::glib::SourceId>> = const { std::cell::RefCell::new(None) };
}

/// Register the interface (from `connect_startup`); `today` is read again every
/// minute only while something watches it, and whenever the lists load.
pub fn serve(app: &gtk::Application) {
    use gtk::prelude::*;
    let id = app.application_id().map(|s| s.to_string()).unwrap_or_else(|| crate::APP_ID.to_string());
    let window_app = app.clone();
    let Some(service) = nyx_core::control::serve_with(app, description(&id), move |request, answer| {
        if request.action == "reminders.open" {
            window::present(&window_app, None);
            return answer(Ok(Value::Null));
        }
        window::when_loaded(move || {
            handle(&request.action, request.arguments, Box::new(move |r| {
                answer(r.map_err(|e| nyx_actions::Error::new(nyx_actions::ErrorKind::Failed, e)));
            }));
        });
    }) else {
        return;
    };
    service.on_watch(|_, on| {
        TIMER.with(|t| {
            if let Some(id) = t.borrow_mut().take() {
                id.remove();
            }
            if on {
                publish();
                *t.borrow_mut() = Some(gtk::glib::timeout_add_seconds_local(60, || {
                    publish();
                    gtk::glib::ControlFlow::Continue
                }));
            }
        });
    });
    SERVICE.with(|s| *s.borrow_mut() = Some(service));
}

/// Send `today` to its watchers (none: nothing to do).
pub fn publish() {
    let Some(service) = SERVICE.with(|s| s.borrow().clone()) else { return };
    if !service.watched("today") {
        return;
    }
    window::when_loaded(move || {
        handle("reminders.today", json!({}), Box::new(move |r| {
            service.publish("today", r.unwrap_or_else(|e| json!({"error": e})));
        }));
    });
}

pub fn handle(name: &str, args: Value, reply: Reply) {
    let tasks = window::tasks();
    let lists = window::lists();
    let name_of = |uid: &str| lists.iter().find(|l| l.uid == uid).map(|l| l.name.clone()).unwrap_or_default();
    let respond = |picked: Vec<&Task>| {
        let more = picked.len() > MAX_TASKS;
        let list: Vec<Value> = picked.iter().take(MAX_TASKS).map(|t| task_json(t, &name_of(&t.list))).collect();
        json!({ "reminders": list, "more": more, "untrusted_content": true })
    };
    match name {
        "reminders.lists" => reply(Ok(json!({ "lists": lists.iter().map(|l| json!({
            "uid": l.uid, "name": l.name, "account": l.group_label(), "read_only": l.read_only,
            "open": tasks.iter().filter(|t| t.list == l.uid && !t.done).count(),
        })).collect::<Vec<_>>() }))),
        "reminders.today" => {
            let today = Local::now().date_naive();
            reply(Ok(respond(tasks.iter().filter(|t| Smart::Today.contains(t, None, today)).collect())));
        }
        "reminders.list" => {
            let list = args["list"].as_str();
            let done = args["include_done"].as_bool().unwrap_or(false);
            reply(Ok(respond(tasks.iter().filter(|t| list.is_none_or(|l| t.list == l) && (done || !t.done)).collect())));
        }
        "reminders.search" => {
            let Ok(query) = text(&args, "query").map(|q| q.to_lowercase()) else { return reply(Err("query is required".into())) };
            reply(Ok(respond(
                tasks
                    .iter()
                    .filter(|t| t.title.to_lowercase().contains(&query) || t.notes.to_lowercase().contains(&query))
                    .collect(),
            )));
        }
        "reminders.create" => {
            let title = match text(&args, "title") {
                Ok(t) if !t.trim().is_empty() => t,
                _ => return reply(Err("title is required".into())),
            };
            let list = args["list"].as_str().map(str::to_string).unwrap_or_else(window::default_list);
            if !lists.iter().any(|l| l.uid == list && !l.read_only) {
                return reply(Err("That list can't take new reminders.".into()));
            }
            let caps = window::list_caps(&list);
            let mut quick = actions::parse_quick(&title, actions::now(), caps, None);
            if let Some(due) = args["due"].as_str() {
                match due_arg(due) {
                    Some(d) => quick.due = Some(d),
                    None => return reply(Err(format!("Couldn't read the due date “{due}”."))),
                }
            }
            let quick = Quick { due: quick.due.map(|d| if caps.date_only { Due::Day(d.day()) } else { d }), ..quick };
            let notes = args["notes"].as_str().map(str::to_string);
            let source = args["source"].as_str().map(str::to_string);
            let l = list.clone();
            actions::add(list, quick, None, notes, move |result| match result {
                Ok(uid) => {
                    if let Some(source) = source {
                        window::set_source(&l, &uid, &source);
                    }
                    reply(Ok(json!({"list": l, "uid": uid})));
                }
                Err(e) => reply(Err(e)),
            });
        }
        "reminders.complete" => {
            let (Ok(list), Ok(uid)) = (text(&args, "list"), text(&args, "uid")) else {
                return reply(Err("list and uid are required".into()));
            };
            let done = args["done"].as_bool().unwrap_or(true);
            worker::ask(
                move || {
                    worker::with_list(&list, |l| {
                        if done {
                            l.complete(&uid)
                        } else {
                            l.modify(&uid, None, Scope::All, &Draft { done: Some(false), ..Draft::default() })
                        }
                    })
                },
                move |r| reply(r.map(|()| json!({"done": done}))),
            );
        }
        "reminders.edit" => {
            let (Ok(list), Ok(uid)) = (text(&args, "list"), text(&args, "uid")) else {
                return reply(Err("list and uid are required".into()));
            };
            let mut draft = Draft {
                summary: args["title"].as_str().map(str::to_string).filter(|t| !t.trim().is_empty()),
                description: args["notes"].as_str().map(str::to_string),
                ..Draft::default()
            };
            if let Some(due) = args["due"].as_str() {
                match due_arg(due) {
                    Some(d) => draft.due = Some(Some(d.when())),
                    None if due.is_empty() => draft.due = Some(None),
                    None => return reply(Err(format!("Couldn't read the due date “{due}”."))),
                }
            }
            if draft == Draft::default() {
                return reply(Err("Nothing to change.".into()));
            }
            worker::ask(
                move || worker::with_list(&list, |l| l.modify(&uid, None, Scope::All, &draft)),
                move |r| reply(r.map(|()| json!({"changed": true}))),
            );
        }
        "reminders.delete" => {
            let (Ok(list), Ok(uid)) = (text(&args, "list"), text(&args, "uid")) else {
                return reply(Err("list and uid are required".into()));
            };
            worker::ask(
                move || worker::with_list(&list, |l| l.remove(&uid, None, Scope::All)),
                move |r| reply(r.map(|()| json!({"deleted": true}))),
            );
        }
        other => reply(Err(format!("Reminders has no tool called {other}."))),
    }
}

fn task_json(t: &Task, list_name: &str) -> Value {
    let due = t.due.map(|d| match d {
        Due::Day(day) => day.to_string(),
        Due::At(at) => at.format("%Y-%m-%dT%H:%M").to_string(),
    });
    json!({
        "list": t.list, "list_name": list_name, "uid": t.uid, "title": t.title,
        "notes": t.notes.chars().take(500).collect::<String>(), "due": due, "done": t.done,
        "overdue": t.overdue(actions::now()), "priority": t.priority, "parent": t.parent, "repeats": t.rule.is_some(), "tags": t.tags,
    })
}

fn due_arg(text: &str) -> Option<Due> {
    let text = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Some(Due::At(dt.with_timezone(&Local).naive_local()));
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(at) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Due::At(at));
        }
    }
    if let Ok(day) = text.parse::<NaiveDate>() {
        return Some(Due::Day(day));
    }
    let parse = nyx_when::parse(text, actions::now());
    match (parse.date, parse.start) {
        (Some(d), Some(t)) => Some(Due::At(d.and_time(t))),
        (Some(d), None) => Some(Due::Day(d)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_is_api_2() {
        let d = nyx_actions::Description::from_value(&description(crate::APP_ID), None).unwrap();
        assert!(d.action("reminders.delete").unwrap().asks());
        assert!(!d.action("reminders.create").unwrap().asks());
        assert_eq!(d.app.panel.as_ref().map(|p| p.id.as_str()), Some("reminders"));
    }

    #[test]
    fn reads_due_dates() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        assert_eq!(due_arg("2026-10-30"), Some(Due::Day(d)));
        assert_eq!(due_arg("2026-10-30T09:15"), Some(Due::At(d.and_hms_opt(9, 15, 0).unwrap())));
        assert!(matches!(due_arg("tomorrow at 9"), Some(Due::At(_))));
        assert_eq!(due_arg("whenever"), None);
    }
}
