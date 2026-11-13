//! Theming: the shared Nyx theme engine, given this app's stylesheet.

pub use nyx_theme::*;

use crate::prefs;

/// Icons the app brings (the flag and the inbox), written once to its cache
/// and added to the icon theme. GTK fills every path of a symbolic icon.
const ICONS: &[(&str, &str)] = &[
    (
        "nyx-reminders-flag-symbolic",
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"><path fill="#000" d="M5 2.7a.8.8 0 0 1 .8.8v.7h11.7a.8.8 0 0 1 .65 1.27L15.6 9.2l2.55 3.73a.8.8 0 0 1-.65 1.27H5.8v6.3a.8.8 0 0 1-1.6 0V3.5a.8.8 0 0 1 .8-.8zm.8 3.1v6.8h10.18l-2.0-2.94a.8.8 0 0 1 0-.92l2.0-2.94z"/></svg>"##,
    ),
    (
        "nyx-reminders-inbox-symbolic",
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24"><path fill="#000" d="M6.1 4.2h11.8c.5 0 .95.3 1.15.76l2.8 6.9c.04.1.05.2.05.3v5.6a2.6 2.6 0 0 1-2.6 2.6H4.7a2.6 2.6 0 0 1-2.6-2.6v-5.6c0-.1.01-.2.05-.3l2.8-6.9c.2-.46.65-.76 1.15-.76zm.37 1.6-2.4 5.9h3.9c.3 0 .58.17.72.44l1.27 2.56h4.08l1.27-2.56a.8.8 0 0 1 .72-.44h3.9l-2.4-5.9zM3.7 13.3v4.47c0 .55.45 1 1 1h14.6c.55 0 1-.45 1-1V13.3h-3.6l-1.27 2.56a.8.8 0 0 1-.72.44H9.3a.8.8 0 0 1-.72-.44L7.3 13.3z"/></svg>"##,
    ),
];

fn install_icons() {
    let dir = nyx_core::paths::cache_dir().join("icons");
    for (name, svg) in ICONS {
        let file = dir.join(format!("{name}.svg"));
        if std::fs::read_to_string(&file).ok().as_deref() != Some(svg) {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&file, svg);
        }
    }
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(&dir);
    }
}

pub fn install() {
    install_icons();
    nyx_theme::use_design_2();
    nyx_theme::install(Style {
        window_class: "reminders-window",
        base_skip: &[],
        css: include_str!("style.css"),
        no_glow_css: "",
        extra_colors: None,
        prefs: || {
            let p = prefs::get();
            ThemePrefs { mode: p.mode, theme: p.theme, glow: p.glow, reduce_motion: p.reduce_motion }
        },
        set_theme: Some(|mode, theme| {
            prefs::update(|p| {
                p.mode = mode;
                p.theme = theme;
            });
        }),
    });
}
