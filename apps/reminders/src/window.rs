//! The Reminders window. The sidebar follows Music: it does not grow, and it
//! collapses to icons under 980 px or when the user collapses it.

use crate::actions;
use crate::detail;
use crate::prefs;
use crate::settings_dialog;
use crate::widgets;
use crate::worker::{self, Update};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use nyx_reminders_core::{Caps, Level, ListInfo, Meta, Sidecar, Smart, Task, sort, tree};
use nyx_theme::Rgb;
use nyx_ui::chrome;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["N"], "New reminder"),
    (&["J"], "Next reminder"),
    (&["K"], "Previous reminder"),
    (&["Space"], "Complete or reopen"),
    (&["S"], "Flag or unflag"),
    (&["Delete"], "Delete"),
    (&["1"], "Today"),
    (&["2"], "Scheduled"),
    (&["3"], "Flagged"),
    (&["4"], "All"),
    (&["R"], "Refresh"),
    (&["Ctrl", "F"], "Find reminders"),
    (&["Esc"], "Close the details or clear the search"),
    (&["Ctrl", "B"], "Collapse or expand sidebar"),
    (&["Ctrl", ","], "Settings"),
    (&["F1"], "Show keyboard shortcuts"),
    (&["Ctrl", "Q"], "Close"),
];

type Key = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Smart(Smart),
    List(String),
}

impl Page {
    fn id(&self) -> String {
        match self {
            Self::Smart(s) => s.id().to_string(),
            Self::List(uid) => format!("list:{uid}"),
        }
    }

    fn from_id(id: &str) -> Self {
        match id.strip_prefix("list:") {
            Some(uid) => Self::List(uid.to_string()),
            None => Self::Smart(Smart::from_id(id).unwrap_or(Smart::Today)),
        }
    }
}

struct Ui {
    window: gtk::ApplicationWindow,
    overlay: gtk::Overlay,
    nav: gtk::Box,
    nav_list: gtk::Box,
    search: gtk::SearchEntry,
    banners: gtk::Box,
    title: gtk::Label,
    quick: gtk::Entry,
    rows: gtk::Box,
    detail: gtk::Box,
    detail_scroll: gtk::ScrolledWindow,
    page: Page,
    lists: Vec<ListInfo>,
    tasks: Vec<Task>,
    meta: HashMap<Key, Meta>,
    sidecar: Option<Sidecar>,
    /// The task whose details show, and what was shown.
    selected: Option<Key>,
    shown_detail: Option<Task>,
    /// Row order on screen, for J/K.
    order: Vec<Key>,
    unavailable: Option<String>,
    /// Online accounts whose task lists need a backend EDS doesn't have.
    unsupported: Vec<String>,
    loaded: bool,
    refreshing: bool,
}

thread_local! {
    /// Each reminder row on the page, for menus opened from the keyboard.
    static ROWS: RefCell<std::collections::HashMap<Key, glib::WeakRef<gtk::Widget>>> = RefCell::new(Default::default());
    static UI: RefCell<Option<Rc<RefCell<Ui>>>> = const { RefCell::new(None) };
    static NARROW: Cell<bool> = const { Cell::new(false) };
    static TOAST: RefCell<Option<gtk::Box>> = const { RefCell::new(None) };
    static STATUS: RefCell<Option<gtk::Label>> = const { RefCell::new(None) };
}

fn ui() -> Option<Rc<RefCell<Ui>>> {
    UI.with(|u| u.borrow().clone())
}

pub fn window() -> Option<gtk::ApplicationWindow> {
    ui().map(|u| u.borrow().window.clone())
}

pub fn overlay() -> Option<gtk::Overlay> {
    ui().map(|u| u.borrow().overlay.clone())
}

thread_local! {
    /// The lists and reminders as the worker last sent them, window or not (Reminders
    /// started by D-Bus for a call); whether the first reminders came; calls waiting.
    static KNOWN: RefCell<(Vec<ListInfo>, Vec<Task>, bool)> = const { RefCell::new((Vec::new(), Vec::new(), false)) };
    static WAITING: RefCell<Vec<Box<dyn FnOnce()>>> = const { RefCell::new(Vec::new()) };
}

pub fn lists() -> Vec<ListInfo> {
    KNOWN.with(|k| k.borrow().0.clone())
}

pub fn tasks() -> Vec<Task> {
    KNOWN.with(|k| k.borrow().1.clone())
}

/// Run `f` once the worker has sent the reminders: at once when it has, else when it
/// does (starting it without a window), or after 8 s whatever happened.
pub fn when_loaded(f: impl FnOnce() + 'static) {
    if KNOWN.with(|k| k.borrow().2) {
        return f();
    }
    WAITING.with(|w| w.borrow_mut().push(Box::new(f)));
    worker::start();
    glib::timeout_add_local_once(std::time::Duration::from_secs(8), run_waiting);
}

fn run_waiting() {
    for f in WAITING.with(|w| std::mem::take(&mut *w.borrow_mut())) {
        f();
    }
}

pub fn meta_of(key: &Key) -> Meta {
    ui().and_then(|u| u.borrow().meta.get(key).cloned()).unwrap_or_default()
}

pub fn list_caps(uid: &str) -> Caps {
    lists().iter().find(|l| l.uid == uid).map(|l| l.caps).unwrap_or_default()
}

/// Where new reminders go from smart lists.
pub fn default_list() -> String {
    let all = lists();
    let chosen = prefs::get().default_list;
    if all.iter().any(|l| l.uid == chosen && !l.read_only) {
        return chosen;
    }
    all.iter().find(|l| !l.read_only).map(|l| l.uid.clone()).unwrap_or_default()
}

pub fn present(app: &gtk::Application, section: Option<&str>) {
    let first = ui().is_none();
    if first {
        build(app);
        worker::start();
        let start = nyx_theme::start_page("today", &prefs::get().last_section);
        show_page(Page::from_id(&start));
    }
    let Some(ui) = ui() else { return };
    let window = ui.borrow().window.clone();
    let snapshot = std::env::var_os("REMINDERS_SNAPSHOT");
    if snapshot.is_some() {
        window.set_default_size(env_num("REMINDERS_SNAPSHOT_W", 1120), env_num("REMINDERS_SNAPSHOT_H", 800));
    }
    window.present();
    match section {
        Some("settings") => settings_dialog::open(),
        Some(id) => show_page(Page::from_id(id)),
        None => {}
    }
    if let Some(out) = snapshot {
        snapshot_and_quit(app, std::path::PathBuf::from(out));
    }
}

