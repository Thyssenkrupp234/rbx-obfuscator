#!/bin/sh
set -eu

REPO_URL="${RBXL_OBFUSCATE_REPO_URL:-https://github.com/Thyssenkrupp234/roblox-obfuscator.git}"
BRANCH="${RBXL_OBFUSCATE_BRANCH:-main}"
INSTALL_ROOT="${RBXL_OBFUSCATE_INSTALL_ROOT:-$HOME/.rbxl-obfuscate}"
SOURCE_DIR="$INSTALL_ROOT/source"
BIN_DIR="${RBXL_OBFUSCATE_BIN_DIR:-$HOME/.local/bin}"
BIN_NAME="rbxl-obfuscate"
PROMETHEUS_INSTALL_URL="https://raw.githubusercontent.com/prometheus-lua/Prometheus/master/install.sh"

info() {
    printf '%s\n' "==> $*"
}

warn() {
    printf '%s\n' "warning: $*" >&2
}

need_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf '%s\n' "error: required command not found: $1" >&2
        exit 1
    fi
}

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        return
    fi

    need_command curl
    info "Rust/Cargo not found; installing Rust with rustup"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y

    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        printf '%s\n' "error: cargo was not found after rustup install. Open a new shell and rerun this script." >&2
        exit 1
    fi
}

download_source() {
    need_command git
    mkdir -p "$INSTALL_ROOT"

    if [ -d "$SOURCE_DIR/.git" ]; then
        info "Updating rbxl-obfuscate source in $SOURCE_DIR"
        git -C "$SOURCE_DIR" fetch --depth 1 origin "$BRANCH"
        git -C "$SOURCE_DIR" checkout "$BRANCH"
        git -C "$SOURCE_DIR" reset --hard "origin/$BRANCH"
    else
        info "Downloading rbxl-obfuscate from $REPO_URL"
        if [ -e "$SOURCE_DIR" ]; then
            printf '%s\n' "error: $SOURCE_DIR exists but is not a git checkout" >&2
            printf '%s\n' "Move it aside or set RBXL_OBFUSCATE_INSTALL_ROOT to a different directory." >&2
            exit 1
        fi
        git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$SOURCE_DIR"
    fi
}

install_prometheus() {
    need_command curl

    if command -v prometheus-lua >/dev/null 2>&1; then
        info "Prometheus is already installed; attempting update"
        if ! prometheus-lua update; then
            warn "prometheus-lua update failed; reinstalling with the official installer"
            curl -fsSL "$PROMETHEUS_INSTALL_URL" | sh
        fi
    else
        info "Installing Prometheus"
        curl -fsSL "$PROMETHEUS_INSTALL_URL" | sh
    fi
}

install_binary() {
    mkdir -p "$BIN_DIR"
    info "Building release binary"
    cargo build --manifest-path "$SOURCE_DIR/Cargo.toml" --release

    info "Installing $BIN_NAME to $BIN_DIR"
    cp "$SOURCE_DIR/target/release/$BIN_NAME" "$BIN_DIR/$BIN_NAME"
    chmod 755 "$BIN_DIR/$BIN_NAME"
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
        printf '%s\n' '# Added by rbxl-obfuscate installer'
        printf '%s\n' "$path_line"
    } >>"$profile_file"
    info "Added $BIN_DIR to PATH in $profile_file"
}

ensure_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) warn "$BIN_DIR is not on PATH for this shell session" ;;
    esac

    append_path_to_file "$HOME/.profile"

    shell_name="$(basename "${SHELL:-}")"
    case "$shell_name" in
        zsh) append_path_to_file "$HOME/.zshrc" ;;
        bash) append_path_to_file "$HOME/.bashrc" ;;
    esac
}

main() {
    need_command curl
    ensure_cargo
    download_source
    install_prometheus
    install_binary
    ensure_path

    info "Installed $BIN_NAME"
    info "Run '$BIN_NAME --help' from a new terminal, or run this now:"
    printf '%s\n' "    export PATH=\"$BIN_DIR:\$PATH\""
}

main "$@"
