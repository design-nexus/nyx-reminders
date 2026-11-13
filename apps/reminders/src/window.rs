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
    /// The page bar: its name, what it holds, Show completed and the order.
    title: gtk::Label,
    page_sub: gtk::Label,
    show_done: gtk::Switch,
    show_done_box: gtk::Box,
    order: gtk::MenuButton,
    order_label: gtk::Label,
    /// Quick add, the list it adds to, and what it understood.
    quick: gtk::Entry,
    dest_dot: gtk::Label,
    dest_name: gtk::Label,
    parse: gtk::Box,
    /// A list chosen for quick add on this page (else the page's or the default).
    quick_target: Option<String>,
    rows: gtk::Box,
    detail: gtk::Box,
    detail_foot: gtk::Box,
    detail_side: gtk::Box,
    page: Page,
    lists: Vec<ListInfo>,
    tasks: Vec<Task>,
    meta: HashMap<Key, Meta>,
    sidecar: Option<Sidecar>,
    /// The task whose details show, and what was shown.
    selected: Option<Key>,
    shown_detail: Option<Task>,
    /// Row order on screen, for J/K.
    row_order: Vec<Key>,
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
    static STATUS: RefCell<Option<(gtk::Image, gtk::Label)>> = const { RefCell::new(None) };
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
        u.quick_target = None;
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
            let m = u.meta.get(&t.key());
            let on_page = match &u.page {
                Page::Smart(Smart::Completed) => Smart::Completed.contains(t, m, today),
                // With Show completed, a smart list keeps the done ones it would hold open.
                Page::Smart(s) => {
                    s.contains(t, m, today) || (show_done && t.done && s.contains(&Task { done: false, ..t.clone() }, m, today))
                }
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
    let no_positions = HashMap::new();
    let mode = sort_mode(&u.page);
    let positions = if mode == "manual" { &u.meta } else { &no_positions };
    sort(&u.tasks, &mut picked, positions);
    if mode == "priority" {
        // Highest first, keeping Today's and Scheduled's day groups together.
        let now = actions::now();
        let grouped = matches!(u.page, Page::Smart(Smart::Today | Smart::Scheduled));
        let mut groups: Vec<&str> = Vec::new();
        let rank: HashMap<usize, usize> = picked
            .iter()
            .map(|&i| {
                let g = if grouped { date_group(&u.tasks[i], now) } else { "" };
                let at = groups.iter().position(|x| *x == g).unwrap_or_else(|| {
                    groups.push(g);
                    groups.len() - 1
                });
                (i, at)
            })
            .collect();
        picked.sort_by_key(|i| (rank[i], u.tasks[*i].done, u.tasks[*i].priority.unwrap_or(10)));
    }
    // Smart lists flatten subtasks; a list nests them.
    match u.page {
        Page::List(_) => tree(&u.tasks, &picked),
        Page::Smart(_) => picked.into_iter().map(|i| (i, 0)).collect(),
    }
}

/// The page's order: Today and Scheduled are by date or priority within each day; the
/// other pages manual, by date or by priority.
fn sort_mode(page: &Page) -> String {
    let p = prefs::get();
    if fixed_order(page) {
        if p.sort_fixed == "priority" { "priority".into() } else { "date".into() }
    } else {
        match p.sort_lists.as_str() {
            "date" | "priority" => p.sort_lists,
            _ => "manual".into(),
        }
    }
}

/// Today and Scheduled, which are grouped by day.
fn fixed_order(page: &Page) -> bool {
    matches!(page, Page::Smart(Smart::Today | Smart::Scheduled))
}

/// The order menu's choices for a page: (key, label).
fn sort_choices(page: &Page) -> &'static [(&'static str, &'static str)] {
    if fixed_order(page) {
        &[("date", "By date"), ("priority", "By priority")]
    } else {
        &[("manual", "Manual order"), ("date", "By date"), ("priority", "By priority")]
    }
}

/// The date group a task falls in on Today and Scheduled.
fn date_group(task: &Task, now: chrono::NaiveDateTime) -> &'static str {
    if task.done {
        return "Completed";
    }
    let Some(due) = task.due else { return "No date" };
    let days = (due.day() - now.date()).num_days();
    match days {
        _ if task.overdue(now) => "Overdue",
        0 => "Today",
        1 => "Tomorrow",
        2..=6 => "This week",
        _ => "Later",
    }
}

fn smart_count(u: &Ui, smart: Smart) -> usize {
    let today = actions::now().date();
    u.tasks.iter().filter(|t| smart.contains(t, u.meta.get(&t.key()), today)).count()
}