fn env_num(key: &str, default: i32) -> i32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Renders the window to a PNG and quits (`REMINDERS_SNAPSHOT=out.png`;
/// `_PAGE`, `_SELECT` (first task), `_W`, `_H`, `_DELAY`).
fn snapshot_and_quit(app: &gtk::Application, out: std::path::PathBuf) {
    let Some(ui) = ui() else { return };
    let window = ui.borrow().window.clone();
    let nav = ui.borrow().nav.clone();
    let (w, h) = (env_num("REMINDERS_SNAPSHOT_W", 1120), env_num("REMINDERS_SNAPSHOT_H", 800));
    window.set_opacity(0.01);
    window.set_title(Some("Reminders snapshot"));
    if let Ok(page) = std::env::var("REMINDERS_SNAPSHOT_PAGE") {
        show_page(Page::from_id(&page));
    }
    window.set_default_size(w, h);
    window.set_size_request(w, h);
    let app = app.clone();
    let delay = env_num("REMINDERS_SNAPSHOT_DELAY", 3000) as u64;
    if std::env::var_os("REMINDERS_SNAPSHOT_SELECT").is_some() {
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay / 2), || move_selection(1));
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        let (w, h, nav_w, compact) = (window.width(), window.height(), nav.width(), nav.has_css_class("compact"));
        if let Some(child) = window.child() {
            let paintable = gtk::WidgetPaintable::new(Some(&child));
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(&gdk::RGBA::BLACK, &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
            paintable.snapshot(&snapshot, w as f64, h as f64);
            if let (Some(node), Some(renderer)) = (snapshot.to_node(), window.renderer())
                && let Err(e) = renderer.render_texture(node, None).save_to_png(&out)
            {
                eprintln!("snapshot failed: {e}");
            }
        }
        println!("snapshot {w}x{h} nav={nav_w} compact={compact} -> {}", out.display());
        app.quit();
    });
}

/// Applies what the worker found.
pub fn apply(update: Update) {
    match &update {
        Update::Lists(lists) => KNOWN.with(|k| k.borrow_mut().0 = lists.clone()),
        Update::Tasks { tasks, .. } => {
            KNOWN.with(|k| {
                let mut k = k.borrow_mut();
                k.1 = tasks.clone();
                k.2 = true;
            });
            run_waiting();
            crate::control::publish();
        }
        Update::Unavailable(_) => {
            KNOWN.with(|k| k.borrow_mut().2 = true);
            run_waiting();
        }
        _ => {}
    }
    let Some(ui) = ui() else { return };
    match update {
        Update::Unavailable(text) => {
            let mut u = ui.borrow_mut();
            u.unavailable = Some(text);
            u.loaded = true;
        }
        Update::Unsupported(accounts) => {
            ui.borrow_mut().unsupported = accounts;
            rebuild_banners();
            return;
        }
        Update::Lists(lists) => {
            let mut u = ui.borrow_mut();
            u.unavailable = None;
            u.loaded = true;
            u.lists = lists;
        }
        Update::Tasks { tasks, loaded, problems } => {
            for p in problems {
                toast(&p);
            }
            let mut u = ui.borrow_mut();
            // Forget local extras for tasks gone from lists that loaded.
            let existing: HashSet<Key> = tasks.iter().map(Task::key).collect();
            if let Some(sidecar) = &u.sidecar {
                let _ = sidecar.prune(&loaded, &existing);
            }
            u.meta = u.sidecar.as_ref().and_then(|s| s.all().ok()).unwrap_or_default();
            u.tasks = tasks;
        }
    }
    redraw();
}

fn redraw() {
    rebuild_nav();
    rebuild_banners();
    rebuild_page();
    refresh_status();
}

pub fn refresh_page() {
    rebuild_page();
    refresh_status();
}

pub(crate) fn show_page(page: Page) {
    let Some(ui) = ui() else { return };
    prefs::update(|p| p.last_section = page.id());
    {
        let mut u = ui.borrow_mut();
        u.page = page;
        u.selected = None;
    }
    redraw();
}

pub fn select(key: Option<Key>) {
    let Some(ui) = ui() else { return };
    ui.borrow_mut().selected = key;
    rebuild_page();
}

fn page_title(u: &Ui) -> String {
    match &u.page {
        Page::Smart(s) => s.name().to_string(),
        Page::List(uid) => u.lists.iter().find(|l| l.uid == *uid).map(|l| l.name.clone()).unwrap_or_else(|| "List".into()),
    }
}

/// The tasks on the current page, in order with their depth.
fn page_tasks(u: &Ui) -> Vec<(usize, usize)> {
    let today = actions::now().date();
    let query = u.search.text().trim().to_lowercase();
    let show_done = prefs::get().show_completed;
    let mut picked: Vec<usize> = (0..u.tasks.len())
        .filter(|i| {
            let t = &u.tasks[*i];
            let on_page = match &u.page {
                Page::Smart(s) => s.contains(t, u.meta.get(&t.key()), today),
                Page::List(uid) => t.list == *uid && (show_done || !t.done),
            };
            let tag = query.trim_start_matches('#');
            on_page
                && (query.is_empty()
                    || t.title.to_lowercase().contains(&query)
                    || t.notes.to_lowercase().contains(&query)
                    || t.tags.iter().any(|g| g.to_lowercase().contains(tag)))
        })
        .collect();
    sort(&u.tasks, &mut picked, &u.meta);
    // Smart lists flatten subtasks; a list nests them.
    match u.page {
        Page::List(_) => tree(&u.tasks, &picked),
        Page::Smart(_) => picked.into_iter().map(|i| (i, 0)).collect(),
    }
}

