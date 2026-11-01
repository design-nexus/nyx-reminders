//! The EDS worker. One thread owns the source registry and the open task
//! lists, runs its own GLib main context so EDS signals arrive there, and hands
//! plain data to the window on the GTK thread.

use nyx_eds::{Calendar, Kind, Registry, View, Watch};
use nyx_reminders_core::{Caps, ListInfo, Task};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug)]
pub enum Update {
    Unavailable(String),
    /// Online accounts whose task lists EDS can't load: its backend for them
    /// isn't installed.
    Unsupported(Vec<String>),
    Lists(Vec<ListInfo>),
    /// Every task of every list, and which lists loaded (others are offline or failed).
    Tasks {
        tasks: Vec<Task>,
        loaded: HashSet<String>,
        problems: Vec<String>,
    },
}

#[derive(Default)]
struct State {
    registry: Option<Registry>,
    _watch: Option<Watch>,
    accounts: Option<nyx_accounts::Client>,
    open: HashMap<String, Calendar>,
    views: HashMap<String, View>,
    sent: Vec<ListInfo>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static PENDING_LISTS: Cell<bool> = const { Cell::new(false) };
    static PENDING_TASKS: Cell<bool> = const { Cell::new(false) };
}

static CTX: OnceLock<glib::MainContext> = OnceLock::new();

pub fn start() {
    let ctx = glib::MainContext::new();
    if CTX.set(ctx.clone()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("reminders-eds".into())
        .spawn(move || {
            let main = ctx.clone();
            let _ = ctx.with_thread_default(move || {
                connect();
                glib::MainLoop::new(Some(&main), false).run();
            });
        })
        .expect("start the reminders worker");
}

/// Runs `work` on the worker and hands its result to `reply` on the GTK thread.
pub fn ask<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, reply: impl FnOnce(T) + 'static) {
    let reply = glib::thread_guard::ThreadGuard::new(reply);
    run(move || {
        let value = work();
        glib::MainContext::default().invoke(move || (reply.into_inner())(value));
    });
}

/// Runs `f` with an open task list (opening it if needed). For use inside [`ask`].
pub fn with_list<T>(uid: &str, f: impl FnOnce(&Calendar) -> Result<T, nyx_eds::Error>) -> Result<T, String> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let state = &mut *s;
        let Some(registry) = &state.registry else { return Err("Task data isn't available.".to_string()) };
        if !state.open.contains_key(uid) {
            let list = Calendar::open(registry, uid, Kind::TaskList, None).map_err(|e| e.to_string())?;
            state.open.insert(uid.to_string(), list);
        }
        f(&state.open[uid]).map_err(|e| e.to_string())
    })
}

/// Asks every open list to sync with its server now; replies with problems.
fn registry_call(
    work: impl FnOnce(&Registry) -> Result<(), nyx_eds::Error> + Send + 'static,
    reply: impl FnOnce(Result<(), String>) + 'static,
) {
    ask(
        move || {
            let r = STATE.with(|s| s.borrow().registry.as_ref().map(work));
            schedule_lists();
            match r {
                Some(Ok(())) => Ok(()),
                Some(Err(e)) => Err(e.to_string()),
                None => Err("Task lists aren't available.".into()),
            }
        },
        reply,
    );
}

/// Switches a task list on or off for every app.
pub fn set_enabled(uid: String, enabled: bool, reply: impl FnOnce(Result<(), String>) + 'static) {
    registry_call(move |r| r.set_enabled(&uid, enabled), reply);
}

pub fn set_color(uid: String, color: String, reply: impl FnOnce(Result<(), String>) + 'static) {
    registry_call(move |r| r.set_color(&uid, Kind::TaskList, &color), reply);
}

/// Every task list, with whether it's switched on (Settings → Task lists).
pub fn all_lists(reply: impl FnOnce(Vec<(nyx_eds::Source, bool)>) + 'static) {
    ask(|| STATE.with(|s| s.borrow().registry.as_ref().map(|r| r.all_sources(Kind::TaskList)).unwrap_or_default()), reply);
}

pub fn refresh(reply: impl FnOnce(Vec<String>) + 'static) {
    ask(
        || {
            let problems = STATE.with(|s| {
                let s = s.borrow();
                s.open
                    .iter()
                    .filter_map(|(uid, list)| list.refresh().err().map(|e| format!("{}: {e}", name_of(&s, uid))))
                    .collect()
            });
            schedule_lists();
            problems
        },
        reply,
    );
}

fn run(f: impl FnOnce() + Send + 'static) {
    if let Some(ctx) = CTX.get() {
        ctx.invoke(f);
    }
}

fn send(update: Update) {
    glib::MainContext::default().invoke(move || crate::window::apply(update));
}

fn after(delay: std::time::Duration, f: impl FnOnce() + Send + 'static) {
    let mut f = Some(f);
    let source = glib::timeout_source_new(delay, None, glib::Priority::DEFAULT, move || {
        if let Some(f) = f.take() {
            f();
        }
        glib::ControlFlow::Break
    });
    source.attach(CTX.get());
}