/// What a page says when it holds nothing, and what can be done about it.
fn empty_page(page: &Page, query: &str, list_name: &str) -> gtk::Box {
    let b = widgets::vbox(8);
    b.add_css_class("empty-state");
    b.set_valign(gtk::Align::Center);
    b.set_halign(gtk::Align::Center);
    b.set_vexpand(true);
    let (icon, title, desc): (&str, String, String) = if !query.is_empty() {
        (
            "system-search-symbolic",
            format!("No reminders match “{query}”"),
            "Search looks at titles, notes and #tags.".into(),
        )
    } else {
        match page {
            Page::Smart(Smart::Today) => ("weather-clear-symbolic", "Nothing due today".into(), "Type above to add one; it's due today.".into()),
            Page::Smart(Smart::Flagged) => (
                "nyx-reminders-flag-symbolic",
                "No flagged reminders".into(),
                "Press S on a reminder to flag it, or add one here.".into(),
            ),
            Page::Smart(Smart::Completed) => ("emblem-ok-symbolic", "Nothing completed yet".into(), String::new()),
            Page::List(_) => (
                "emblem-ok-symbolic",
                format!("Nothing left in {list_name}"),
                "Type above to add one — “Water the plants every 3 days” sets the repeat for you.".into(),
            ),
            _ => ("view-list-symbolic", "No reminders".into(), "Type above to add one.".into()),
        }
    };
    let i = gtk::Image::from_icon_name(icon);
    i.set_pixel_size(40);
    i.add_css_class("dim");
    b.append(&i);
    b.append(&widgets::label(&title, "empty-title"));
    if !desc.is_empty() {
        let d = widgets::label(&desc, "dim");
        d.set_wrap(true);
        d.set_justify(gtk::Justification::Center);
        d.set_max_width_chars(48);
        b.append(&d);
    }
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(8);
    if !query.is_empty() {
        if *page != Page::Smart(Smart::All) {
            let every = gtk::Button::with_label("Search every list");
            every.add_css_class("suggested-action");
            every.connect_clicked(|_| {
                let Some(ui) = ui() else { return };
                ui.borrow_mut().page = Page::Smart(Smart::All);
                redraw();
            });
            buttons.append(&every);
        }
        let clear = gtk::Button::with_label("Clear the search");
        clear.add_css_class("flat");
        clear.connect_clicked(|_| {
            if let Some(ui) = ui() {
                let search = ui.borrow().search.clone();
                search.set_text("");
            }
        });
        buttons.append(&clear);
    } else if matches!(page, Page::List(_)) && !prefs::get().show_completed {
        let show = gtk::Button::with_label("Show completed");
        show.add_css_class("flat");
        show.connect_clicked(|_| {
            prefs::update(|p| p.show_completed = true);
            refresh_page();
        });
        buttons.append(&show);
    }
    if buttons.first_child().is_some() {
        b.append(&buttons);
    }
    b
}

fn rebuild_page() {
    let Some(ui) = ui() else { return };
    let (rows, entries, selected, title_text, quick, detail_box, foot, side) = {
        let u = ui.borrow();
        (
            u.rows.clone(),
            page_tasks(&u),
            u.selected.clone(),
            page_title(&u),
            u.quick.clone(),
            u.detail.clone(),
            u.detail_foot.clone(),
            u.detail_side.clone(),
        )
    };
    chrome::set_title(&ui.borrow().window, "Reminders", &title_text);
    let now = actions::now();
    let (tasks, lists, meta, page) = {
        let u = ui.borrow();
        (u.tasks.clone(), u.lists.clone(), u.meta.clone(), u.page.clone())
    };
    let can_add = {
        let u = ui.borrow();
        match &u.page {
            Page::Smart(Smart::Completed) => false,
            Page::List(uid) => u.lists.iter().any(|l| l.uid == *uid && !l.read_only),
            Page::Smart(_) => u.lists.iter().any(|l| !l.read_only),
        }
    };
    quick.set_sensitive(can_add);
    quick.parent().inspect(|p| p.set_visible(can_add));
    update_page_bar(&title_text, entries.len());
    update_dest();

    while let Some(child) = rows.first_child() {
        rows.remove(&child);
    }
    let mut order = Vec::new();
    let query = ui.borrow().search.text().trim().to_string();
    if entries.is_empty() && ui.borrow().loaded {
        rows.append(&empty_page(&page, &query, &title_text));
    }
    let grouped = matches!(page, Page::Smart(Smart::Today | Smart::Scheduled)) && query.is_empty();
    let mut group = "";
    let mut done_header = false;
    for (pos, (index, depth)) in entries.iter().enumerate() {
        let task = &tasks[*index];
        if grouped {
            let g = date_group(task, now);
            if g != group {
                group = g;
                let count = entries[pos..].iter().take_while(|(i, _)| date_group(&tasks[*i], now) == g).count();
                rows.append(&group_heading(g, count, g == "Overdue", pos == 0));
            }
        } else if task.done && !done_header && !matches!(page, Page::Smart(Smart::Completed)) {
            rows.append(&group_heading("Completed", 0, false, pos == 0));
            done_header = true;
        }
        let list = matches!(page, Page::Smart(_))
            .then(|| lists.iter().find(|l| l.uid == task.list).map(|l| (l.name.clone(), l.color.clone())))
            .flatten();
        let subs: Vec<&Task> = tasks.iter().filter(|t| t.parent.as_deref() == Some(task.uid.as_str()) && t.list == task.list).collect();
        let sub_count = (!subs.is_empty() && matches!(page, Page::Smart(_)))
            .then(|| (subs.iter().filter(|s| s.done).count(), subs.len()));
        let row = task_row(task, *depth, meta.get(&task.key()), list, sub_count, selected.as_ref() == Some(&task.key()), now);
        // A list's open reminders reorder by dragging (not while searching or in date order).
        if matches!(page, Page::List(_)) && !task.done && query.is_empty() && sort_mode(&page) == "manual" {
            attach_drag(&row, task.key());
        }
        rows.append(&row);
        order.push(task.key());
    }
    ui.borrow_mut().row_order = order;

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
                detail::fill(&detail_box, &foot, &task, meta.get(&task.key()).unwrap_or(&Meta::default()), &subtasks);
                ui.borrow_mut().shown_detail = Some(task);
            }
            side.set_visible(true);
        }
        None => {
            ui.borrow_mut().shown_detail = None;
            side.set_visible(false);
        }
    }
}