fn smart_count(u: &Ui, smart: Smart) -> usize {
    let today = actions::now().date();
    u.tasks.iter().filter(|t| smart.contains(t, u.meta.get(&t.key()), today)).count()
}

fn rebuild_page() {
    let Some(ui) = ui() else { return };
    let (rows, entries, selected, title_text, quick, detail_box, scroll) = {
        let u = ui.borrow();
        (
            u.rows.clone(),
            page_tasks(&u),
            u.selected.clone(),
            page_title(&u),
            u.quick.clone(),
            u.detail.clone(),
            u.detail_scroll.clone(),
        )
    };
    ui.borrow().title.set_text(&title_text);
    ui.borrow().title.set_visible(!title_text.is_empty());
    chrome::set_title(&ui.borrow().window, "Reminders", &title_text);
    let can_add = {
        let u = ui.borrow();
        match &u.page {
            Page::Smart(Smart::Completed) => false,
            Page::List(uid) => u.lists.iter().any(|l| l.uid == *uid && !l.read_only),
            Page::Smart(_) => u.lists.iter().any(|l| !l.read_only),
        }
    };
    quick.set_sensitive(can_add);

    while let Some(child) = rows.first_child() {
        rows.remove(&child);
    }
    let now = actions::now();
    let (tasks, lists, meta, page) = {
        let u = ui.borrow();
        (u.tasks.clone(), u.lists.clone(), u.meta.clone(), u.page.clone())
    };
    let mut order = Vec::new();
    if entries.is_empty() {
        let (icon, text) = match &page {
            Page::Smart(Smart::Today) => ("emblem-ok-symbolic", "Nothing due today"),
            Page::Smart(Smart::Completed) => ("emblem-ok-symbolic", "Nothing completed yet"),
            Page::Smart(Smart::Flagged) => ("starred-symbolic", "No flagged reminders"),
            _ => ("view-list-symbolic", "No reminders"),
        };
        let loaded = ui.borrow().loaded;
        if loaded {
            rows.append(&widgets::empty_state(icon, text, "", None));
        }
    }
    let mut done_header = false;
    for (index, depth) in entries {
        let task = &tasks[index];
        if task.done && !done_header && matches!(page, Page::List(_)) {
            let label = widgets::label("COMPLETED", "group-title");
            label.set_xalign(0.0);
            label.add_css_class("completed-head");
            rows.append(&label);
            done_header = true;
        }
        let list_name =
            if matches!(page, Page::Smart(_)) { lists.iter().find(|l| l.uid == task.list).map(|l| l.name.clone()) } else { None };
        let color = lists.iter().find(|l| l.uid == task.list).and_then(|l| l.color.clone());
        let row = task_row(task, depth, meta.get(&task.key()), list_name, color, selected.as_ref() == Some(&task.key()), now);
        // A list's open reminders reorder by dragging (not while searching).
        if matches!(page, Page::List(_)) && !task.done && ui.borrow().search.text().trim().is_empty() {
            attach_drag(&row, task.key());
        }
        rows.append(&row);
        order.push(task.key());
    }
    ui.borrow_mut().order = order;

    // Details for the selected task; not rebuilt while the user is typing in them.
    let chosen = selected.as_ref().and_then(|k| tasks.iter().find(|t| t.key() == *k)).cloned();
    match chosen {
        Some(task) => {
            let typing = gtk::prelude::GtkWindowExt::focus(&ui.borrow().window).is_some_and(|f| f.is_ancestor(&detail_box));
            let changed = ui.borrow().shown_detail.as_ref() != Some(&task);
            if changed && !typing {
                let subtasks: Vec<Task> = tasks
                    .iter()
                    .filter(|t| t.parent.as_deref() == Some(task.uid.as_str()) && t.list == task.list)
                    .cloned()
                    .collect();
                detail::fill(&detail_box, &task, meta.get(&task.key()).unwrap_or(&Meta::default()), &subtasks);
                ui.borrow_mut().shown_detail = Some(task);
            }
            scroll.set_visible(true);
        }
        None => {
            ui.borrow_mut().shown_detail = None;
            scroll.set_visible(false);
        }
    }
}

