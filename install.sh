#!/bin/sh
# Builds discord-gtk and installs it for the current user, so it shows up in
# the app launcher. Remove it again with ./install.sh --uninstall.
set -e
cd "$(dirname "$0")"
prefix="${PREFIX:-$HOME/.local}"

if [ "$1" = "--uninstall" ]; then
    rm -f "$prefix/bin/discord-gtk" \
          "$prefix/share/applications/dev.yugi.DiscordGtk.desktop" \
          "$prefix/share/icons/hicolor/scalable/apps/dev.yugi.DiscordGtk.svg"
    echo "Removed. Your settings and token stay in ~/.config/discord-gtk."
    exit 0
fi

command -v cargo >/dev/null || { echo "Rust is missing. Install it from https://rustup.rs and run this again."; exit 1; }
missing=""
for lib in gtk4 libadwaita-1 webkitgtk-6.0 opus; do
    pkg-config --exists "$lib" 2>/dev/null || missing="$missing $lib"
done
if [ -n "$missing" ]; then
    echo "Missing development libraries:$missing"
    echo "See the Building section of the README for the package names on your distro."
    exit 1
fi

cargo build --release
install -Dm755 target/release/discord-gtk "$prefix/bin/discord-gtk"
# Launchers do not always have ~/.local/bin on their PATH, so use the full path.
install -d "$prefix/share/applications"
sed "s|^Exec=.*|Exec=$prefix/bin/discord-gtk|" packaging/dev.yugi.DiscordGtk.desktop > "$prefix/share/applications/dev.yugi.DiscordGtk.desktop"
install -Dm644 packaging/dev.yugi.DiscordGtk.svg "$prefix/share/icons/hicolor/scalable/apps/dev.yugi.DiscordGtk.svg"
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" || true
echo "Installed to $prefix. Find discord-gtk in your app launcher."