/// A date group's heading over its reminders ("Overdue 1").
fn group_heading(text: &str, count: usize, overdue: bool, first: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("task-group");
    if overdue {
        row.add_css_class("overdue");
    }
    if first {
        row.add_css_class("first");
    }
    row.append(&widgets::label(text, "task-group-name"));
    if count > 0 {
        row.append(&widgets::label(&count.to_string(), "task-group-count"));
    }
    row
}

/// The page bar: the page's name, what it holds, and its switches.
fn update_page_bar(title: &str, shown: usize) {
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    u.title.set_text(title);
    let sub = match &u.page {
        Page::Smart(Smart::Today) => actions::now().date().format("%A %-d %B").to_string(),
        Page::List(uid) => {
            let open = u.tasks.iter().filter(|t| t.list == *uid && !t.done).count();
            let group = u.lists.iter().find(|l| l.uid == *uid).map(|l| l.group_label().to_string()).unwrap_or_default();
            format!("{group} · {open} open")
        }
        _ => match shown {
            1 => "1 reminder".into(),
            n => format!("{n} reminders"),
        },
    };
    u.page_sub.set_text(&sub);
    let completed = u.page == Page::Smart(Smart::Completed);
    u.show_done_box.set_visible(!completed);
    if u.show_done.is_active() != prefs::get().show_completed {
        u.show_done.set_active(prefs::get().show_completed);
    }
    u.order.set_visible(!completed);
    let fixed = fixed_order(&u.page);
    u.order.set_tooltip_text(Some(if fixed {
        "Within each day: by time, or highest priority first"
    } else {
        "Manual keeps the order you drag reminders into"
    }));
    let mode = sort_mode(&u.page);
    let label = sort_choices(&u.page).iter().find(|(k, _)| *k == mode).map(|(_, l)| *l).unwrap_or("By date");
    u.order_label.set_text(label);
}

/// A small chip in a reminder's line: an icon and a few words.
fn chip(icon: &str, text: &str, class: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    b.add_css_class("task-chip");
    if !class.is_empty() {
        b.add_css_class(class);
    }
    if !icon.is_empty() {
        b.append(&gtk::Image::from_icon_name(icon));
    }
    if !text.is_empty() {
        b.append(&gtk::Label::new(Some(text)));
    }
    b
}