fn task_row(
    task: &Task,
    depth: usize,
    meta: Option<&Meta>,
    list_name: Option<String>,
    color: Option<String>,
    selected: bool,
    now: chrono::NaiveDateTime,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("task-row");
    if selected {
        row.add_css_class("selected");
    }
    row.set_margin_start(depth as i32 * 28);
    let check = gtk::CheckButton::new();
    check.add_css_class("task-check");
    check.set_active(task.done);
    check.set_valign(gtk::Align::Start);
    check.set_tooltip_text(Some(if task.done { "Reopen (Space)" } else { "Complete (Space)" }));
    {
        let t = task.clone();
        check.connect_toggled(move |c| actions::set_done(&t, c.is_active()));
    }
    row.append(&check);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let title = widgets::label(&task.title, "task-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    if task.done {
        title.add_css_class("done");
    }
    text.append(&title);
    let meta_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    if let Some(due) = task.due {
        let l = widgets::label(&actions::due_text(due, now), "task-due");
        if task.overdue(now) {
            l.add_css_class("overdue");
        }
        meta_line.append(&l);
    }
    if task.rule.is_some() {
        let i = gtk::Image::from_icon_name("media-playlist-repeat-symbolic");
        i.add_css_class("dim");
        i.set_tooltip_text(Some("Repeats"));
        meta_line.append(&i);
    }
    if !task.notes.trim().is_empty() {
        let first: String = task.notes.lines().next().unwrap_or("").chars().take(80).collect();
        let l = widgets::label(&first, "dim");
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_hexpand(true);
        l.set_xalign(0.0);
        meta_line.append(&l);
    }
    for tag in &task.tags {
        meta_line.append(&widgets::label(&format!("#{tag}"), "task-tag"));
    }
    if let Some(name) = list_name {
        let dot = gtk::DrawingArea::new();
        dot.set_content_width(8);
        dot.set_content_height(8);
        dot.set_valign(gtk::Align::Center);
        dot.set_draw_func(move |_, cr, _, _| {
            let c = color.as_deref().map(Rgb::hex).unwrap_or_else(|| Rgb::hex(&nyx_theme::palette().accent));
            cr.set_source_rgb(c.0, c.1, c.2);
            cr.arc(4.0, 4.0, 3.5, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        });
        meta_line.append(&dot);
        meta_line.append(&widgets::label(&name, "dim"));
    }
    if meta_line.first_child().is_some() {
        text.append(&meta_line);
    }
    row.append(&text);

    if let Some(level) = task.priority_level().filter(|_| !task.done) {
        let (marks, class) = match level {
            Level::High => ("!!!", "priority-high"),
            Level::Medium => ("!!", "priority-medium"),
            Level::Low => ("!", "priority-low"),
        };
        let p = widgets::label(marks, class);
        p.set_valign(gtk::Align::Start);
        row.append(&p);
    }
    if meta.is_some_and(|m| m.flagged) {
        let f = gtk::Image::from_icon_name("starred-symbolic");
        f.add_css_class("flag");
        f.set_valign(gtk::Align::Start);
        f.set_tooltip_text(Some("Flagged (S)"));
        row.append(&f);
    }

    let click = gtk::GestureClick::new();
    let key = task.key();
    click.connect_released(move |_, _, _, _| select(Some(key.clone())));
    row.add_controller(click);
    let t = task.clone();
    widgets::on_right_click(&row, move |w, x, y| {
        select(Some(t.key()));
        crate::menus::task(w, x, y, &t);
    });
    ROWS.with(|r| r.borrow_mut().insert(task.key(), row.upcast_ref::<gtk::Widget>().downgrade()));
    row
}

/// Lets a row be dragged onto another to go before it (or after it, dropped on
/// its lower half). The order is kept in the sidecar.
fn attach_drag(row: &gtk::Box, key: (String, String)) {
    let source = gtk::DragSource::new();
    source.set_actions(gtk::gdk::DragAction::MOVE);
    let dragged = format!("{}\u{1f}{}", key.0, key.1);
    source.connect_prepare(move |_, _, _| Some(gtk::gdk::ContentProvider::for_value(&dragged.to_value())));
    let r = row.clone();
    source.connect_drag_begin(move |s, _| {
        s.set_icon(Some(&gtk::WidgetPaintable::new(Some(&r))), 0, 0);
    });
    row.add_controller(source);

    let target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    let r = row.clone();
    target.connect_drop(move |_, value, _, y| {
        let Some((list, uid)) = value.get::<String>().ok().and_then(|v| v.split_once('\u{1f}').map(|(l, u)| (l.to_string(), u.to_string())))
        else {
            return false;
        };
        let after = y > r.height() as f64 / 2.0;
        move_task((list, uid), &key, after);
        true
    });
    row.add_controller(target);
}

/// Puts `moved` before (or after) `onto` in the shown order of its list.
fn move_task(moved: (String, String), onto: &(String, String), after: bool) {
    let Some(ui) = ui() else { return };
    if moved == *onto || moved.0 != onto.0 {
        return;
    }
    let mut order: Vec<(String, String)> = ui.borrow().order.iter().filter(|k| k.0 == moved.0 && **k != moved).cloned().collect();
    let Some(at) = order.iter().position(|k| k == onto) else { return };
    order.insert(if after { at + 1 } else { at }, moved);
    let saved = {
        let mut u = ui.borrow_mut();
        let saved = u.sidecar.as_mut().map(|s| s.reorder(&order));
        u.meta = u.sidecar.as_ref().and_then(|s| s.all().ok()).unwrap_or_default();
        saved
    };
    if let Some(Err(err)) = saved {
        toast(&format!("Couldn't save the order: {err}"));
    }
    // After the drop finishes: rebuilding removes the rows it came through.
    glib::idle_add_local_once(rebuild_page);
}

/// The selected reminder's menu (the Menu key), at its row.
fn menu_for_selected() {
    let Some(task) = selected_task() else { return };
    let Some(row) = ROWS.with(|r| r.borrow().get(&task.key()).and_then(|w| w.upgrade())) else { return };
    let y = row.height() as f64 / 2.0;
    crate::menus::task(&row, 40.0, y, &task);
}

/// Flags or unflags a task (kept on this computer).
pub fn set_flag(task: &Task, flagged: bool) {
    let Some(ui) = ui() else { return };
    let key = task.key();
    let mut meta = ui.borrow().meta.get(&key).cloned().unwrap_or_default();
    if meta.flagged == flagged {
        return;
    }
    meta.flagged = flagged;
    let saved = ui.borrow().sidecar.as_ref().map(|s| s.set(&key, &meta));
    if let Some(Err(err)) = saved {
        toast(&format!("Couldn't save the flag: {err}"));
        return;
    }
    ui.borrow_mut().meta.insert(key, meta);
    rebuild_nav();
    refresh_page();
}

/// Records where a task came from (a Mail message).
pub fn set_source(list: &str, uid: &str, source: &str) {
    let key = (list.to_string(), uid.to_string());
    let Some(ui) = ui() else {
        // No window: straight to the sidecar the window reads.
        if let Ok(sidecar) = Sidecar::open(&nyx_core::paths::data_dir().join("reminders.sqlite")) {
            let mut meta = sidecar.get(&key).ok().unwrap_or_default();
            meta.source = Some(source.to_string());
            let _ = sidecar.set(&key, &meta);
        }
        return;
    };
    let mut meta = ui.borrow().meta.get(&key).cloned().unwrap_or_default();
    meta.source = Some(source.to_string());
    if let Some(sidecar) = &ui.borrow().sidecar {
        let _ = sidecar.set(&key, &meta);
    }
    ui.borrow_mut().meta.insert(key, meta);
}

/// The task lists have loaded (`--add` waits for this).
pub fn ready() -> bool {
    ui().is_some_and(|u| u.borrow().loaded && !u.borrow().lists.is_empty())
}

/// Adds a reminder from words ("Call Sam tomorrow at 9") to the list new
/// reminders go to, whatever page is showing (`--add`, from Glimpse).
pub fn add_text(text: &str) {
    let list = default_list();
    if list.is_empty() {
        toast("There's no list to add reminders to. Add an account in Settings → Online Accounts.");
        return;
    }
    let quick = actions::parse_quick(text, actions::now(), list_caps(&list), None);
    let dropped = quick.dropped.clone();
    let list_name = lists().iter().find(|l| l.uid == list).map(|l| l.name.clone()).unwrap_or_default();
    actions::add(list, quick, None, None, move |result| match result {
        Ok(_) if dropped.is_empty() => toast(&format!("Added to {list_name}.")),
        Ok(_) => toast(&format!("Added to {list_name}, without a {} (the list can't keep one).", dropped.join(" or "))),
        Err(err) => toast(&err),
    });
}

fn add_from_quick(text: &str) {
    let Some(ui) = ui() else { return };
    let page = ui.borrow().page.clone();
    let list = match &page {
        Page::List(uid) => uid.clone(),
        Page::Smart(_) => default_list(),
    };
    if list.is_empty() {
        toast("There's no list to add reminders to. Add an account in Settings → Online Accounts.");
        return;
    }
    let today = actions::now().date();
    let default_day = matches!(page, Page::Smart(Smart::Today)).then_some(today);
    let quick = actions::parse_quick(text, actions::now(), list_caps(&list), default_day);
    let flag = matches!(page, Page::Smart(Smart::Flagged));
    let dropped = quick.dropped.clone();
    let list_name = lists().iter().find(|l| l.uid == list).map(|l| l.name.clone()).unwrap_or_default();
    actions::add(list.clone(), quick, None, None, move |result| match result {
        Ok(uid) => {
            if flag {
                set_flag(&Task { list: list.clone(), uid, ..placeholder() }, true);
            }
            if !dropped.is_empty() {
                toast(&format!("{list_name} can't keep a {}; added without it.", dropped.join(" or ")));
            }
        }
        Err(err) => toast(&err),
    });
}

fn placeholder() -> Task {
    Task {
        list: String::new(),
        uid: String::new(),
        title: String::new(),
        notes: String::new(),
        url: None,
        due: None,
        done: false,
        priority: None,
        parent: None,
        rule: None,
        alarms: Vec::new(),
        tags: Vec::new(),
    }
}

fn selected_task() -> Option<Task> {
    let ui = ui()?;
    let key = ui.borrow().selected.clone()?;
    ui.borrow().tasks.iter().find(|t| t.key() == key).cloned()
}

fn move_selection(step: i32) {
    let Some(ui) = ui() else { return };
    let order = ui.borrow().order.clone();
    if order.is_empty() {
        return;
    }
    let current = ui.borrow().selected.clone().and_then(|k| order.iter().position(|o| *o == k));
    let next = match current {
        Some(i) => (i as i32 + step).clamp(0, order.len() as i32 - 1) as usize,
        None => 0,
    };
    select(Some(order[next].clone()));
}

fn refresh() {
    let Some(ui) = ui() else { return };
    if ui.borrow().refreshing {
        return;
    }
    ui.borrow_mut().refreshing = true;
    refresh_status();
    worker::refresh(|problems| {
        if let Some(state) = self::ui() {
            state.borrow_mut().refreshing = false;
        }
        if problems.is_empty() {
            toast("Lists refreshed.");
        } else {
            for p in problems {
                toast(&p);
            }
        }
        refresh_status();
    });
}

fn refresh_status() {
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    let mut parts = Vec::new();
    let shown = u.order.len();
    parts.push(match shown {
        0 => "No reminders".to_string(),
        1 => "1 reminder".to_string(),
        n => format!("{n} reminders"),
    });
    let now = actions::now();
    let overdue = u.tasks.iter().filter(|t| t.overdue(now)).count();
    if overdue > 0 {
        parts.push(format!("{overdue} overdue"));
    }
    if u.refreshing {
        parts.push("Refreshing…".into());
    } else if u.lists.iter().any(|l| l.offline) {
        parts.push("Offline".into());
    }
    STATUS.with(|s| {
        if let Some(label) = s.borrow().as_ref() {
            label.set_text(&parts.join(" · "));
        }
    });
}

pub fn open_online_accounts() {
    let settings = nyx_accounts::find_bin("nyx-settings");
    let started =
        settings.is_some_and(|bin| std::process::Command::new(bin).args(["--section", "online-accounts"]).spawn().is_ok());
    if !started && nyx_accounts::open_management().is_err() {
        toast("Install Nyx Settings or gnome-online-accounts-gtk to add accounts.");
    }
}

// ---------- Banners ----------

fn rebuild_banners() {
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    let banners = u.banners.clone();
    while let Some(child) = banners.first_child() {
        banners.remove(&child);
    }
    if let Some(text) = &u.unavailable {
        banners.append(&widgets::banner(&glib::markup_escape_text(text), true));
    }
    for account in &u.unsupported {
        banners.append(&widgets::banner(
            &format!(
                "<b>{}</b>: its task lists are unavailable. Check Online Accounts and refresh.",
                glib::markup_escape_text(account)
            ),
            true,
        ));
    }
    let mut asked = Vec::new();
    for list in u.lists.iter().filter(|l| l.needs_sign_in) {
        let group = list.group_label().to_string();
        if asked.contains(&group) {
            continue;
        }
        let banner = widgets::banner(&format!("<b>{}</b> needs you to sign in again.", glib::markup_escape_text(&group)), true);
        let sign_in = gtk::Button::with_label("Sign in");
        sign_in.set_valign(gtk::Align::Center);
        sign_in.connect_clicked(|_| open_online_accounts());
        banner.append(&sign_in);
        banners.append(&banner);
        asked.push(group);
    }
    if u.lists.iter().any(|l| l.offline) {
        banners.append(&widgets::banner("Offline. Showing your saved lists; changes sync when you're back online.", false));
    }
    banners.set_visible(banners.first_child().is_some());
}

// ---------- Sidebar ----------

fn nav_button(icon: &str, title: &str, tooltip: &str, count: usize, active: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("nav-item");
    if active {
        button.add_css_class("active");
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.append(&gtk::Image::from_icon_name(icon));
    let label = widgets::label(title, "nav-label");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.add_css_class("compact-hide");
    content.append(&label);
    if count > 0 {
        let badge = widgets::label(&count.to_string(), "nav-readout");
        badge.add_css_class("compact-hide");
        content.append(&badge);
    }
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(tooltip));
    button
}

fn list_button(list: &ListInfo, count: usize, active: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("nav-item");
    if active {
        button.add_css_class("active");
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let swatch = gtk::DrawingArea::new();
    swatch.set_content_width(16);
    swatch.set_content_height(16);
    swatch.set_valign(gtk::Align::Center);
    let color = list.color.clone();
    swatch.set_draw_func(move |_, cr, w, h| {
        let c = color.as_deref().map(Rgb::hex).unwrap_or_else(|| Rgb::hex(&nyx_theme::palette().accent));
        cr.set_source_rgb(c.0, c.1, c.2);
        cr.arc(w as f64 / 2.0, h as f64 / 2.0, 6.0, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    });
    content.append(&swatch);
    let label = widgets::label(&list.name, "nav-label");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.add_css_class("compact-hide");
    content.append(&label);
    if list.offline {
        let offline = gtk::Image::from_icon_name("network-offline-symbolic");
        offline.add_css_class("compact-hide");
        offline.add_css_class("dim");
        offline.set_tooltip_text(Some("Offline: showing the saved copy"));
        content.append(&offline);
    } else if count > 0 {
        let badge = widgets::label(&count.to_string(), "nav-readout");
        badge.add_css_class("compact-hide");
        content.append(&badge);
    }
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(&list.name));
    let uid = list.uid.clone();
    button.connect_clicked(move |_| show_page(Page::List(uid.clone())));
    crate::menus::attach_list(&button, list);
    button
}

/// A group heading; `folded` is set for an account's, which folds its lists away.
fn group_label(text: &str, first: bool, add: bool, folded: Option<bool>) -> gtk::Widget {
    let label = widgets::label(&text.to_uppercase(), "nav-group");
    label.add_css_class("compact-hide");
    if first {
        label.add_css_class("first");
    }
    if !add && folded.is_none() {
        return label.upcast();
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("compact-hide");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    row.append(&label);
    if add {
        let plus = widgets::icon_button("list-add-symbolic", "Add an account");
        plus.add_css_class("flat");
        plus.add_css_class("nav-group-add");
        plus.set_valign(gtk::Align::End);
        plus.connect_clicked(|_| open_online_accounts());
        row.append(&plus);
    }
    if let Some(folded) = folded {
        let (icon, tip) = if folded { ("pan-end-symbolic", "Show its lists") } else { ("pan-down-symbolic", "Hide its lists") };
        let fold = widgets::icon_button(icon, tip);
        fold.add_css_class("flat");
        fold.add_css_class("nav-group-add");
        fold.set_valign(gtk::Align::End);
        let key = text.to_string();
        fold.connect_clicked(move |_| toggle_group_folded(&key));
        row.append(&fold);
        let click = gtk::GestureClick::new();
        let key = text.to_string();
        click.connect_released(move |_, _, _, _| toggle_group_folded(&key));
        label.add_controller(click);
    }
    row.upcast()
}

/// Folds or unfolds an account's lists in the sidebar.
pub(crate) fn toggle_group_folded(key: &str) {
    prefs::update(|p| {
        if p.collapsed_groups.iter().any(|g| g == key) {
            p.collapsed_groups.retain(|g| g != key);
        } else {
            p.collapsed_groups.push(key.to_string());
        }
    });
    rebuild_nav();
}

pub(crate) fn rebuild_nav() {
    let Some(ui) = ui() else { return };
    let list_box = ui.borrow().nav_list.clone();
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    let (page, lists, tasks) = {
        let u = ui.borrow();
        (u.page.clone(), u.lists.clone(), u.tasks.clone())
    };
    list_box.append(&group_label("Smart lists", true, false, None));
    for (smart, icon, key) in [
        (Smart::Today, "x-office-calendar-symbolic", "1"),
        (Smart::Scheduled, "alarm-symbolic", "2"),
        (Smart::Flagged, "starred-symbolic", "3"),
        (Smart::All, "view-list-symbolic", "4"),
        (Smart::Completed, "emblem-ok-symbolic", ""),
    ] {
        let count = if smart == Smart::Completed { 0 } else { smart_count(&ui.borrow(), smart) };
        let tooltip = if key.is_empty() { smart.name().to_string() } else { format!("{} ({key})", smart.name()) };
        let button = nav_button(icon, smart.name(), &tooltip, count, page == Page::Smart(smart));
        button.connect_clicked(move |_| show_page(Page::Smart(smart)));
        list_box.append(&button);
    }

    let mut groups: Vec<(String, Vec<ListInfo>)> = Vec::new();
    for list in lists {
        let label = list.group_label().to_string();
        match groups.iter_mut().find(|(g, _)| *g == label) {
            Some((_, items)) => items.push(list),
            None => groups.push((label, vec![list])),
        }
    }
    let folded_groups = prefs::get().collapsed_groups;
    for (i, (label, items)) in groups.iter().enumerate() {
        let folded = folded_groups.contains(label);
        list_box.append(&group_label(label, false, i == 0, Some(folded)));
        if folded {
            continue;
        }
        for list in items {
            let open = tasks.iter().filter(|t| t.list == list.uid && !t.done).count();
            list_box.append(&list_button(list, open, page == Page::List(list.uid.clone())));
        }
    }

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    list_box.append(&spacer);
    let accounts = nav_button("system-users-symbolic", "Online accounts", "Add or sign in to accounts in Settings", 0, false);
    accounts.connect_clicked(|_| open_online_accounts());
    list_box.append(&accounts);

    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

// ---------- Building ----------

fn build(app: &gtk::Application) {
    let window =
        gtk::ApplicationWindow::builder().application(app).title("Reminders").default_width(1120).default_height(800).build();
    window.add_css_class("reminders-window");
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_icon_name(Some("io.github.design_nexus.nyx.Reminders"));
    nyx_ui::keys::install(
        &window,
        nyx_ui::keys::Common {
            settings: Some(settings_dialog::open),
            shortcuts: Some(show_shortcuts),
            close: true,
            ..Default::default()
        },
    );

    let nav = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav.add_css_class("settings-navigation");
    nav.set_hexpand(false);
    let nav_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_list.set_hexpand(false);
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hexpand(false)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&nav_list)
        .build();
    // The sidebar's top: finding reminders, and its toggle (centred on the narrow rail).
    let nav_top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    nav_top.add_css_class("nav-top");
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Find reminders  (Ctrl+F)"));
    search.update_property(&[gtk::accessible::Property::Label("Find reminders")]);
    search.add_css_class("sidebar-search");
    search.add_css_class("compact-hide");
    search.set_hexpand(true);
    nav_top.append(&search);
    let collapse = nyx_ui::widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.add_css_class("nav-toggle");
    collapse.connect_clicked(|_| toggle_sidebar());
    nav_top.append(&collapse);
    nav.append(&nav_top);
    nav.append(&nav_scroll);
    search.connect_search_changed(|_| refresh_page());
    search.connect_stop_search(|entry| entry.set_text(""));

    // The sidebar's foot: what's shown and whether it's current, and Refresh.
    let nav_foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    nav_foot.add_css_class("nav-foot");
    let readout = widgets::label("", "status-readout");
    readout.set_ellipsize(gtk::pango::EllipsizeMode::End);
    readout.set_hexpand(true);
    readout.set_xalign(0.0);
    readout.add_css_class("compact-hide");
    STATUS.with(|s| *s.borrow_mut() = Some(readout.clone()));
    nav_foot.append(&readout);
    let reload = nyx_ui::widgets::bar_button("view-refresh-symbolic", "Refresh (R)");
    reload.connect_clicked(|_| refresh());
    nav_foot.append(&reload);
    nav.append(&nav_foot);

    let banners = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banners.add_css_class("banners");
    banners.set_visible(false);

    // Quick add sits in the header; what it understood shows over the list.
    let quick = gtk::Entry::new();
    quick.add_css_class("quick-add");
    quick.add_css_class("bar-search");
    quick.set_width_chars(36);
    quick.set_placeholder_text(Some("Add a reminder — “Call Sam tomorrow at 9”"));
    quick.update_property(&[gtk::accessible::Property::Label("Add a reminder")]);
    quick.set_primary_icon_name(Some("list-add-symbolic"));
    let quick_note = widgets::label("", "quick-note");
    quick_note.set_xalign(0.0);
    quick_note.set_visible(false);
    {
        let note = quick_note.clone();
        quick.connect_changed(move |e| {
            let text = e.text().to_string();
            let parse = nyx_when::parse(&text, actions::now());
            let mut parts = Vec::new();
            if let Some(date) = parse.date {
                let due = match parse.start {
                    Some(t) => nyx_reminders_core::Due::At(date.and_time(t)),
                    None => nyx_reminders_core::Due::Day(date),
                };
                parts.push(actions::due_text(due, actions::now()));
            }
            if parse.repeat.is_some() {
                parts.push("repeats".into());
            }
            note.set_visible(!parts.is_empty() && !text.trim().is_empty());
            note.set_text(&format!("“{}” · {}", parse.title.trim(), parts.join(" · ")));
        });
    }
    quick.connect_activate(|e| {
        let text = e.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        e.set_text("");
        add_from_quick(&text);
    });
    let quick_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    quick_box.add_css_class("quick-box");
    quick_box.append(&quick_note);

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rows.add_css_class("task-list");
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_column.append(&quick_box);
    list_column.append(&rows);
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .hexpand(true)
        .child(&list_column)
        .build();
    let detail = gtk::Box::new(gtk::Orientation::Vertical, 10);
    detail.add_css_class("detail-panel");
    let detail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&detail)
        .width_request(detail::WIDTH)
        .build();
    detail_scroll.add_css_class("detail-scroll");
    detail_scroll.add_css_class("inspector");
    detail_scroll.set_hexpand(false);
    detail_scroll.set_visible(false);
    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.append(&list_scroll);
    split.append(&detail_scroll);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("settings-content");
    content.set_hexpand(true);
    content.append(&banners);
    content.append(&split);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.append(&nav);
    body.append(&content);

    // The header: the app with the page as its subtitle, quick add, and Settings.
    let header = nyx_ui::chrome::app_header(&window, "checkbox-checked-symbolic", "Reminders");
    header.center.append(&quick);
    header.end.append(&nyx_ui::chrome::settings_button(settings_dialog::open));
    let title = header.subtitle.clone();

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("window-frame");
    frame.append(&header.root);
    frame.append(&body);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&frame));
    window.set_child(Some(&overlay));
    install_keys(&window, &search, &quick);

    let sidecar = match Sidecar::open(&nyx_core::paths::data_dir().join("reminders.sqlite")) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("nyx-reminders: flags won't be saved: {e}");
            None
        }
    };
    let ui = Ui {
        window: window.clone(),
        overlay: overlay.clone(),
        nav: nav.clone(),
        nav_list,
        search,
        banners,
        title,
        quick,
        rows,
        detail,
        detail_scroll,
        page: Page::Smart(Smart::Today),
        lists: Vec::new(),
        tasks: Vec::new(),
        meta: HashMap::new(),
        sidecar,
        selected: None,
        shown_detail: None,
        order: Vec::new(),
        unavailable: None,
        unsupported: Vec::new(),
        loaded: false,
        refreshing: false,
    };
    UI.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(ui))));

    let apply_width = {
        let nav = nav.clone();
        let window = window.clone();
        move || {
            let width = if window.width() > 0 { window.width() } else { window.default_width() };
            let narrow = width > 0 && width < 980;
            settings_dialog::fit(&window);
            let prev = NARROW.with(|n| n.get());
            let sized = nav.has_css_class("sized");
            if narrow == prev && sized {
                return;
            }
            nav.add_css_class("sized");
            NARROW.with(|n| n.set(narrow));
            apply_compact(&nav, narrow || prefs::get().sidebar_collapsed);
        }
    };
    let aw = apply_width.clone();
    window.connect_realize(move |_| aw());
    let probe = gtk::DrawingArea::new();
    probe.set_can_target(false);
    probe.set_can_focus(false);
    overlay.add_overlay(&probe);
    overlay.set_measure_overlay(&probe, false);
    probe.connect_resize(move |_, _, _| {
        let aw = apply_width.clone();
        glib::idle_add_local_once(aw);
    });
    // Due times move on: "Today" becomes "Yesterday", things become overdue.
    glib::timeout_add_seconds_local(60, || {
        refresh_page();
        glib::ControlFlow::Continue
    });
    redraw();
}

