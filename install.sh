#!/bin/sh
set -eu

REPO_URL="${RBX_OBFUSCATOR_REPO_URL:-https://github.com/Thyssenkrupp234/roblox-obfuscator.git}"
BRANCH="${RBX_OBFUSCATOR_BRANCH:-main}"
INSTALL_ROOT="${RBX_OBFUSCATOR_INSTALL_ROOT:-$HOME/.rbx-obfuscator}"
SOURCE_DIR="$INSTALL_ROOT/source"
BIN_DIR="${RBX_OBFUSCATOR_BIN_DIR:-$HOME/.local/bin}"
BIN_NAME="rbx-obfuscator"
PROMETHEUS_INSTALL_URL="https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh"
VERBOSE="${RBX_OBFUSCATOR_VERBOSE:-0}"

usage() {
    cat <<EOF
rbx-obfuscator installer

Usage:
  install.sh [--verbose]

Environment:
  RBX_OBFUSCATOR_REPO_URL      Git repository URL
  RBX_OBFUSCATOR_BRANCH        Branch to install
  RBX_OBFUSCATOR_INSTALL_ROOT  Source checkout directory
  RBX_OBFUSCATOR_BIN_DIR       Directory for the installed binary
  RBX_OBFUSCATOR_VERBOSE=1     Show command output
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --verbose | -v)
            VERBOSE=1
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            printf '%s\n' "error: unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
    shift
done

title() {
    printf '\n%s\n' "rbx-obfuscator installer"
    printf '%s\n\n' "========================"
}

say() {
    printf '%s\n' "$*"
}

step() {
    printf '%s' "  -> $* ... "
}

ok() {
    printf '%s\n' "ok"
}

warn() {
    printf '%s\n' "warning: $*" >&2
}

die() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

need_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        die "required command not found: $1"
    fi
}

print_log_tail() {
    log_file="$1"
    if [ -s "$log_file" ]; then
        printf '\n%s\n' "Last installer output:"
        tail -n 20 "$log_file" >&2 || true
    fi
    printf '%s\n' "Full log: $log_file" >&2
}

run_cmd() {
    label="$1"
    shift
    step "$label"

    if [ "$VERBOSE" = "1" ]; then
        printf '\n'
        if "$@"; then
            say "     ok"
            return
        fi
        say "     failed"
        exit 1
    fi

    log_file="$(mktemp "${TMPDIR:-/tmp}/rbx-obfuscator-install.XXXXXX")"
    if "$@" >"$log_file" 2>&1; then
        rm -f "$log_file"
        ok
    else
        printf '%s\n' "failed"
        print_log_tail "$log_file"
        exit 1
    fi
}

try_cmd() {
    label="$1"
    shift
    step "$label"

    if [ "$VERBOSE" = "1" ]; then
        printf '\n'
        if "$@"; then
            say "     ok"
            return 0
        fi
        say "     failed"
        return 1
    fi

    log_file="$(mktemp "${TMPDIR:-/tmp}/rbx-obfuscator-install.XXXXXX")"
    if "$@" >"$log_file" 2>&1; then
        rm -f "$log_file"
        ok
        return 0
    fi

    printf '%s\n' "failed"
    warn "see $log_file for details"
    return 1
}

run_shell() {
    label="$1"
    command="$2"
    step "$label"

    if [ "$VERBOSE" = "1" ]; then
        printf '\n'
        if sh -c "$command"; then
            say "     ok"
            return
        fi
        say "     failed"
        exit 1
    fi

    log_file="$(mktemp "${TMPDIR:-/tmp}/rbx-obfuscator-install.XXXXXX")"
    if sh -c "$command" >"$log_file" 2>&1; then
        rm -f "$log_file"
        ok
    else
        printf '%s\n' "failed"
        print_log_tail "$log_file"
        exit 1
    fi
}

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        step "Rust toolchain"
        ok
        return
    fi

    need_command curl
    run_shell "Install Rust toolchain" \
        "curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y"

    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    command -v cargo >/dev/null 2>&1 ||
        die "cargo was not found after rustup install. Open a new shell and rerun this script."
}

download_source() {
    need_command git
    mkdir -p "$INSTALL_ROOT"

    if [ -d "$SOURCE_DIR/.git" ]; then
        run_cmd "Fetch latest source" git -C "$SOURCE_DIR" fetch --depth 1 origin "$BRANCH"
        run_cmd "Check out $BRANCH" git -C "$SOURCE_DIR" checkout "$BRANCH"
        run_cmd "Reset source checkout" git -C "$SOURCE_DIR" reset --hard "origin/$BRANCH"
    else
        if [ -e "$SOURCE_DIR" ]; then
            die "$SOURCE_DIR exists but is not a git checkout. Move it aside or set RBX_OBFUSCATOR_INSTALL_ROOT."
        fi
        run_cmd "Download source" git clone --quiet --depth 1 --branch "$BRANCH" "$REPO_URL" "$SOURCE_DIR"
    fi
}

install_prometheus() {
    need_command curl

    if command -v prometheus-lua >/dev/null 2>&1; then
        if try_cmd "Update Prometheus" prometheus-lua update; then
            return
        fi
        warn "prometheus-lua update failed; reinstalling with the official installer"
    fi

    run_shell "Install Prometheus" "curl -fsSL '$PROMETHEUS_INSTALL_URL' | sh"
}

install_binary() {
    mkdir -p "$BIN_DIR"
    run_cmd "Build release binary" cargo build --manifest-path "$SOURCE_DIR/Cargo.toml" --release
    run_cmd "Install $BIN_NAME" cp "$SOURCE_DIR/target/release/$BIN_NAME" "$BIN_DIR/$BIN_NAME"
    run_cmd "Set executable bit" chmod 755 "$BIN_DIR/$BIN_NAME"
}

append_path_to_file() {
    profile_file="$1"
    path_line="export PATH=\"$BIN_DIR:\$PATH\""

    mkdir -p "$(dirname "$profile_file")"
    touch "$profile_file"

    if grep -F "$BIN_DIR" "$profile_file" >/dev/null 2>&1; then
        return
    fi

    {
        printf '\n'
        printf '%s\n' '# Added by rbx-obfuscator installer'
        printf '%s\n' "$path_line"
    } >>"$profile_file"
}

ensure_path() {
    step "Shell PATH"
    append_path_to_file "$HOME/.profile"

    shell_name="$(basename "${SHELL:-}")"
    case "$shell_name" in
        zsh) append_path_to_file "$HOME/.zshrc" ;;
        bash) append_path_to_file "$HOME/.bashrc" ;;
    esac
    ok
}

main() {
    title
    say "Installing from $REPO_URL ($BRANCH)"
    say "Binary target: $BIN_DIR/$BIN_NAME"
    say ""

    need_command curl
    ensure_cargo
    download_source
    install_prometheus
    install_binary
    ensure_path

    say ""
    say "Done. Try it with:"
    say "  $BIN_DIR/$BIN_NAME --help"
    say ""
    say "For this terminal session, run:"
    say "  export PATH=\"$BIN_DIR:\$PATH\""
}

main "$@"
