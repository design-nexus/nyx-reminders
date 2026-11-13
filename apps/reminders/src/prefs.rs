//! This app's own preferences (`~/.config/nyx-reminders/settings.toml`).

pub use nyx_theme::ThemeMode;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub mode: ThemeMode,
    pub theme: String,
    pub reduce_motion: bool,
    pub snooze_minutes: i64,
    pub sidebar_collapsed: bool,
    pub glow: bool,
    /// The page shown last: a smart list id or `list:<uid>`.
    pub last_section: String,
    /// The list new reminders go to from smart lists.
    pub default_list: String,
    /// Show completed tasks under a list's open ones.
    pub show_completed: bool,
    /// The time "tomorrow" or "on Friday" means when no time is given.
    pub default_hour: u32,
    /// Minutes before a timed reminder's due time its alert goes off; none when empty.
    pub timed_alert: Option<i64>,
    /// Reminders due on a day (no time) get an alert at `default_hour`.
    pub day_alerts: bool,
    /// Accounts whose lists are folded away in the sidebar (by heading).
    pub collapsed_groups: Vec<String>,
    /// Today and Scheduled's order within each day: "date" or "priority".
    pub sort_fixed: String,
    /// A list's order: "manual" (as dragged), "date" or "priority".
    pub sort_lists: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            reduce_motion: false,
            snooze_minutes: 10,
            sidebar_collapsed: false,
            glow: true,
            last_section: "today".into(),
            default_list: String::new(),
            show_completed: false,
            default_hour: 9,
            timed_alert: Some(0),
            day_alerts: true,
            collapsed_groups: Vec::new(),
            sort_fixed: "date".into(),
            sort_lists: "manual".into(),
        }
    }
}

/// Times as "2 PM" where the locale uses a 12-hour clock, else "14:00".
pub fn twelve_hour() -> bool {
    let locale =
        ["LC_ALL", "LC_TIME", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default();
    let region = locale.split(['.', '@']).next().unwrap_or("").rsplit('_').next().unwrap_or("").to_string();
    ["US", "CA", "AU", "NZ", "IN", "PH", "PK", "EG", "SA"].contains(&region.as_str())
}

nyx_core::prefs_store!(Prefs);