/// A list's colour dot.
fn list_dot(color: Option<String>, size: i32) -> gtk::DrawingArea {
    let dot = gtk::DrawingArea::new();
    dot.set_content_width(size);
    dot.set_content_height(size);
    dot.set_valign(gtk::Align::Center);
    dot.set_draw_func(move |_, cr, w, h| {
        let c = color.as_deref().map(Rgb::hex).unwrap_or_else(|| Rgb::hex(&nyx_theme::palette().accent));
        cr.set_source_rgb(c.0, c.1, c.2);
        cr.arc(w as f64 / 2.0, h as f64 / 2.0, w.min(h) as f64 / 2.0 - 0.5, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    });
    dot
}

fn task_row(
    task: &Task,
    depth: usize,
    meta: Option<&Meta>,
    list: Option<(String, Option<String>)>,
    subs: Option<(usize, usize)>,
    selected: bool,
    now: chrono::NaiveDateTime,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("task-row");
    if selected {
        row.add_css_class("selected");
    }
    row.set_margin_start(depth as i32 * 30);
    // Priority as a thin bar at the row's edge.
    let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bar.add_css_class("prio-bar");
    match task.priority_level().filter(|_| !task.done) {
        Some(Level::High) => bar.add_css_class("high"),
        Some(Level::Medium) => bar.add_css_class("medium"),
        Some(Level::Low) => bar.add_css_class("low"),
        None => {}
    }
    if let Some(level) = task.priority_level() {
        bar.set_tooltip_text(Some(match level {
            Level::High => "High priority",
            Level::Medium => "Medium priority",
            Level::Low => "Low priority",
        }));
    }
    row.append(&bar);
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

    let text = gtk::Box::new(gtk::Orientation::Vertical, 5);
    text.set_hexpand(true);
    let title = widgets::label(&task.title, "task-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    if task.done {
        title.add_css_class("done");
    }
    text.append(&title);
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    line.add_css_class("task-line");
    if let Some(due) = task.due {
        line.append(&chip("x-office-calendar-symbolic", &actions::due_text(due, now), if task.overdue(now) { "overdue" } else { "" }));
    }
    if let Some(rule) = &task.rule {
        line.append(&chip("media-playlist-repeat-symbolic", &actions::repeat_text(rule), ""));
    }
    if let Some((name, color)) = list {
        let l = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        l.add_css_class("task-list-name");
        l.append(&list_dot(color, 8));
        l.append(&gtk::Label::new(Some(&name)));
        line.append(&l);
    }
    for tag in &task.tags {
        line.append(&widgets::label(&format!("#{tag}"), "task-tag"));
    }
    if let Some((done, total)) = subs {
        line.append(&chip("view-list-symbolic", &format!("{done} of {total}"), "plain"));
    }
    if !task.notes.trim().is_empty() {
        let first: String = task.notes.lines().next().unwrap_or("").chars().take(80).collect();
        let l = widgets::label(&first, "task-note");
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_hexpand(true);
        l.set_xalign(0.0);
        line.append(&l);
    }
    if line.first_child().is_some() {
        text.append(&line);
    }
    row.append(&text);
    if meta.is_some_and(|m| m.flagged) {
        let f = gtk::Image::from_icon_name("nyx-reminders-flag-symbolic");
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
    let mut order: Vec<(String, String)> = ui.borrow().row_order.iter().filter(|k| k.0 == moved.0 && **k != moved).cloned().collect();
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

/// The list quick add adds to: the one picked for this page, else the page's
/// own list, else where new reminders go.
fn quick_list() -> String {
    let Some(ui) = ui() else { return String::new() };
    let u = ui.borrow();
    let writable = |uid: &str| u.lists.iter().any(|l| l.uid == uid && !l.read_only);
    if let Some(t) = u.quick_target.as_deref().filter(|t| writable(t)) {
        return t.to_string();
    }
    if let Page::List(uid) = &u.page
        && writable(uid)
    {
        return uid.clone();
    }
    drop(u);
    default_list()
}

/// The quick-add chip: the list's dot and name.
fn update_dest() {
    let Some(ui) = ui() else { return };
    let uid = quick_list();
    let u = ui.borrow();
    let list = u.lists.iter().find(|l| l.uid == uid);
    u.dest_name.set_text(list.map(|l| l.name.as_str()).unwrap_or("No list"));
    let color = list.and_then(|l| l.color.clone()).unwrap_or_else(|| nyx_theme::palette().accent);
    u.dest_dot.set_markup(&format!("<span foreground=\"{}\">●</span>", glib::markup_escape_text(&color)));
}

/// What quick add understood, as chips under it, and where it goes.
fn update_parse(text: &str) {
    let Some(ui) = ui() else { return };
    let parse = ui.borrow().parse.clone();
    while let Some(c) = parse.first_child() {
        parse.remove(&c);
    }
    if text.trim().is_empty() {
        parse.set_visible(false);
        return;
    }
    let list = quick_list();
    let page = ui.borrow().page.clone();
    let today = actions::now().date();
    let default_day = matches!(page, Page::Smart(Smart::Today)).then_some(today);
    let q = actions::parse_quick(text, actions::now(), list_caps(&list), default_day);
    parse.append(&widgets::label("Adds", "quick-word"));
    if let Some(due) = q.due {
        parse.append(&chip("x-office-calendar-symbolic", &actions::due_text(due, actions::now()), "accent"));
    }
    if let Some(rule) = &q.repeat {
        parse.append(&chip("media-playlist-repeat-symbolic", &actions::repeat_text(rule), "accent"));
    }
    for tag in &q.tags {
        parse.append(&chip("", &format!("#{tag}"), "accent"));
    }
    if matches!(page, Page::Smart(Smart::Flagged)) {
        parse.append(&chip("nyx-reminders-flag-symbolic", "Flagged", "accent"));
    }
    let name = lists().iter().find(|l| l.uid == list).map(|l| l.name.clone()).unwrap_or_default();
    let to = gtk::Label::new(None);
    to.add_css_class("quick-word");
    let mut tail = format!("to <b>{}</b> · Enter adds", glib::markup_escape_text(&name));
    if !q.dropped.is_empty() {
        tail.push_str(&format!(" · {} can't keep a {}", glib::markup_escape_text(&name), q.dropped.join(" or ")));
    }
    to.set_markup(&tail);
    parse.append(&to);
    parse.set_visible(true);
}

fn add_from_quick(text: &str) {
    let Some(ui) = ui() else { return };
    let page = ui.borrow().page.clone();
    let list = quick_list();
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
    let elsewhere = !matches!(&page, Page::List(uid) if *uid == list);
    actions::add(list.clone(), quick, None, None, move |result| match result {
        Ok(uid) => {
            if flag {
                set_flag(&Task { list: list.clone(), uid, ..placeholder() }, true);
            }
            if !dropped.is_empty() {
                toast(&format!("Added to {list_name}, without a {} ({list_name} can't keep one).", dropped.join(" or ")));
            } else if elsewhere {
                toast(&format!("Added to {list_name}."));
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
    let order = ui.borrow().row_order.clone();
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

/// The sidebar's foot: whether the lists are current, said once.
fn refresh_status() {
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    let (icon, class, text) = if u.refreshing {
        ("view-refresh-symbolic", "", "Refreshing…".to_string())
    } else if u.lists.iter().any(|l| l.offline) {
        ("network-offline-symbolic", "warn", "Offline · changes are kept and sent later".to_string())
    } else if u.lists.iter().any(|l| l.needs_sign_in) {
        ("dialog-warning-symbolic", "warn", "An account needs you to sign in".to_string())
    } else {
        ("emblem-ok-symbolic", "ok", "Up to date".to_string())
    };
    STATUS.with(|s| {
        if let Some((image, label)) = s.borrow().as_ref() {
            image.set_icon_name(Some(icon));
            for c in ["ok", "warn"] {
                image.remove_css_class(c);
            }
            if !class.is_empty() {
                image.add_css_class(class);
            }
            label.set_text(&text);
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
                "<b>{}: its task lists are unavailable</b>\nCheck Online Accounts, then refresh.",
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
        let banner = widgets::banner(
            &format!(
                "<b>{} needs you to sign in again</b>\nIts lists still show what was last synced; new changes wait until you sign in.",
                glib::markup_escape_text(&group)
            ),
            true,
        );
        let sign_in = gtk::Button::with_label("Sign in");
        sign_in.add_css_class("suggested-action");
        sign_in.set_valign(gtk::Align::Center);
        sign_in.connect_clicked(|_| open_online_accounts());
        banner.append(&sign_in);
        banners.append(&banner);
        asked.push(group);
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
    let swatch = list_dot(list.color.clone(), 10);
    swatch.set_margin_start(3);
    swatch.set_margin_end(3);
    content.append(&swatch);
    let label = widgets::label(&list.name, "nav-label");
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
    button.set_tooltip_text(Some(&list.name));
    let uid = list.uid.clone();
    button.connect_clicked(move |_| show_page(Page::List(uid.clone())));
    crate::menus::attach_list(&button, list);
    button
}

/// An account's heading, in its section's colour, which folds its lists away.
fn group_label(text: &str, folded: bool, section: &str) -> gtk::Widget {
    let row = nyx_ui::sidebar::group_label(text, false);
    nyx_ui::sidebar::set_section(&row, section);
    let label = row.first_child().unwrap_or_else(|| row.clone().upcast());
    let (icon, tip) = if folded { ("pan-end-symbolic", "Show its lists") } else { ("pan-down-symbolic", "Hide its lists") };
    let fold = widgets::icon_button(icon, tip);
    fold.add_css_class("flat");
    fold.add_css_class("nav-group-add");
    fold.set_valign(gtk::Align::Center);
    let key = text.to_string();
    fold.connect_clicked(move |_| toggle_group_folded(&key));
    row.append(&fold);
    let click = gtk::GestureClick::new();
    let key = text.to_string();
    click.connect_released(move |_, _, _, _| toggle_group_folded(&key));
    label.add_controller(click);
    row.upcast()
}

/// A smart list's tile: its icon on its colour, its count, its name.
fn smart_tile(smart: Smart, icon: &str, class: &str, count: usize, active: bool, key: &str) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("smart-tile");
    b.add_css_class(class);
    if active {
        b.add_css_class("active");
    }
    let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let disc = gtk::CenterBox::new();
    disc.add_css_class("tile-icon");
    disc.set_center_widget(Some(&gtk::Image::from_icon_name(icon)));
    disc.set_size_request(24, 24);
    disc.set_halign(gtk::Align::Start);
    disc.set_valign(gtk::Align::Center);
    top.append(&disc);
    let n = widgets::label(&count.to_string(), "tile-count");
    n.add_css_class("compact-hide");
    n.set_hexpand(true);
    n.set_xalign(1.0);
    top.append(&n);
    col.append(&top);
    let name = widgets::label(smart.name(), "tile-name");
    name.add_css_class("compact-hide");
    name.set_xalign(0.0);
    col.append(&name);
    b.set_child(Some(&col));
    b.set_tooltip_text(Some(&format!("{} ({key})", smart.name())));
    b.update_property(&[gtk::accessible::Property::Label(smart.name())]);
    b.connect_clicked(move |_| show_page(Page::Smart(smart)));
    b
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
    let tiles = gtk::FlowBox::new();
    tiles.add_css_class("smart-tiles");
    tiles.set_selection_mode(gtk::SelectionMode::None);
    tiles.set_homogeneous(true);
    tiles.set_min_children_per_line(2);
    tiles.set_max_children_per_line(2);
    tiles.set_row_spacing(6);
    tiles.set_column_spacing(6);
    for (smart, icon, class, key) in [
        (Smart::Today, "weather-clear-symbolic", "tile-today", "1"),
        (Smart::Scheduled, "x-office-calendar-symbolic", "tile-scheduled", "2"),
        (Smart::Flagged, "nyx-reminders-flag-symbolic", "tile-flagged", "3"),
        (Smart::All, "nyx-reminders-inbox-symbolic", "tile-all", "4"),
    ] {
        let count = smart_count(&ui.borrow(), smart);
        tiles.insert(&smart_tile(smart, icon, class, count, page == Page::Smart(smart), key), -1);
    }
    list_box.append(&tiles);
    let done = tasks.iter().filter(|t| t.done).count();
    let completed = nav_button("emblem-ok-symbolic", "Completed", "Completed", done, page == Page::Smart(Smart::Completed));
    completed.connect_clicked(|_| show_page(Page::Smart(Smart::Completed)));
    list_box.append(&completed);

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
        // Account groups take the swatches in turn (Design 2.0's coloured sections); the
        // icon rail shows a short bar in the colour where the heading was.
        let section = nyx_ui::sidebar::rotating_section(i);
        let bar = gtk::Separator::new(gtk::Orientation::Horizontal);
        bar.add_css_class("nav-divider");
        bar.add_css_class("compact-show");
        bar.add_css_class(&format!("section-{section}"));
        bar.set_visible(false);
        list_box.append(&bar);
        list_box.append(&group_label(label, folded, &section));
        if folded {
            continue;
        }
        for list in items {
            let open = tasks.iter().filter(|t| t.list == list.uid && !t.done).count();
            list_box.append(&list_button(list, open, page == Page::List(list.uid.clone())));
        }
    }

    let line = gtk::Separator::new(gtk::Orientation::Horizontal);
    line.add_css_class("nav-line");
    list_box.append(&line);
    let accounts = nav_button("system-users-symbolic", "Accounts", "Add an account or sign in again, in Settings", 0, false);
    accounts.connect_clicked(|_| open_online_accounts());
    list_box.append(&accounts);

    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
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
    nav.append(&nav_scroll);

    // The sidebar's foot: whether the lists are current, and Refresh.
    let nav_foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    nav_foot.add_css_class("nav-foot");
    let state = gtk::Image::from_icon_name("emblem-ok-symbolic");
    state.add_css_class("status-icon");
    state.add_css_class("compact-hide");
    nav_foot.append(&state);
    let readout = widgets::label("", "status-readout");
    readout.set_ellipsize(gtk::pango::EllipsizeMode::End);
    readout.set_hexpand(true);
    readout.set_xalign(0.0);
    readout.add_css_class("compact-hide");
    STATUS.with(|s| *s.borrow_mut() = Some((state.clone(), readout.clone())));
    nav_foot.append(&readout);
    let reload = nyx_ui::widgets::bar_button("view-refresh-symbolic", "Refresh (R)");
    reload.connect_clicked(|_| refresh());
    nav_foot.append(&reload);
    nav.append(&nav_foot);

    let banners = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banners.add_css_class("banners");
    banners.set_visible(false);

    // The page bar: name and what it holds, Show completed, the order.
    let page_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    page_bar.add_css_class("page-bar");
    let title = widgets::label("", "page-title");
    let page_sub = widgets::label("", "page-sub");
    page_sub.set_ellipsize(gtk::pango::EllipsizeMode::End);
    page_sub.set_hexpand(true);
    page_sub.set_xalign(0.0);
    page_bar.append(&title);
    page_bar.append(&page_sub);
    let show_done_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    show_done_box.add_css_class("page-toggle");
    let show_label = widgets::label("Show completed", "dim");
    show_done_box.append(&show_label);
    let show_done = gtk::Switch::new();
    show_done.set_valign(gtk::Align::Center);
    show_done.set_active(prefs::get().show_completed);
    show_done.update_relation(&[gtk::accessible::Relation::LabelledBy(&[show_label.upcast_ref()])]);
    show_done.connect_active_notify(|s| {
        if prefs::get().show_completed != s.is_active() {
            prefs::update(|p| p.show_completed = s.is_active());
            refresh_page();
        }
    });
    show_done_box.append(&show_done);
    page_bar.append(&show_done_box);
    // The order: a menu of this page's choices (Today and Scheduled have their own).
    let order = gtk::MenuButton::new();
    order.add_css_class("order-pick");
    order.set_valign(gtk::Align::Center);
    let order_face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let order_label = widgets::label("By date", "");
    order_face.append(&order_label);
    order_face.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    order.set_child(Some(&order_face));
    let order_pop = gtk::Popover::new();
    order_pop.add_css_class("menu-popover");
    order.set_popover(Some(&order_pop));
    order_pop.connect_show(|pop| {
        let Some(ui) = ui() else { return };
        let page = ui.borrow().page.clone();
        let current = sort_mode(&page);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 1);
        for (key, text) in sort_choices(&page) {
            let b = gtk::Button::new();
            b.add_css_class("flat");
            b.add_css_class("menu-item");
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let mark = if current == *key { gtk::Image::from_icon_name("object-select-symbolic") } else { gtk::Image::new() };
            mark.set_pixel_size(16);
            line.append(&mark);
            line.append(&widgets::label(text, ""));
            b.set_child(Some(&line));
            let (pop, fixed) = (pop.clone(), fixed_order(&page));
            b.connect_clicked(move |_| {
                pop.popdown();
                prefs::update(|p| if fixed { p.sort_fixed = key.to_string() } else { p.sort_lists = key.to_string() });
                refresh_page();
            });
            col.append(&b);
        }
        pop.set_child(Some(&col));
    });
    page_bar.append(&order);

    // Quick add at the top of the list: the line, and the list it adds to.
    let quick = gtk::Entry::new();
    quick.add_css_class("quick-add");
    quick.set_hexpand(true);
    quick.set_placeholder_text(Some("Add a reminder — “Call Sam tomorrow at 9”"));
    quick.update_property(&[gtk::accessible::Property::Label("Add a reminder")]);
    quick.set_primary_icon_name(Some("list-add-symbolic"));
    quick.connect_changed(|e| update_parse(&e.text()));
    quick.connect_activate(|e| {
        let text = e.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        e.set_text("");
        add_from_quick(&text);
    });
    let dest = gtk::MenuButton::new();
    dest.add_css_class("quick-dest");
    dest.set_tooltip_text(Some("The list new reminders go to from here"));
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    let dest_dot = gtk::Label::new(Some("●"));
    dest_dot.add_css_class("dest-dot");
    let dest_name = gtk::Label::new(None);
    dest_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    dest_name.set_max_width_chars(16);
    face.append(&dest_dot);
    face.append(&dest_name);
    face.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    dest.set_child(Some(&face));
    let dest_pop = gtk::Popover::new();
    dest_pop.add_css_class("menu-popover");
    dest_pop.set_has_arrow(false);
    let dest_list = gtk::Box::new(gtk::Orientation::Vertical, 1);
    dest_pop.set_child(Some(&dest_list));
    {
        let (list, pop) = (dest_list.clone(), dest_pop.clone());
        dest_pop.connect_show(move |_| {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            let current = quick_list();
            for l in lists().into_iter().filter(|l| !l.read_only) {
                let b = gtk::Button::new();
                b.add_css_class("flat");
                b.add_css_class("menu-item");
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                row.append(&list_dot(l.color.clone(), 9));
                let name = gtk::Label::new(Some(&l.name));
                name.set_xalign(0.0);
                name.set_hexpand(true);
                row.append(&name);
                if l.uid == current {
                    row.append(&gtk::Image::from_icon_name("object-select-symbolic"));
                }
                b.set_child(Some(&row));
                let (uid, p) = (l.uid.clone(), pop.clone());
                b.connect_clicked(move |_| {
                    p.popdown();
                    if let Some(ui) = ui() {
                        ui.borrow_mut().quick_target = Some(uid.clone());
                    }
                    update_dest();
                    if let Some(ui) = ui() {
                        let text = ui.borrow().quick.text();
                        update_parse(&text);
                    }
                });
                list.append(&b);
            }
        });
    }
    dest.set_popover(Some(&dest_pop));
    let quick_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    quick_row.add_css_class("quick-row");
    quick_row.append(&quick);
    quick_row.append(&dest);
    let parse = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    parse.add_css_class("quick-parse");
    parse.set_visible(false);

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rows.add_css_class("task-list");
    rows.set_vexpand(true);
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .hexpand(true)
        .child(&rows)
        .build();
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_column.set_hexpand(true);
    list_column.append(&page_bar);
    list_column.append(&quick_row);
    list_column.append(&parse);
    list_column.append(&list_scroll);

    let detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail.add_css_class("detail-panel");
    let detail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&detail)
        .build();
    let detail_foot = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    detail_foot.add_css_class("detail-foot");
    let detail_side = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_side.add_css_class("inspector");
    detail_side.add_css_class("detail-side");
    detail_side.set_size_request(detail::WIDTH, -1);
    detail_side.set_hexpand(false);
    detail_side.append(&detail_scroll);
    detail_side.append(&detail_foot);
    detail_side.set_visible(false);
    let line = nyx_ui::frame::pane_line(gtk::Orientation::Vertical);
    detail_side.bind_property("visible", &line, "visible").sync_create().build();
    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.set_vexpand(true);
    split.append(&list_column);
    split.append(&line);
    split.append(&detail_side);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("settings-content");
    content.set_hexpand(true);
    content.append(&banners);
    content.append(&split);

    // The header: the app, the sidebar's toggle, search in the middle, Settings.
    let header = nyx_ui::chrome::app_header(&window, "checkbox-checked-symbolic", "Reminders");
    let collapse = nyx_ui::widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.connect_clicked(|_| toggle_sidebar());
    header.start.append(&collapse);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search reminders  (Ctrl+F)"));
    search.update_property(&[gtk::accessible::Property::Label("Search reminders")]);
    search.add_css_class("bar-search");
    search.set_width_chars(34);
    search.connect_search_changed(|_| refresh_page());
    search.connect_stop_search(|entry| entry.set_text(""));
    header.center.append(&search);
    header.end.append(&nyx_ui::chrome::settings_button(settings_dialog::open));

    let frame = nyx_ui::frame::AppFrame::new(&header.root);
    // The page bar and the list reach the sheet's edges, as in the mockups.
    frame.set_flush(true);
    frame.set_sidebar(&nav);
    frame.set_page(&content);
    let overlay = frame.install(&window);
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
        page_sub,
        show_done,
        show_done_box,
        order,
        order_label,
        quick,
        dest_dot,
        dest_name,
        parse,
        quick_target: None,
        rows,
        detail,
        detail_foot,
        detail_side,
        page: Page::Smart(Smart::Today),
        lists: Vec::new(),
        tasks: Vec::new(),
        meta: HashMap::new(),
        sidecar,
        selected: None,
        shown_detail: None,
        row_order: Vec::new(),
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
        if widget.has_css_class("compact-hide") {
            widget.set_visible(!compact);
        }
        if widget.has_css_class("compact-show") {
            widget.set_visible(compact);
        }
        if widget.has_css_class("nav-item")
            && let Some(content) = widget.downcast_ref::<gtk::Button>().and_then(|b| b.child())
        {
            content.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
        }
        // In the icon rail a tile is a square with only its disc, centred.
        if widget.has_css_class("smart-tile") {
            widget.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
        }
        if widget.has_css_class("tile-icon") {
            widget.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Start });
            if let Some(row) = widget.parent() {
                row.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
            }
        }
        // The smart list tiles stack into the icon rail.
        if let Some(tiles) = widget.downcast_ref::<gtk::FlowBox>().filter(|f| f.has_css_class("smart-tiles")) {
            let per_line = if compact { 1 } else { 2 };
            tiles.set_min_children_per_line(per_line);
            tiles.set_max_children_per_line(per_line);
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
