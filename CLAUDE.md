# philly

A keyboard-first file explorer with a git panel for Omarchy: Rust, GTK4, libadwaita. It is also the desktop's file picker (portal backend) and default file manager.

## After a change: run `./reload.sh`

All philly windows and the file picker live in one process started from `~/.local/bin/philly`, so a change only shows after that binary is replaced and the process restarted.

- After a successful change (it builds, `cargo test` passes, and you checked it works), run `./reload.sh` without asking. It builds the release binary, installs it, closes every open philly window, and restarts the background service, so Maarten can open philly and check.
- When unsure, ask Maarten before running it. Unsure means: the change is unfinished or unverified, or closing the windows could cost something (a copy or move still running, or an Open/Save dialog open for another app: window class `dev.philly.Picker`).
- Say in the final response that philly was reloaded and its windows were closed, or that it was not and why.
- `./install.sh` (without `--no-takeover`) is the first-time setup; it needs sudo and restarts the portal. Don't use it to refresh.

## Trying a build without touching the running philly

`PHILLY_NO_DBUS=1 target/debug/philly <path>` runs a separate instance (app id `dev.philly.PhillyDev`, no portal registration). Add `PHILLY_DEBUG=1` to log key events, and set `XDG_STATE_HOME` to a scratch directory to keep its saved state apart.
