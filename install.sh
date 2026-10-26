#!/bin/sh
# ocinfinity installer — Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/WDRXN/ocinfinity/main/install.sh | sh
#
# Environment overrides:
#   OCNINFINITY_VERSION      release tag to install (default: the latest release)
#   OCNINFINITY_INSTALL_DIR  where to put the binary (default: /usr/local/bin when
#                            writable, otherwise ~/.local/bin)
#   GITHUB_TOKEN             only needed if the repository is private
#
# POSIX sh only: no bashisms, no prompts, safe to pipe into `sh`.

set -eu

REPO="WDRXN/ocinfinity"
BIN="ocinfinity"
SELF="$(basename "$0" 2>/dev/null || echo install.sh)"

log() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but was not found"
}

# fetch URL DEST — download with curl or wget, adding auth only when a token is
# present. A private repo needs the token; a public one ignores it.
fetch() {
    _url="$1"
    _dest="$2"
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$_url" -o "$_dest"
        else
            curl -fsSL "$_url" -o "$_dest"
        fi
    else
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget -q --header="Authorization: Bearer ${GITHUB_TOKEN}" "$_url" -O "$_dest"
        else
            wget -q "$_url" -O "$_dest"
        fi
    fi
}

# fetch_text URL — same as above but to stdout.
fetch_text() {
    if command -v curl >/dev/null 2>&1; then
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl -fsSL -H "Authorization: Bearer ${GITHUB_TOKEN}" "$1"
        else
            curl -fsSL "$1"
        fi
    else
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            wget -q --header="Authorization: Bearer ${GITHUB_TOKEN}" "$1" -O -
        else
            wget -q "$1" -O -
        fi
    fi
}

# ---------------------------------------------------------------- uninstall ---

uninstall() {
    _removed=0
    for _candidate in \
        "${OCNINFINITY_INSTALL_DIR:-}" \
        "$HOME/.local/bin" \
        "/usr/local/bin" \
        "$HOME/bin"
    do
        [ -n "$_candidate" ] || continue
        if [ -f "$_candidate/$BIN" ]; then
            rm -f "$_candidate/$BIN" && log "removed $_candidate/$BIN"
            _removed=1
        fi
    done
    if [ "$_removed" -eq 0 ]; then
        log "nothing to remove"
    else
        log "configuration is left in place (see 'ocinfinity config path')"
    fi
    exit 0
}

case "${1:-}" in
    --uninstall | uninstall) uninstall ;;
esac

# --------------------------------------------------------------- discovery ---

need uname
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 \
    || die "either curl or wget is required"

_os="$(uname -s)"
_arch="$(uname -m)"

case "$_os" in
    Linux) _platform="linux" ;;
    Darwin) _platform="macos" ;;
    *) die "unsupported operating system '$_os'; Windows users run install.ps1" ;;
esac

case "$_arch" in
    x86_64 | amd64) _machine="x64" ;;
    arm64 | aarch64) _machine="arm64" ;;
    *) die "unsupported architecture '$_arch'" ;;
esac

if [ "$_platform" = "linux" ] && [ "$_machine" = "arm64" ]; then
    die "no Linux arm64 build is published; build from source instead"
fi

if [ "$_platform" = "macos" ]; then
    if [ "$_machine" = "arm64" ]; then
        _asset="ocinfinity-macos-arm"
    else
        _asset="ocinfinity-macos"
    fi
else
    _asset="ocinfinity-linux"
fi

_install_dir="${OCNINFINITY_INSTALL_DIR:-}"
if [ -z "$_install_dir" ]; then
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        _install_dir="/usr/local/bin"
    else
        _install_dir="$HOME/.local/bin"
    fi
fi

# ------------------------------------------------------------------ version ---

_tag="${OCNINFINITY_VERSION:-}"
if [ -z "$_tag" ]; then
    log "resolving the latest release..."
    _release_json="$(fetch_text "https://api.github.com/repos/${REPO}/releases/latest")" \
        || die "could not query the latest release"
    _tag="$(printf '%s' "$_release_json" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$_tag" ] || die "could not determine the latest tag; set OCNINFINITY_VERSION"
fi

log "installing $BIN $_tag ($_platform/$_machine)"

# ---------------------------------------------------------------- download ---

_tmp="$(mktemp -d 2>/dev/null || mktemp -d -t ocinfinity)"
trap 'rm -rf "$_tmp"' EXIT INT TERM

_base="https://github.com/${REPO}/releases/download/${_tag}"

fetch "${_base}/${_asset}" "$_tmp/$_asset" \
    || die "download failed: ${_base}/${_asset}"

if fetch "${_base}/SHA256SUMS" "$_tmp/SHA256SUMS" 2>/dev/null; then
    _expected="$(sed -n "s/^\([0-9a-f]\{64\}\)  ${_asset}\$/\1/p" "$_tmp/SHA256SUMS" | head -n 1)"
    if [ -n "$_expected" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            _actual="$(sha256sum "$_tmp/$_asset" | cut -d' ' -f1)"
        elif command -v shasum >/dev/null 2>&1; then
            _actual="$(shasum -a 256 "$_tmp/$_asset" | cut -d' ' -f1)"
        else
            _actual=""
            log "warning: no sha256 tool found, skipping verification"
        fi
        if [ -n "$_actual" ] && [ "$_actual" != "$_expected" ]; then
            die "checksum mismatch for $_asset (expected $_expected, got $_actual)"
        fi
        if [ -n "$_actual" ]; then
            log "checksum verified"
        fi
    fi
else
    log "warning: SHA256SUMS not published for $_tag, skipping verification"
fi

# ----------------------------------------------------------------- install ---

mkdir -p "$_install_dir" || die "cannot create $_install_dir"
install -m 0755 "$_tmp/$_asset" "$_install_dir/$BIN" \
    || die "could not install to $_install_dir"

log "installed $_install_dir/$BIN"

case ":${PATH}:" in
    *":${_install_dir}:"*) ;;
    *)
        log ""
        log "$_install_dir is not on your PATH. Add it with:"
        log "  export PATH=\"${_install_dir}:\$PATH\""
        ;;
esac

log ""
log "next:"
log "  $BIN doctor          # check the local setup"
log "  $BIN connect claude-code   # start the bridge and open Claude Code"
