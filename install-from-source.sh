#!/usr/bin/env bash
# fox-services-top source installer.
#
#   curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/install-from-source.sh | bash
#   ./install-from-source.sh --ref v2.2.0
#   ./install-from-source.sh --service          # also install the headless collector's user unit
#   ./install-from-source.sh --cpu-power        # also make the CPU power counters readable (needs sudo)
#   ./install-from-source.sh --install-rust     # install rustup first if cargo is missing
#
# Hosts: Linux on x86_64 or aarch64 with glibc, WSL 2 included; WSL 1 and musl
# are refused before anything is cloned.
#
# Where install.sh downloads a published release, this builds one from a git
# checkout with the toolchain pinned by rust-toolchain.toml. It is the way to
# install when no release exists yet, and the only way to get `--daemon` until
# one is published.
#
# Safety properties (matching install.sh):
# * builds in a temporary directory, never in the install directory,
# * runs the built binary with --version --verbose to prove it loads on this host,
# * replaces the installed binary with an atomic rename and keeps the previous
#   one as `fox-services-top.previous` for rollback (`install.sh --rollback`),
# * keeps the configuration and state directories untouched.
#
# With --service the systemd *user* unit from packaging/systemd is installed
# and enabled: the headless collector starts with the session (or at boot with
# `loginctl enable-linger`), and any `fox-services-top` attaches to it. See
# "Background collector" in docs/USER_GUIDE.md.
#
# With --cpu-power the udev rule from packaging/udev is installed (as root) so
# the collector reads the powercap/RAPL energy counters as a normal user; the
# CPU's power, the CPU + GPU total and the power charts need it. Without it a
# non-root run shows no CPU power, which the installer notes at the end.
set -euo pipefail

REPO="harishgovardhandamodar/FoxServicesTop"
BINARY="fox-services-top"
REF="master"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
SERVICE=0
CPU_POWER=0
INSTALL_RUST=0
STAGING=""

cleanup() {
    if [ -n "$STAGING" ] && [ -d "$STAGING" ]; then
        rm -rf -- "$STAGING"
    fi
}
trap cleanup EXIT

info() { printf '\033[1;34m[info]\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$1" >&2; }
fail() { printf '\033[1;31m[error]\033[0m %s\n' "$1" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: install-from-source.sh [--ref GIT-REF] [--install-dir DIR] [--service] [--cpu-power] [--install-rust]

  --ref           git branch, tag or commit to build (default: master)
  --install-dir   target directory (default: $INSTALL_DIR or ~/.local/bin)
  --service       install and enable the systemd user collector service
  --cpu-power     make the CPU power counters readable (powercap/RAPL; uses sudo)
  --install-rust  install rustup (with the pinned toolchain) if cargo is missing
EOF
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --ref) REF="${2:?--ref needs a value}"; shift 2 ;;
            --install-dir) INSTALL_DIR="${2:?--install-dir needs a value}"; shift 2 ;;
            --service) SERVICE=1; shift ;;
            --cpu-power) CPU_POWER=1; shift ;;
            --install-rust) INSTALL_RUST=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) fail "unknown argument: $1" ;;
        esac
    done
}

require() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool not found: $1"
}

is_musl() {
    [ -f /etc/alpine-release ] && return 0
    # musl's `ldd --version` prints its banner on stderr and exits 1, so the
    # output is captured whatever the exit status.
    local report
    report="$(ldd --version 2>&1 || true)"
    case "$report" in
        *[Mm][Uu][Ss][Ll]*) return 0 ;;
    esac
    return 1
}

detect_host() {
    local system arch
    system="$(uname -s)"
    [ "$system" = "Linux" ] || fail "fox-services-top supports Linux only (found ${system})"
    case "$(uname -r)" in
        # WSL 1 translates system calls without a Linux kernel: no pidfd, no GPU.
        *-Microsoft) fail "WSL 1 is not supported (kernel $(uname -r)); use WSL 2" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) fail "unsupported architecture: $(uname -m)" ;;
    esac
    if is_musl; then
        fail "musl libc detected: only GNU builds are supported"
    fi
    echo "${arch}-unknown-linux-gnu"
}

ensure_rust() {
    # rustup reads rust-toolchain.toml in the checkout and fetches the pinned
    # toolchain on the first build by itself.
    if ! command -v cargo >/dev/null 2>&1; then
        if [ "$INSTALL_RUST" != "1" ]; then
            fail "cargo not found; install Rust from https://rustup.rs, or pass --install-rust"
        fi
        require curl
        info "installing rustup (minimal profile)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        # shellcheck disable=SC1091
        [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
        command -v cargo >/dev/null 2>&1 || fail "rustup installed but cargo is still not in PATH (restart your shell)"
    fi
    # The release binary is linked by cc; a Rust-only install has no linker.
    command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 \
        || fail "no C compiler found; install one (Debian/Ubuntu: sudo apt install build-essential)"
}

build() {
    local checkout="$1"
    info "cloning ${REPO} (${REF})"
    git clone --quiet "https://github.com/${REPO}.git" "$checkout" \
        || fail "could not clone https://github.com/${REPO}.git"
    git -C "$checkout" checkout --quiet "$REF" \
        || fail "no branch, tag or commit named ${REF} in ${REPO}"
    BUILD_COMMIT="$(git -C "$checkout" rev-parse --short HEAD)"
    info "building ${BINARY} (${BUILD_COMMIT}, release, --locked)"
    (cd "$checkout" && cargo build --release --locked) \
        || fail "the build failed; the toolchain above is what this checkout pins (rust-toolchain.toml)"
}

verify_binary() {
    local candidate="$1" target="$2" report
    # Prove the freshly built binary loads on this host before it replaces
    # anything: a missing shared library or a too-new glibc shows up here.
    if ! report="$("$candidate" --version --verbose 2>&1)"; then
        case "$report" in
            *GLIBC_*) fail "built binary needs a newer glibc than this host has ($(getconf GNU_LIBC_VERSION 2>/dev/null || echo unknown)): $report" ;;
        esac
        fail "built binary does not run on this host: $report"
    fi
    grep -qxF -- "target: ${target}" <<<"$report" || fail "built binary target does not match ${target}"
    BUILD_VERSION="$(sed -n "s/^${BINARY} \([^ ]*\)$/\1/p" <<<"$report" | head -n 1)"
    [ -n "$BUILD_VERSION" ] || fail "built binary did not report its version"
}

