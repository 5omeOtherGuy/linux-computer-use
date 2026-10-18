#!/bin/bash
# Install the agent-desktop owner view: packages, command links and an
# xfce4-panel Generic Monitor item that lists live agent workspaces.
#
#   panel/install-panel.sh              # dry run: print what would change
#   panel/install-panel.sh --apply      # install and restart the panel
#   panel/install-panel.sh --uninstall  # remove the panel item (links stay)
#
# Options: --panel N (default 1), --bin-dir DIR (default ~/.local/bin),
#          --no-packages, --no-restart,
#          --replace-links (point existing command links elsewhere at this checkout).
# Existing files in the bin dir are never replaced; existing links to another
# checkout only with --replace-links. Both are checked before any change.
# A backup of the panel channel is written before any panel change.
set -euo pipefail

REPO="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
MODE=dry
PANEL=1
BIN_DIR="$HOME/.local/bin"
PACKAGES=1
RESTART=1
REPLACE_LINKS=0
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}"
STATE="$CONFIG/agent-desktop"
PKGS=(tigervnc-standalone-server tigervnc-viewer openbox xdotool wmctrl xauth dbus-daemon
      python3-pil zenity xfce4-genmon-plugin)

while [ $# -gt 0 ]; do
    case "$1" in
        --apply) MODE=apply ;;
        --uninstall) MODE=uninstall ;;
        --panel) PANEL="$2"; shift ;;
        --bin-dir) BIN_DIR="$2"; shift ;;
        --no-packages) PACKAGES=0 ;;
        --no-restart) RESTART=0 ;;
        --replace-links) REPLACE_LINKS=1 ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

say() { printf '%s\n' "$*"; }
do_() { if [ "$MODE" = dry ]; then say "would run: $*"; else "$@"; fi; }
q() { xfconf-query -c xfce4-panel "$@"; }

plugin_ids() { q -p "/panels/panel-$PANEL/plugin-ids" 2>/dev/null | tail -n +3; }

set_plugin_ids() {
    local args=()
    for i in "$@"; do args+=(-t int -s "$i"); done
    do_ q -p "/panels/panel-$PANEL/plugin-ids" -n -a "${args[@]}"
}

backup_panel() {
    local b
    b="$STATE/panel-backup-$(date +%Y%m%d-%H%M%S).txt"
    say "backup of the panel channel: $b"
    if [ "$MODE" != dry ]; then
        mkdir -p "$STATE"
        q -lv > "$b"
    fi
}

restart_panel() {
    if [ "$RESTART" = 1 ]; then do_ xfce4-panel -r; else say "restart the panel yourself: xfce4-panel -r"; fi
}

if [ "$MODE" = uninstall ]; then
    [ -f "$STATE/panel-plugin-id" ] || { say "no panel item recorded in $STATE/panel-plugin-id"; exit 0; }
    ID=$(cat "$STATE/panel-plugin-id")
    [ "$(q -p "/plugins/plugin-$ID" 2>/dev/null)" = genmon ] || { say "plugin-$ID is not genmon; not touching it"; exit 1; }
    backup_panel
    mapfile -t ids < <(plugin_ids)
    keep=()
    for i in "${ids[@]}"; do [ "$i" = "$ID" ] || keep+=("$i"); done
    set_plugin_ids "${keep[@]}"
    do_ q -p "/plugins/plugin-$ID" -r -R
    restart_panel
    # the exiting panel re-saves the plugin's rc file, so remove it afterwards
    [ "$MODE" = dry ] || sleep 3
    do_ rm -f -- "$CONFIG/xfce4/panel/genmon-$ID.rc" "$STATE/panel-plugin-id"
    say "removed panel item plugin-$ID"
    exit 0
fi

# 0. command links: check every destination before changing anything
LINKS=(bin/agent-desktop panel/agent-desktop-menu panel/agent-desktop-genmon)
for f in "${LINKS[@]}"; do
    dest="$BIN_DIR/$(basename "$f")"
    if [ -L "$dest" ]; then
        if [ "$(readlink -f -- "$dest")" != "$(readlink -f -- "$REPO/$f")" ] && [ "$REPLACE_LINKS" != 1 ]; then
            say "existing link $dest -> $(readlink -- "$dest"); use --replace-links to point it here"
            exit 1
        fi
    elif [ -e "$dest" ]; then
        say "refusing to replace $dest: it is not a symlink"
        exit 1
    fi
done

# 1. packages
if [ "$PACKAGES" = 1 ]; then
    missing=()
    for p in "${PKGS[@]}"; do
        [ "$(dpkg-query -Wf '${db:Status-Abbrev}' "$p" 2>/dev/null)" = "ii " ] || missing+=("$p")
    done
    if [ ${#missing[@]} -gt 0 ]; then
        say "missing packages: ${missing[*]}"
        if apt-get -s install -y "${missing[@]}" | grep -E '^(Remv|Purg)'; then
            say "installing would remove packages; stopping"; exit 1
        fi
        do_ sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}"
    else
        say "packages: all present"
    fi
fi

# 2. command links
do_ mkdir -p "$BIN_DIR"
for f in "${LINKS[@]}"; do
    dest="$BIN_DIR/$(basename "$f")"
    if [ -L "$dest" ]; then
        [ "$(readlink -f -- "$dest")" = "$(readlink -f -- "$REPO/$f")" ] && continue
        do_ rm -- "$dest"
    fi
    do_ ln -sn -- "$REPO/$f" "$dest"
done

# 3. panel item
if [ -f "$STATE/panel-plugin-id" ] && [ "$(q -p "/plugins/plugin-$(cat "$STATE/panel-plugin-id")" 2>/dev/null)" = genmon ]; then
    say "panel item already installed as plugin-$(cat "$STATE/panel-plugin-id")"
    exit 0
fi
mapfile -t ids < <(plugin_ids)
[ ${#ids[@]} -gt 0 ] || { say "panel-$PANEL has no plugin-ids; use --panel N"; exit 1; }
ID=$(( $(q -p /plugins -l | grep -oE '/plugin-[0-9]+$' | grep -oE '[0-9]+' | sort -n | tail -n 1) + 1 ))
# place it just left of the system tray, else at the end
new=()
placed=0
for i in "${ids[@]}"; do
    if [ $placed = 0 ] && [ "$(q -p "/plugins/plugin-$i" 2>/dev/null)" = systray ]; then new+=("$ID"); placed=1; fi
    new+=("$i")
done
[ $placed = 1 ] || new+=("$ID")
backup_panel
say "adding genmon as plugin-$ID to panel-$PANEL: ${ids[*]} -> ${new[*]}"
do_ q -p "/plugins/plugin-$ID" -n -t string -s genmon
RC="$CONFIG/xfce4/panel/genmon-$ID.rc"
say "writing $RC"
if [ "$MODE" != dry ]; then
    mkdir -p "$(dirname "$RC")" "$STATE"
    cat > "$RC" <<EOF
Command=$BIN_DIR/agent-desktop-genmon
UseLabel=0
Text=
UpdatePeriod=3000
Font=Sans 10
EOF
    echo "$ID" > "$STATE/panel-plugin-id"
fi
set_plugin_ids "${new[@]}"
restart_panel
say "done. The item is empty while no agent workspace runs; it shows '🖥 N' otherwise."