fn connect() {
    match Registry::connect() {
        Ok(registry) => {
            let watch = registry.watch(schedule_lists);
            let accounts = nyx_accounts::Client::connect().ok();
            if let Some(client) = &accounts {
                client.watch(sync_accounts);
            }
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.registry = Some(registry);
                s._watch = Some(watch);
                s.accounts = accounts;
            });
            load();
            send_unsupported();
            poll_online();
            after(std::time::Duration::from_millis(250),sync_accounts);
        }
        Err(err) => {
            let text=format!("Native data could not be opened ({err}). Your existing data has been retained. Trying again…");
            send(Update::Unavailable(text));
            after(std::time::Duration::from_secs(10), connect);
        }
    }
}

fn poll_online() {
    after(std::time::Duration::from_secs(30), || {
        send_lists(false);
        poll_online();
    });
}

fn schedule_lists() {
    if PENDING_LISTS.with(|p| p.replace(true)) {
        return;
    }
    after(std::time::Duration::from_millis(250), || {
        PENDING_LISTS.with(|p| p.set(false));
        load();
        send_unsupported();
    });
}

/// Refresh native accounts independently; cached data is shown first.
fn sync_accounts(){
    STATE.with(|state|{let state=state.borrow();if let Some(registry)=&state.registry{let _=registry.refresh_accounts(nyx_accounts::Service::Tasks);}});
    schedule_lists();
}
fn send_unsupported() { send(Update::Unsupported(Vec::new())); }

fn schedule_tasks() {
    if PENDING_TASKS.with(|p| p.replace(true)) {
        return;
    }
    after(std::time::Duration::from_millis(250), || {
        PENDING_TASKS.with(|p| p.set(false));
        load_tasks();
    });
}

fn name_of(state: &State, uid: &str) -> String {
    state.sent.iter().find(|c| c.uid == uid).map(|c| c.name.clone()).unwrap_or_else(|| uid.to_string())
}

/// What a list's backend keeps. Google Tasks keeps a title, notes, a due date,
/// completion and subtasks; others keep everything VTODO has.
fn caps(list: Option<&Calendar>, backend: &str) -> Caps {
    let gtasks = backend == "gtasks";
    let can = |c: &str| list.is_some_and(|l| l.can(c));
    Caps {
        date_only: gtasks || can("task-date-only"),
        alerts: !gtasks && !can("task-no-alarm"),
        repeat: !gtasks,
        priority: !gtasks,
        tags: !gtasks,
    }
}

fn list_infos(state: &State) -> Vec<ListInfo> {
    let Some(registry) = &state.registry else { return Vec::new() };
    let attention: HashSet<String> = state
        .accounts
        .as_ref()
        .and_then(|c| c.list().ok())
        .map(|list| list.into_iter().filter(|a| a.status == "needs_sign_in").map(|a| a.id).collect())
        .unwrap_or_default();
    registry
        .sources(Kind::TaskList)
        .into_iter()
        .map(|s| {
            let open = state.open.get(&s.uid);
            ListInfo {
                name: s.label().to_string(),
                needs_sign_in: s.account.as_ref().is_some_and(|a| attention.contains(a)),
                offline: open.is_some_and(|c| !c.online()),
                read_only: open.is_some_and(|c| c.read_only()),
                caps: caps(open, &s.backend),
                uid: s.uid,
                color: s.color,
                selected: s.selected,
                group: s.group,
                account: s.account,
            }
        })
        .collect()
}

fn send_lists(always: bool) {
    let list = STATE.with(|s| list_infos(&s.borrow()));
    let changed = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let changed = s.sent != list;
        s.sent = list.clone();
        changed
    });
    if always || changed {
        send(Update::Lists(list));
    }
}

/// Opens every list, then sends the lists (with their capabilities) and tasks.
fn load() {
    let mut problems = Vec::new();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let state = &mut *s;
        let Some(registry) = &state.registry else { return };
        let sources = registry.sources(Kind::TaskList);
        for src in &sources {
            if state.open.contains_key(&src.uid) {
                continue;
            }
            match Calendar::open(registry, &src.uid, Kind::TaskList, None) {
                Ok(list) => {
                    if let Ok(view) = list.watch("#t", schedule_tasks) {
                        state.views.insert(src.uid.clone(), view);
                    }
                    state.open.insert(src.uid.clone(), list);
                }
                Err(err) => problems.push(format!("{}: {err}", src.label())),
            }
        }
        let current: HashSet<&String> = sources.iter().map(|s| &s.uid).collect();
        state.open.retain(|uid, _| current.contains(uid));
        state.views.retain(|uid, _| current.contains(uid));
    });
    send_lists(true);
    load_tasks_with(problems);
}

fn load_tasks() {
    load_tasks_with(Vec::new());
}

fn load_tasks_with(mut problems: Vec<String>) {
    let mut tasks = Vec::new();
    let mut loaded = HashSet::new();
    STATE.with(|s| {
        let s = s.borrow();
        for (uid, list) in &s.open {
            match list.items("#t") {
                Ok(items) => {
                    tasks.extend(items.iter().map(|item| Task::from_item(uid, item)));
                    loaded.insert(uid.clone());
                }
                Err(err) => problems.push(format!("{}: {err}", name_of(&s, uid))),
            }
        }
    });
    send(Update::Tasks { tasks, loaded, problems });
    send_lists(false);
}
