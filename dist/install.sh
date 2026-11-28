#!/usr/bin/env bash
# Puts the fabcli binary next to this script on your PATH by linking
# ~/.local/bin/fabcli to it. No sudo, no shell-profile edits.
#
#   ./install.sh             link (or re-link) ~/.local/bin/fabcli
#   ./install.sh status      which fabcli a new shell runs, and its version
#   ./install.sh uninstall   remove the link (only if it points at a FabCLI install)
#
# A link counts as pointing at a FabCLI install when its target is a
# file named `fabcli` whose folder holds a BUILD_INFO.txt with a
# `version=` line. That is decided without running the target.
#
# Exit codes: 0 installed or already present (status/uninstall: done),
# 1 failed or refused, 3 replaced an older FabCLI install.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BIN="$HERE/fabcli"
BIN_DIR="$HOME/.local/bin"
LINK="$BIN_DIR/fabcli"
APT_LINE="sudo apt install libgtk-3-0t64 libwebkit2gtk-4.1-0 libsoup-3.0-0"

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
}

# True when $1 (a path) is a FabCLI install's binary.
is_install() {
    [ "$(basename "$1")" = fabcli ] && [ -f "$1" ] &&
        grep -q '^version=' "$(dirname "$1")/BUILD_INFO.txt" 2>/dev/null
}

# The link's target, resolved; empty when it doesn't resolve.
link_target() {
    readlink -f "$LINK" 2>/dev/null || true
}

# True when the link points at a FabCLI install.
link_is_install() {
    is_install "$(link_target)"
}

check_libraries() {
    command -v ldd >/dev/null 2>&1 || return 0
    local missing
    missing="$(ldd "$BIN" 2>/dev/null | grep 'not found' | awk '{print $1}' | sort -u)"
    [ -z "$missing" ] && return 0
    echo
    echo "Missing runtime libraries:"
    echo "$missing" | sed 's/^/  /'
    echo "Install them (Ubuntu 24.04) with:"
    echo "  $APT_LINE"
}

path_hint() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) return 0 ;;
    esac
    echo
    echo "$BIN_DIR is not on this shell's PATH yet. On Ubuntu it is added at"
    echo "login once the folder exists: open a new login shell (or log out and in)."
    echo "Elsewhere, add this line to your shell profile:"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
}

next_steps() {
    echo
    echo "Next steps, in a new shell:"
    echo "  fabcli auth login        sign in to Epic, about once a year"
    echo "  fabcli skill install     for Claude Code: install the FabCLI skill"
    echo "  fabcli guide             a short guide for AI agents and people"
    echo "Later updates: fabcli update (no need to run install.sh again)."
}

install() {
    local result=0
    if [ -L "$LINK" ]; then
        local target
        target="$(link_target)"
        if [ "$target" = "$BIN" ]; then
            echo "fabcli is already installed: $LINK -> $BIN"
            check_libraries
            path_hint
            return 0
        fi
        if ! link_is_install; then
            echo "ERROR: $LINK is a link to something that isn't a FabCLI install:"
            echo "  $(readlink "$LINK")"
            echo "Nothing changed. Remove it yourself if it's stale (rm \"$LINK\"), then re-run."
            return 1
        fi
        ln -sfn "$BIN" "$LINK" || return 1
        echo "Replaced the older FabCLI install ($target)."
        result=3
    elif [ -e "$LINK" ]; then
        echo "ERROR: $LINK exists and is not a link. Nothing changed."
        echo "Move it away yourself, then re-run."
        return 1
    else
        mkdir -p "$BIN_DIR" || return 1
        ln -s "$BIN" "$LINK" || return 1
    fi
    echo "Linked $LINK -> $BIN"
    check_libraries
    path_hint
    next_steps
    return "$result"
}

status() {
    if [ -L "$LINK" ]; then
        echo "Link: $LINK -> $(readlink "$LINK")"
    elif [ -e "$LINK" ]; then
        echo "Link: $LINK is a regular file, not a link"
    else
        echo "Link: none ($LINK)"
    fi
    # What a new shell runs: ~/.local/bin is on PATH there once it exists.
    local found
    if [ -x "$LINK" ]; then
        found="$LINK"
    else
        found="$(command -v fabcli 2>/dev/null || true)"
    fi
    if [ -z "$found" ]; then
        echo "A new shell finds no fabcli."
        return 0
    fi
    local resolved version
    resolved="$(readlink -f "$found" 2>/dev/null || echo "$found")"
    version="$("$found" --version 2>/dev/null || echo "unknown (--version failed)")"
    echo "A new shell runs: $resolved"
    echo "Version: $version"
    if [ "$resolved" = "$BIN" ]; then
        echo "That is this folder's fabcli."
    else
        echo "That is not this folder's fabcli ($BIN)."
    fi
}

uninstall() {
    if [ ! -L "$LINK" ]; then
        if [ -e "$LINK" ]; then
            echo "ERROR: $LINK is not a link; not removing it."
            return 1
        fi
        echo "Nothing to remove: $LINK doesn't exist."
        return 0
    fi
    if ! link_is_install; then
        echo "ERROR: $LINK doesn't point at a FabCLI install; not removing it."
        return 1
    fi
    rm -f "$LINK" || return 1
    echo "Removed $LINK."
    echo "No files, tokens or keystore entries were touched; fabcli auth logout removes those."
}

if [ ! -f "$BIN" ]; then
    echo "ERROR: fabcli not found next to install.sh ($HERE)."
    echo "Unpack the whole release, then run ./install.sh from that folder."
    exit 1
fi

case "${1:-}" in
    "") install ;;
    status) status ;;
    uninstall) uninstall ;;
    -h | --help) usage ;;
    *)
        echo "Unknown argument: $1"
        usage
        exit 1
        ;;
esac
