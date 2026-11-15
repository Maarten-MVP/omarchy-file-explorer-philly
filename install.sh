#!/usr/bin/env bash
# Install philly and make it the default file manager + file picker.
#   ./install.sh            build, install, take over
#   ./install.sh --no-takeover   build and install only
#   ./install.sh --revert   give Nautilus its roles back (philly stays installed)
set -euo pipefail
cd "$(dirname "$0")"

BIN="$HOME/.local/bin/philly"
APPS="$HOME/.local/share/applications"
DBUS="$HOME/.local/share/dbus-1/services"
PORTAL_CONF="$HOME/.config/xdg-desktop-portal/hyprland-portals.conf"
HYPR="$HOME/.config/hypr/philly.lua"

revert() {
  echo "Giving Nautilus its roles back…"
  rm -f "$DBUS/org.freedesktop.FileManager1.service" "$HYPR"
  [[ -f "$PORTAL_CONF" ]] && sed -i '/^org\.freedesktop\.impl\.portal\.FileChooser=philly$/d' "$PORTAL_CONF"
  xdg-mime default org.gnome.Nautilus.desktop inode/directory
  sed -i '/hypr\.philly/d; /^-- philly file explorer$/d' "$HOME/.config/hypr/hyprland.lua" 2>/dev/null || true
  pkill -f "philly --service" || true
  systemctl --user restart xdg-desktop-portal.service || true
  hyprctl reload >/dev/null 2>&1 || true
  echo "Done. philly is still installed at $BIN."
}

if [[ "${1:-}" == "--revert" ]]; then revert; exit 0; fi

echo "→ Building (release)…"
cargo build --release
install -Dm755 target/release/philly "$BIN"
install -Dm644 data/dev.philly.Philly.desktop "$APPS/dev.philly.Philly.desktop"
update-desktop-database "$APPS" >/dev/null 2>&1 || true

mkdir -p "$DBUS"
cat > "$DBUS/org.freedesktop.impl.portal.desktop.philly.service" <<SVC
[D-BUS Service]
Name=org.freedesktop.impl.portal.desktop.philly
Exec=$BIN --service
SVC

if [[ "${1:-}" == "--no-takeover" ]]; then
  echo "Installed. Run: philly"
  exit 0
fi

echo "→ Taking over file manager roles…"
# "Show in folder" from browsers and apps.
cat > "$DBUS/org.freedesktop.FileManager1.service" <<SVC
[D-BUS Service]
Name=org.freedesktop.FileManager1
Exec=$BIN --service
SVC
# Default app for folders.
xdg-mime default dev.philly.Philly.desktop inode/directory

# Portal file chooser (the upload / save dialog in other apps).
PORTAL_FILE=/usr/share/xdg-desktop-portal/portals/philly.portal
if [[ ! -f $PORTAL_FILE ]]; then
  if sudo -n true 2>/dev/null || [[ -t 0 ]]; then
    echo "  The portal registration needs sudo once ($PORTAL_FILE):"
    sudo install -Dm644 data/philly.portal "$PORTAL_FILE"
  else
    echo "  SKIPPED: file picker registration needs sudo. Run in a terminal:"
    echo "    sudo install -Dm644 $PWD/data/philly.portal $PORTAL_FILE && systemctl --user restart xdg-desktop-portal"
  fi
fi
# Add only our FileChooser line; keep whatever else is configured there.
mkdir -p "$(dirname "$PORTAL_CONF")"
if [[ ! -f $PORTAL_CONF ]]; then
  printf '[preferred]\ndefault=hyprland;gtk\n' > "$PORTAL_CONF"
fi
if [[ -f $PORTAL_FILE ]] && ! grep -q '^org\.freedesktop\.impl\.portal\.FileChooser=' "$PORTAL_CONF"; then
  sed -i '/^\[preferred\]/a org.freedesktop.impl.portal.FileChooser=philly' "$PORTAL_CONF"
fi

# Hyprland: keybinding, autostart of the background service, picker window rules.
cat > "$HYPR" <<LUA
-- philly file explorer (written by ~/Projects/philly/install.sh; remove with --revert)
-- Replaces Omarchy's defaults: SUPER+SHIFT+F (was: nautilus), SUPER+ALT+SHIFT+F (was: nautilus-cwd).
o.rebind("SUPER + SHIFT + F", "File manager", { launch = "$BIN --new-window" })
o.rebind("SUPER + ALT + SHIFT + F", "File manager (cwd)", { launch = '$BIN "\$(omarchy-cmd-terminal-cwd)"' })
o.launch_on_start("$BIN --service")
-- File picker windows (Open / Save dialogs for other apps) float, centered.
o.window("^dev\\\\.philly\\\\.Picker\$", { float = true, center = true, size = { 1180, 720 } })
LUA
if ! grep -q 'hypr.philly' "$HOME/.config/hypr/hyprland.lua"; then
  printf '\n-- philly file explorer\nrequire("hypr.philly")\n' >> "$HOME/.config/hypr/hyprland.lua"
fi

pkill -f "philly --service" || true
setsid -f "$BIN" --service >/dev/null 2>&1 || true
systemctl --user restart xdg-desktop-portal.service || true
hyprctl reload >/dev/null 2>&1 || true
echo "Done. Super+Shift+F opens philly. Revert any time with: ./install.sh --revert"
