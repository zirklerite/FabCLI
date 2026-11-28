#!/usr/bin/env bash
# Tests for dist/install.sh. Run on Linux: `bash tests/install_sh_test.sh`
# (cargo test runs it there through tests/installer.rs). HOME points at
# a temp dir and `ldd` is stubbed, so nothing outside the temp dir is
# touched and the result doesn't depend on installed libraries.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAILED=0

fail() {
    echo "FAIL: $*"
    FAILED=1
}

# A fake unpacked release in $WORK/$1: install.sh, a `fabcli` that
# prints a version, and BUILD_INFO.txt (unless $2 is "nobuildinfo").
release() {
    local dir="$WORK/$1"
    mkdir -p "$dir"
    cp "$ROOT/dist/install.sh" "$dir/"
    chmod +x "$dir/install.sh"
    printf '#!/bin/sh\necho "fabcli 9.9.9"\n' >"$dir/fabcli"
    chmod +x "$dir/fabcli"
    [ "${2:-}" = nobuildinfo ] || printf 'version=9.9.9\nplatform=linux64\n' >"$dir/BUILD_INFO.txt"
    echo "$dir"
}

# Runs install.sh from release folder $1 with args; sets OUT and CODE.
run() {
    local dir="$1"
    shift
    OUT="$(HOME="$WORK/home" PATH="$WORK/stub:$PATH" "$dir/install.sh" "$@" 2>&1)"
    CODE=$?
}

expect_code() {
    [ "$CODE" = "$1" ] || fail "$2: exit $CODE, expected $1. Output: $OUT"
}

reset_home() {
    rm -rf "$WORK/home"
    mkdir -p "$WORK/home"
}

# ldd stub: reports nothing missing unless $WORK/stub/missing exists.
mkdir -p "$WORK/stub"
cat >"$WORK/stub/ldd" <<EOF
#!/bin/sh
[ -f "$WORK/stub/missing" ] && echo "	libgtk-3.so.0 => not found"
exit 0
EOF
chmod +x "$WORK/stub/ldd"

A="$(release a)"
B="$(release b)"
OTHER="$(release other nobuildinfo)"
LINK="$WORK/home/.local/bin/fabcli"

# Fresh install: creates ~/.local/bin and the link, prints next steps.
reset_home
run "$A"
expect_code 0 "fresh install"
[ "$(readlink -f "$LINK")" = "$A/fabcli" ] || fail "fresh install: link doesn't point at $A/fabcli"
case "$OUT" in *"fabcli auth login"*"fabcli skill install"*) ;; *) fail "fresh install: no next steps" ;; esac
case "$OUT" in *"Missing runtime libraries"*) fail "fresh install: reported missing libraries" ;; esac

# Re-run: already present, link unchanged.
run "$A"
expect_code 0 "re-run"
case "$OUT" in *"already installed"*) ;; *) fail "re-run: not reported as already installed" ;; esac

# Moved folder: a link to another FabCLI install is replaced, exit 3.
run "$B"
expect_code 3 "replace older install"
[ "$(readlink -f "$LINK")" = "$B/fabcli" ] || fail "replace: link doesn't point at $B/fabcli"

# A link to something that isn't a FabCLI install is refused.
ln -sfn "$OTHER/fabcli" "$LINK"
run "$A"
expect_code 1 "link to non-FabCLI target"
[ "$(readlink -f "$LINK")" = "$OTHER/fabcli" ] || fail "non-FabCLI link was changed"

# A dangling link is refused too (its target can't be checked).
ln -sfn "$WORK/gone/fabcli" "$LINK"
run "$A"
expect_code 1 "dangling link"

# A regular file is left alone.
rm -f "$LINK"
echo "not a link" >"$LINK"
run "$A"
expect_code 1 "regular file"
[ "$(cat "$LINK")" = "not a link" ] || fail "regular file was changed"

# Missing libraries: named, with the apt line, and the link still made.
reset_home
touch "$WORK/stub/missing"
run "$A"
rm -f "$WORK/stub/missing"
expect_code 0 "missing libraries"
case "$OUT" in *"libgtk-3.so.0"*"sudo apt install libgtk-3-0t64 libwebkit2gtk-4.1-0 libsoup-3.0-0"*) ;; *) fail "missing libraries: no names or apt line. Output: $OUT" ;; esac
[ -L "$LINK" ] || fail "missing libraries: link not created"

# Status: the version comes from running the binary.
run "$A" status
expect_code 0 "status"
case "$OUT" in *"fabcli 9.9.9"*"this folder's fabcli"*) ;; *) fail "status: no version or ownership. Output: $OUT" ;; esac

# Uninstall: removes a FabCLI link, refuses anything else.
run "$A" uninstall
expect_code 0 "uninstall"
[ -e "$LINK" ] && fail "uninstall: link still there"
run "$A" uninstall
expect_code 0 "uninstall with nothing to remove"
ln -sfn "$OTHER/fabcli" "$LINK"
run "$A" uninstall
expect_code 1 "uninstall of a non-FabCLI link"
[ -L "$LINK" ] || fail "uninstall removed a non-FabCLI link"

# Unknown argument.
run "$A" bogus
expect_code 1 "unknown argument"

if [ "$FAILED" = 0 ]; then
    echo "install.sh: all tests passed"
fi
exit "$FAILED"
