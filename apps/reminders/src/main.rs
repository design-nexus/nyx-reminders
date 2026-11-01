//! Reminders — your task lists from every account, for Omarchy.

mod actions;
mod control;
mod detail;
mod menus;
mod prefs;
mod settings_dialog;
mod settings_page;
mod theme;
mod widgets;
mod window;
mod worker;

use gtk::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.design_nexus.nyx.Reminders";

const USAGE: &str = "Usage: nyx-reminders [OPTIONS]\n\
\n\
  --section ID    open today, scheduled, flagged, all, completed, list:UID or settings\n\
  --add TEXT      add a reminder from words (\"Call Sam tomorrow at 9\")\n\
  --toggle        close the window if it's open, otherwise open it\n\
  --describe      print the control tools (JSON) and exit\n";

/// Starts the alerts service if it's installed and not running (the installer
/// enables it for login without starting it).
fn start_alerts() {
    if std::env::var_os("NYX_ACCOUNTS_PREVIEW").is_some() {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe.with_file_name("nyx-remindersd"))
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
        }
        return;
    }
    if std::env::var_os("REMINDERS_SNAPSHOT").is_some() {
        return;
    }
    let unit = nyx_core::paths::config_home().join("systemd/user/nyx-remindersd.service");
    if unit.exists() {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", "--no-block", "nyx-remindersd.service"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Adds once the lists have loaded (a fresh start takes a moment); gives up
/// after 15 s.
fn add_when_ready(text: String, tries: u32) {
    if window::ready() {
        window::add_text(&text);
    } else if tries < 60 {
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || add_when_ready(text, tries + 1));
    } else {
        window::toast("The task lists didn't load, so the reminder wasn't added.");
    }
}

fn main() -> glib::ExitCode {
    nyx_core::app::init(nyx_core::app::App { slug: "nyx-reminders", name: "Reminders" });
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    // The control tools as JSON, without a window (Nyx Assistant's abilities).
    if args.iter().any(|a| a == "--describe") {
        println!("{}", control::tools());
        return glib::ExitCode::SUCCESS;
    }
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("GSK_RENDERER", "ngl") };
    }
    let app = gtk::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_startup(|app| {
        theme::install();
        nyx_core::control::serve_async(app, control::tools(), control::handle);
        start_alerts();
    });
    app.connect_command_line(|app, cl| {
        let argv: Vec<String> = cl.arguments().iter().map(|a| a.to_string_lossy().to_string()).collect();
        let has = |flag: &str| argv.iter().any(|a| a == flag);
        let value = |flag: &str| argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).cloned();
        if has("--toggle")
            && let Some(w) = window::window()
            && w.is_visible()
        {
            w.close();
            return glib::ExitCode::SUCCESS;
        }
        window::present(app, value("--section").as_deref());
        if let Some(text) = value("--add").filter(|t| !t.trim().is_empty()) {
            add_when_ready(text, 0);
        }
        glib::ExitCode::SUCCESS
    });
    app.connect_shutdown(|_| prefs::flush());
    app.run()
}
