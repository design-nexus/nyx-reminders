> Development branch: Online Accounts now uses the isolated native preview.
> Runtime EDS/EWS dependencies have been replaced by `nyx-pim` and libical.
> Existing EDS data is retained and can be explicitly exported/imported with
> `nyx-pim-migrate`. Provider and migration limitations are tracked in
> the sibling `nyx-accounts/docs/IMPLEMENTATION.md`; this branch is not merge-ready.

# Reminders

Your task lists from Google Tasks, Microsoft To Do, Nextcloud, compatible CalDAV servers and this
computer, in one place, for Omarchy. Part of the [Nyx](https://github.com/design-nexus/nyx) apps.

- Today, Scheduled, Flagged, All and Completed across every list
- Type “Call Sam tomorrow at 9” to add a reminder with its due date
- Due dates and times, alerts, repeats, priority, notes, links, subtasks and tags (`#bills` when adding)
- Drag reminders into your own order
- Alerts as desktop notifications, with Complete and Snooze
- A repeating reminder moves to its next date when you complete it
- Works offline from the saved copy and catches up when you're back online
- Follows your Omarchy theme; works from the keyboard and tiled at half-screen

Accounts are added in **Settings → Online Accounts**. This feature branch uses
native authentication and a separate SQLite task store. Provider capabilities vary:
Google Tasks has date-only deadlines without reminders or recurrence; personal
Microsoft accounts use Graph To Do. Existing EDS data remains untouched.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nyx-reminders/main/install.sh | bash
```

## Keys

| Key | Action |
| --- | --- |
| N | New reminder |
| J / K | Next / previous |
| Space | Complete |
| S | Flag |
| 1–4 | Today, Scheduled, Flagged, All |
| Ctrl+F | Find |
| F1 | All shortcuts |