install_binary() {
    local candidate="$1" destination="${INSTALL_DIR}/${BINARY}"
    mkdir -p "$INSTALL_DIR"
    # Stage on the SAME filesystem as the target so the final replace is one
    # atomic rename (as install.sh does), and keep the previous binary.
    local staging
    staging="$(mktemp -d "${INSTALL_DIR}/.${BINARY}-install.XXXXXX")"
    cp -- "$candidate" "${staging}/${BINARY}"
    chmod 0755 "${staging}/${BINARY}"
    if [ -f "$destination" ]; then
        cp -p -- "$destination" "${destination}.previous"
        info "previous binary kept as ${destination}.previous"
    fi
    mv -f -- "${staging}/${BINARY}" "$destination"
    rm -rf -- "$staging"
    info "installed ${BINARY} ${BUILD_VERSION} (${BUILD_COMMIT}) to ${destination}"

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) warn "${INSTALL_DIR} is not in PATH; add it, e.g. echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
    esac
}

install_service() {
    require systemctl
    local unit_dir="$HOME/.config/systemd/user" unit="${BINARY}.service"
    local source="${STAGING}/checkout/packaging/systemd/${unit}"
    [ -f "$source" ] || fail "the checkout has no packaging/systemd/${unit}"
    if [ "$INSTALL_DIR" != "$HOME/.local/bin" ]; then
        warn "the unit's ExecStart points at ~/.local/bin/${BINARY}; edit ${unit_dir}/${unit} for ${INSTALL_DIR}"
    fi
    mkdir -p "$unit_dir"
    cp -- "$source" "${unit_dir}/${unit}"
    info "installed ${unit_dir}/${unit}"
    if ! systemctl --user daemon-reload 2>/dev/null; then
        warn "cannot reach this user's systemd session bus; the unit is installed but not enabled"
        warn "from a login shell of this user run: systemctl --user enable --now ${unit}"
        return
    fi
    systemctl --user enable --now "$unit" || {
        warn "the service did not start; see: systemctl --user status ${unit}"
        return
    }
    info "collector service enabled and started"
    info "start it at boot (before login) with: loginctl enable-linger \"$USER\""
}

# powercap/RAPL energy counters are root-only by default (Linux 5.10+), so a
# non-root collector reads no CPU power. The udev rule restores read access.
install_cpu_power_rule() {
    require udevadm
    local rule="60-fox-services-top-powercap.rules"
    local rule_dir="/etc/udev/rules.d"
    local source="${STAGING}/checkout/packaging/udev/${rule}"
    [ -f "$source" ] || fail "the checkout has no packaging/udev/${rule}"
    local sudo_cmd=()
    if [ "$(id -u)" != "0" ]; then
        command -v sudo >/dev/null 2>&1 || fail "--cpu-power needs root; install ${source} into ${rule_dir} by hand"
        sudo_cmd=(sudo)
    fi
    "${sudo_cmd[@]}" install -m 0644 -- "$source" "${rule_dir}/${rule}" \
        || fail "could not install ${rule_dir}/${rule}"
    info "installed ${rule_dir}/${rule}"
    "${sudo_cmd[@]}" udevadm control --reload-rules || warn "could not reload the udev rules"
    "${sudo_cmd[@]}" udevadm trigger --subsystem-match=powercap || warn "could not trigger the powercap devices"
    info "CPU power counters are readable without root; the collector picks them up on its own"
}

# Notes the option when CPU power is hidden only by permissions, so the missing
# ⚡ is never a silent surprise.
hint_cpu_power() {
    local counter
    for counter in /sys/class/powercap/*/energy_uj; do
        [ -e "$counter" ] || continue
        if [ ! -r "$counter" ]; then
            info "CPU power counters are root-only: re-run with --cpu-power to make them readable, or run the collector as root"
            return
        fi
    done
}

main() {
    parse_args "$@"
    # The host is checked first, so an unsupported system hears that rather
    # than which of the tools below it lacks.
    detect_host >/dev/null
    require git
    ensure_rust
    STAGING="$(mktemp -d)"
    build "${STAGING}/checkout"
    local candidate="${STAGING}/checkout/target/release/${BINARY}"
    [ -f "$candidate" ] || fail "build produced no binary at target/release/${BINARY}"
    verify_binary "$candidate" "$(detect_host)"
    install_binary "$candidate"
    if [ "$CPU_POWER" = "1" ]; then
        install_cpu_power_rule
    else
        hint_cpu_power
    fi
    if [ "$SERVICE" = "1" ]; then
        install_service
    else
        info "to keep collecting with no window open: fox-services-top --daemon (see docs/USER_GUIDE.md)"
    fi
}

main "$@"