fn apply_compact(nav: &gtk::Box, compact: bool) {
    if compact {
        nav.add_css_class("compact");
    } else {
        nav.remove_css_class("compact");
    }
    fn walk(widget: &gtk::Widget, compact: bool) {
        if widget.has_css_class("nav-toggle") {
            widget.set_hexpand(compact);
            widget.set_halign(if compact { gtk::Align::Center } else { gtk::Align::End });
        }
        if widget.has_css_class("compact-hide") {
            widget.set_visible(!compact);
        }
        if widget.has_css_class("nav-item")
            && let Some(content) = widget.downcast_ref::<gtk::Button>().and_then(|b| b.child())
        {
            content.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            walk(&next, compact);
            child = next.next_sibling();
        }
    }
    walk(nav.upcast_ref(), compact);
}

pub fn toggle_sidebar() {
    prefs::update(|p| p.sidebar_collapsed = !p.sidebar_collapsed);
    let Some(ui) = ui() else { return };
    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

fn typing(window: &gtk::Window) -> bool {
    let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else { return false };
    focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.ancestor(gtk::Entry::static_type()).is_some()
}

fn install_keys(window: &gtk::ApplicationWindow, search: &gtk::SearchEntry, quick: &gtk::Entry) {
    let keys = gtk::EventControllerKey::new();
    let (search, quick, win) = (search.clone(), quick.clone(), window.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        if ctrl {
            match key {
                gdk::Key::f => {
                    search.grab_focus();
                }
                gdk::Key::b => toggle_sidebar(),
                gdk::Key::n => {
                    quick.grab_focus();
                }
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Escape {
            if typing(win.upcast_ref()) && quick.has_focus() {
                gtk::prelude::GtkWindowExt::set_focus(&win, None::<&gtk::Widget>);
                return glib::Propagation::Stop;
            }
            if !search.text().is_empty() {
                search.set_text("");
                return glib::Propagation::Stop;
            }
            if selected_task().is_some() {
                select(None);
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if typing(win.upcast_ref()) {
            return glib::Propagation::Proceed;
        }
        if widgets::is_menu_key(key, mods) {
            menu_for_selected();
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::n => {
                quick.grab_focus();
            }
            gdk::Key::j | gdk::Key::Down => move_selection(1),
            gdk::Key::k | gdk::Key::Up => move_selection(-1),
            gdk::Key::space => {
                if let Some(t) = selected_task() {
                    actions::set_done(&t, !t.done);
                }
            }
            gdk::Key::s => {
                if let Some(t) = selected_task() {
                    let flagged = meta_of(&t.key()).flagged;
                    set_flag(&t, !flagged);
                }
            }
            gdk::Key::Delete => {
                if let Some(t) = selected_task() {
                    let subs: Vec<String> = tasks()
                        .iter()
                        .filter(|s| s.parent.as_deref() == Some(t.uid.as_str()) && s.list == t.list)
                        .map(|s| s.uid.clone())
                        .collect();
                    select(None);
                    actions::delete(&t, subs);
                }
            }
            gdk::Key::_1 => show_page(Page::Smart(Smart::Today)),
            gdk::Key::_2 => show_page(Page::Smart(Smart::Scheduled)),
            gdk::Key::_3 => show_page(Page::Smart(Smart::Flagged)),
            gdk::Key::_4 => show_page(Page::Smart(Smart::All)),
            gdk::Key::r => refresh(),
            gdk::Key::slash => {
                search.grab_focus();
            }
            gdk::Key::F1 => show_shortcuts(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}

pub fn show_shortcuts() {
    let (dialog, card) = widgets::dialog("Keyboard shortcuts", 520);
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    for (keys, what) in SHORTCUTS {
        list.append(&widgets::row(what, "", Some(widgets::key_caps(keys).upcast_ref())));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(560)
        .child(&list)
        .build();
    card.append(&scroll);
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    let dialog2 = dialog.clone();
    close.connect_clicked(move |_| dialog2.close());
    card.append(&close);
    dialog.present();
}

pub fn toast(message: &str) {
    show_toast(message, None);
}

/// A toast with Undo; it stays 6 s.
pub fn toast_undo(message: &str, undo: impl Fn() + 'static) {
    show_toast(message, Some(Box::new(undo)));
}

fn show_toast(message: &str, undo: Option<Box<dyn Fn()>>) {
    let Some(ui) = ui() else {
        eprintln!("nyx-reminders: {message}");
        return;
    };
    let overlay = ui.borrow().overlay.clone();
    if let Some(old) = TOAST.with(|t| t.borrow_mut().take())
        && old.parent().is_some()
    {
        overlay.remove_overlay(&old);
    }
    let label = gtk::Label::new(Some(message));
    label.set_wrap(true);
    label.set_max_width_chars(70);
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bx.add_css_class("toast");
    bx.set_halign(gtk::Align::Center);
    bx.set_valign(gtk::Align::End);
    bx.set_margin_bottom(48);
    bx.append(&label);
    let lasts = if undo.is_some() { 6000 } else { 3500 };
    if let Some(undo) = undo {
        let button = gtk::Button::with_label("Undo");
        let (o, b) = (overlay.clone(), bx.clone());
        button.connect_clicked(move |_| {
            undo();
            if b.parent().is_some() {
                o.remove_overlay(&b);
            }
        });
        bx.append(&button);
    }
    overlay.add_overlay(&bx);
    TOAST.with(|t| *t.borrow_mut() = Some(bx.clone()));
    glib::timeout_add_local_once(std::time::Duration::from_millis(lasts), move || {
        if bx.parent().is_some() {
            overlay.remove_overlay(&bx);
        }
    });
}
