//! Desktop notifications (org.freedesktop.Notifications) for due reminders,
//! with Complete, Snooze and Dismiss.

use chrono::{DateTime, Local};
use gio::prelude::*;
use nyx_eds::Reminder;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const ICON: &str = "io.github.design_nexus.nyx.Reminders";
/// NotificationClosed reason: dismissed by the user.
const CLOSED_BY_USER: u32 = 2;

pub struct Notifier {
    conn: Option<gio::DBusConnection>,
    shown: Rc<RefCell<HashMap<u32, Reminder>>>,
    _subscriptions: Vec<gio::SignalSubscription>,
}

impl Notifier {
    pub fn new() -> Self {
        let conn = notification_bus();
        let shown: Rc<RefCell<HashMap<u32, Reminder>>> = Rc::default();
        let mut subscriptions = Vec::new();
        if let Some(conn) = &conn {
            let s = shown.clone();
            subscriptions.push(conn.subscribe_to_signal(
                Some(NOTIFICATIONS),
                Some(NOTIFICATIONS),
                Some("ActionInvoked"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some((id, action)) = signal.parameters.get::<(u32, String)>() else { return };
                    let Some(reminder) = s.borrow_mut().remove(&id) else { return };
                    match action.as_str() {
                        "snooze" => crate::with_reminders(|r| r.snooze(&reminder, crate::snooze_until())),
                        "complete" => {
                            crate::complete(&reminder.source, &reminder.item.uid);
                            crate::with_reminders(|r| {
                                let _ = r.dismiss(&reminder);
                            });
                        }
                        "default" | "open" => {
                            let _ = std::process::Command::new("nyx-reminders").args(["--section", "today"]).spawn();
                        }
                        _ => crate::with_reminders(|r| {
                            let _ = r.dismiss(&reminder);
                        }),
                    }
                },
            ));
            let s = shown.clone();
            subscriptions.push(conn.subscribe_to_signal(
                Some(NOTIFICATIONS),
                Some(NOTIFICATIONS),
                Some("NotificationClosed"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some((id, reason)) = signal.parameters.get::<(u32, u32)>() else { return };
                    // Closed by the user counts as dismissed; expired or replaced keeps it pending.
                    if let Some(reminder) = s.borrow_mut().remove(&id)
                        && reason == CLOSED_BY_USER
                    {
                        crate::with_reminders(|r| {
                            let _ = r.dismiss(&reminder);
                        });
                    }
                },
            ));
        }
        Self { conn, shown, _subscriptions: subscriptions }
    }

    pub fn show(&self, reminder: Reminder) {
        let Some(conn) = &self.conn else { return };
        let summary =
            if reminder.item.summary.trim().is_empty() { "Untitled".to_string() } else { reminder.item.summary.clone() };
        let body = body(&reminder, Local::now().timestamp());
        let actions: Vec<String> = ["default", "Open", "complete", "Complete", "snooze", &format!("Snooze {} min", crate::snooze_minutes()), "dismiss", "Dismiss"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut hints: HashMap<String, glib::Variant> = HashMap::new();
        hints.insert("desktop-entry".into(), ICON.to_variant());
        hints.insert("urgency".into(), 1u8.to_variant());

        let params = ("Reminders", 0u32, ICON, summary.as_str(), body.as_str(), actions, hints, 0i32).to_variant();
        let shown = self.shown.clone();
        conn.call(
            Some(NOTIFICATIONS),
            PATH,
            NOTIFICATIONS,
            "Notify",
            Some(&params),
            Some(glib::VariantTy::new("(u)").expect("type")),
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
            move |result| match result.ok().and_then(|v| v.get::<(u32,)>()) {
                Some((id,)) => {
                    shown.borrow_mut().insert(id, reminder);
                }
                None => eprintln!("nyx-calendard: couldn't show a notification"),
            },
        );
    }
}

/// "Due now", "Due at 2:00 PM", "Due today", "Overdue since Oct 20", then the
/// first line of the notes.
fn body(reminder: &Reminder, now: i64) -> String {
    let mut parts = Vec::new();
    let today = Local::now().date_naive();
    match reminder.item.due {
        Some(nyx_eds::When::Time(due)) => {
            let at = DateTime::from_timestamp(due, 0).map(|t| t.with_timezone(&Local));
            let clock = at
                .map(|t| if twelve_hour() { t.format("%-I:%M %p").to_string() } else { t.format("%H:%M").to_string() })
                .unwrap_or_default();
            parts.push(if (due - now).abs() < 120 {
                "Due now".to_string()
            } else if due < now {
                format!("Was due at {clock}")
            } else {
                format!("Due at {clock}")
            });
        }
        Some(nyx_eds::When::Date { year, month, day }) => {
            let date = chrono::NaiveDate::from_ymd_opt(year, month, day);
            parts.push(match date {
                Some(d) if d == today => "Due today".to_string(),
                Some(d) if d < today => format!("Overdue since {}", d.format("%b %-d")),
                Some(d) => format!("Due {}", d.format("%a, %b %-d")),
                None => String::new(),
            });
        }
        None => {}
    }
    if let Some(first) = reminder.item.description.lines().map(str::trim).find(|l| !l.is_empty()) {
        parts.push(first.chars().take(120).collect());
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// A 12-hour clock where the locale uses one (as the Calendar window does).
fn twelve_hour() -> bool {
    let locale =
        ["LC_ALL", "LC_TIME", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default();
    let region = locale.split(['.', '@']).next().unwrap_or("").rsplit('_').next().unwrap_or("").to_string();
    ["US", "CA", "AU", "NZ", "IN", "PH", "PK", "EG", "SA"].contains(&region.as_str())
}

/// Evolution's own reminder window is running (it would show the same alerts).
pub fn other_notifier_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else { return false };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim().starts_with("evolution-alarm"))
    })
}

// Preview engine traffic stays private; desktop notifications use the original bus.
fn notification_bus() -> Option<gio::DBusConnection> {
    match std::env::var("NYX_ACCOUNTS_SECRET_BUS_ADDRESS") {
        Ok(address) if address.starts_with("unix:") => gio::DBusConnection::for_address_sync(
            &address, gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None::<&gio::DBusAuthObserver>, gio::Cancellable::NONE,
        ).ok(),
        Ok(_) => None,
        Err(_) => gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok(),
    }
}
