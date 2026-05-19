#!/usr/bin/env bash
# =========================================================================
# deadmkt-node one-liner installer (MR4a)
#
# Usage:
#   curl -sSL https://get.deadmkt.com | bash
#   curl -sSL https://get.deadmkt.com | bash -s -- --rebuild
#   ./install.sh [--rebuild] [--ref BRANCH] [--non-interactive]
#
# Takes a fresh Linux VPS with sudo and turns it into a running deadmkt
# node. Installs Docker + git, clones the repo, builds the image, runs
# the MR1a non-interactive setup, starts the node with
# --restart=unless-stopped, and prints the MR2 status JSON.
#
# Idempotent: re-running with an existing keystore skips prompts and
# only restarts the container if needed.
#
# Spec: planning/archive/MR4-installer.md
# =========================================================================

set -euo pipefail

# ── Config ───────────────────────────────────────────────────────────────
REPO_URL="${DEADMKT_REPO_URL:-https://github.com/anthropics/deadmkt-node.git}"
REPO_REF="${DEADMKT_REPO_REF:-release}"
REPO_DIR="${DEADMKT_REPO_DIR:-$HOME/deadmkt-node}"
DATA_DIR="${DEADMKT_DATA_DIR:-$HOME/.deadmkt}"
IMAGE_TAG="${DEADMKT_IMAGE_TAG:-deadmkt-node:local}"
CONTAINER_NAME="${DEADMKT_CONTAINER:-deadmkt-node}"

REBUILD=0
NON_INTERACTIVE=0

# ── Flags ────────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --rebuild)         REBUILD=1; shift ;;
        --ref)             REPO_REF="$2"; shift 2 ;;
        --non-interactive) NON_INTERACTIVE=1; shift ;;
        --help|-h)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown flag: $1" >&2
            exit 2
            ;;
    esac
done

# ── Output helpers ───────────────────────────────────────────────────────
say()   { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m  +\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m  !\033[0m %s\n' "$*" >&2; }
die()   { printf '\033[1;31m  x\033[0m %s\n' "$*" >&2; exit 1; }

# ── Prereq checks ────────────────────────────────────────────────────────
check_prereqs() {
    say "Checking prerequisites"

    [ "$(uname -s)" = "Linux" ] || die "Linux only. Detected $(uname -s)."

    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|aarch64) ok "Architecture: $arch" ;;
        *) die "Unsupported architecture: $arch (need x86_64 or aarch64)" ;;
    esac

    # sudo: either we're root, or sudo works.
    if [ "$(id -u)" != "0" ]; then
        if ! command -v sudo >/dev/null 2>&1; then
            die "Not running as root and sudo is not installed."
        fi
        # Don't require -n; an interactive sudo prompt is acceptable here
        # because the installer is end-user-facing.
        if ! sudo -v >/dev/null 2>&1; then
            die "sudo authentication failed. Run the installer as a user with sudo access."
        fi
        SUDO="sudo"
    else
        SUDO=""
    fi
    ok "Privileged: $([ -n "$SUDO" ] && echo "via sudo" || echo "running as root")"

    # Distro detection
    if [ ! -r /etc/os-release ]; then
        die "Cannot read /etc/os-release; unsupported distro."
    fi
    # shellcheck disable=SC1091
    . /etc/os-release
    case "${ID:-unknown}" in
        ubuntu|debian|fedora|centos|rhel|rocky|almalinux)
            ok "Distro: ${PRETTY_NAME:-$ID}"
            DISTRO_ID="$ID"
            ;;
        *)
            die "Unsupported distro: ${ID:-unknown}. Install Docker + git manually, then re-run."
            ;;
    esac
}

# ── Package installs ─────────────────────────────────────────────────────
pkg_install() {
    # Install one or more packages via the right package manager.
    case "$DISTRO_ID" in
        ubuntu|debian)
            $SUDO apt-get update -qq
            $SUDO apt-get install -y -qq "$@"
            ;;
        fedora|centos|rhel|rocky|almalinux)
            $SUDO dnf install -y -q "$@" 2>/dev/null \
                || $SUDO yum install -y -q "$@"
            ;;
    esac
}

ensure_git() {
    if command -v git >/dev/null 2>&1; then
        ok "git: $(git --version | head -1)"
        return
    fi
    say "Installing git"
    pkg_install git
    ok "git installed"
}

ensure_docker() {
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        ok "Docker: $(docker --version)"
        return
    fi

    if command -v docker >/dev/null 2>&1 && ! docker info >/dev/null 2>&1; then
        # Docker present but daemon not reachable -- usually a permission issue.
        # Try with sudo; if that works, the user needs to be in the docker group.
        if $SUDO docker info >/dev/null 2>&1; then
            warn "Docker daemon reachable via sudo but not as $(id -un)."
            warn "Add yourself to the docker group:  sudo usermod -aG docker $(id -un)"
            warn "Then log out and back in, or run:  newgrp docker"
            DOCKER="$SUDO docker"
            return
        fi
        die "Docker is installed but not running. Start it with: $SUDO systemctl start docker"
    fi

    say "Installing Docker (via get.docker.com)"
    curl -fsSL https://get.docker.com -o /tmp/get-docker.sh
    $SUDO sh /tmp/get-docker.sh
    rm -f /tmp/get-docker.sh

    # Enable + start
    if command -v systemctl >/dev/null 2>&1; then
        $SUDO systemctl enable --now docker
    fi

    # Group membership for non-root users
    if [ -n "$SUDO" ]; then
        $SUDO usermod -aG docker "$(id -un)" || true
        warn "Added $(id -un) to the docker group."
        warn "The current shell still needs sudo for docker until you re-login."
        DOCKER="$SUDO docker"
    else
        DOCKER="docker"
    fi
    ok "Docker installed: $($DOCKER --version)"
}

# ── Repo + build ─────────────────────────────────────────────────────────
clone_or_pull() {
    if [ -d "$REPO_DIR/.git" ]; then
        say "Updating existing repo at $REPO_DIR"
        # Refuse to clobber uncommitted work.
        if ! git -C "$REPO_DIR" diff --quiet || ! git -C "$REPO_DIR" diff --cached --quiet; then
            die "$REPO_DIR has uncommitted changes. Stash or commit them before re-running."
        fi
        git -C "$REPO_DIR" fetch --quiet origin "$REPO_REF"
        git -C "$REPO_DIR" checkout --quiet "$REPO_REF"
        git -C "$REPO_DIR" pull --ff-only --quiet
        ok "Repo at $(git -C "$REPO_DIR" rev-parse --short HEAD) on $REPO_REF"
    else
        say "Cloning $REPO_URL ($REPO_REF) to $REPO_DIR"
        git clone --quiet --branch "$REPO_REF" --depth 1 "$REPO_URL" "$REPO_DIR"
        ok "Cloned to $REPO_DIR"
    fi
}

build_image() {
    if [ "$REBUILD" -eq 0 ] && $DOCKER image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
        ok "Image $IMAGE_TAG already built; skipping (use --rebuild to force)"
        return
    fi
    say "Building Docker image (this takes 3-6 minutes on a small VPS)"
    (cd "$REPO_DIR" && $DOCKER build -t "$IMAGE_TAG" .)
    ok "Image built: $IMAGE_TAG"
}

# ── Interactive setup ────────────────────────────────────────────────────
prompt_with_default() {
    # $1 = prompt, $2 = default; reads from /dev/tty so it works under `curl | bash`.
    local prompt="$1" default="$2" reply
    if [ -n "$default" ]; then
        printf '%s [%s]: ' "$prompt" "$default" > /dev/tty
    else
        printf '%s: ' "$prompt" > /dev/tty
    fi
    IFS= read -r reply < /dev/tty || reply=""
    printf '%s\n' "${reply:-$default}"
}

prompt_password() {
    # Reads twice; no-echo via stty. Returns via stdout.
    local prompt="$1" p1 p2
    while :; do
        printf '%s: ' "$prompt" > /dev/tty
        stty -echo < /dev/tty
        IFS= read -r p1 < /dev/tty || p1=""
        stty echo < /dev/tty
        printf '\n' > /dev/tty
        if [ "${#p1}" -lt 8 ]; then
            printf '  (password must be at least 8 characters)\n' > /dev/tty
            continue
        fi
        printf 'Confirm %s: ' "$prompt" > /dev/tty
        stty -echo < /dev/tty
        IFS= read -r p2 < /dev/tty || p2=""
        stty echo < /dev/tty
        printf '\n' > /dev/tty
        if [ "$p1" = "$p2" ]; then
            printf '%s\n' "$p1"
            return
        fi
        printf '  (passwords did not match; try again)\n' > /dev/tty
    done
}

validate_address() {
    # Lower-cased, 0x-prefixed, exactly 64 hex chars after the prefix.
    local addr="$1"
    addr="$(printf '%s' "$addr" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')"
    [[ "$addr" =~ ^0x[0-9a-f]{1,64}$ ]] || return 1
    printf '%s\n' "$addr"
}

run_setup() {
    # Skip entirely if keystore already exists.
    if [ -f "$DATA_DIR/keystore.json" ]; then
        ok "Keystore already present at $DATA_DIR/keystore.json -- skipping setup."
        return 0
    fi

    if [ "$NON_INTERACTIVE" -eq 1 ]; then
        die "Non-interactive mode requested but no keystore present. Re-run interactively or pre-place keystore.json + run --config setup.json yourself."
    fi

    say "Configuring node"
    printf 'No keystore found. We need three things to set up your node:\n' > /dev/tty
    printf '  1. The beneficiary address (a Supra wallet you control -- receives profits and exits)\n' > /dev/tty
    printf '  2. A keystore password (8+ chars; ENCRYPTS your trustee key; LOSE IT = lose the NFT)\n' > /dev/tty
    printf '  3. The network (testnet for now)\n\n' > /dev/tty

    local beneficiary password
    while :; do
        local raw
        raw="$(prompt_with_default 'Beneficiary address (0x...)' '')"
        beneficiary="$(validate_address "$raw" || true)"
        if [ -n "$beneficiary" ]; then
            ok "Beneficiary: $beneficiary"
            break
        fi
        printf '  (invalid address; need 0x followed by up to 64 hex chars)\n' > /dev/tty
    done

    password="$(prompt_password 'Keystore password')"

    # Setup.json on host -- mounted read-only into the container for the
    # MR1a non-interactive setup run. chmod 600 so MR1d's perm check
    # accepts it (`enforce_setup_config_permissions`).
    mkdir -p "$DATA_DIR"
    chmod 700 "$DATA_DIR"
    local tmp_setup
    tmp_setup="$(mktemp "$DATA_DIR/setup.XXXXXX.json")"
    chmod 600 "$tmp_setup"
    # We deliberately do not write the password to a logged shell line.
    # Use printf into the file via a heredoc-style construct.
    cat > "$tmp_setup" <<EOF
{
  "network": "testnet",
  "beneficiary_address": "$beneficiary",
  "node_role": "trading",
  "keystore_password": "$password"
}
EOF
    # Clear the password var ASAP.
    password=""

    say "Running first-boot setup inside the container"
    local setup_log
    setup_log="$(mktemp)"
    # Bind-mount the host data dir. The container writes keystore.json +
    # config.json into /data, which lands at $DATA_DIR on the host.
    set +e
    $DOCKER run --rm \
        -v "$DATA_DIR:/data" \
        "$IMAGE_TAG" \
        deadmkt-node --config /data/"$(basename "$tmp_setup")" \
        > "$setup_log" 2>&1
    local rc=$?
    set -e

    # Setup may have scrubbed the password (MR1d); delete the file
    # outright regardless to avoid any chance of leaving secrets on disk.
    rm -f "$tmp_setup"

    if [ "$rc" -ne 0 ]; then
        warn "Setup failed (exit $rc). Last 30 lines of output:"
        tail -n 30 "$setup_log" >&2
        rm -f "$setup_log"
        die "Setup did not complete. Fix the issue above and re-run the installer."
    fi
    # Show the structured result on success so the operator sees the
    # NFT id, trustee address, and any warnings.
    tail -n 1 "$setup_log"
    rm -f "$setup_log"
    ok "Setup completed; keystore + config written to $DATA_DIR"
}

# ── Run the node ─────────────────────────────────────────────────────────
start_container() {
    say "Starting node container"

    # Stop any existing container (rename-collision protection on re-run).
    if $DOCKER ps -a --format '{{.Names}}' | grep -qx "$CONTAINER_NAME"; then
        local state
        state="$($DOCKER inspect --format '{{.State.Status}}' "$CONTAINER_NAME" 2>/dev/null || echo unknown)"
        case "$state" in
            running)
                ok "Container $CONTAINER_NAME is already running."
                return 0
                ;;
            exited|created)
                ok "Restarting existing container $CONTAINER_NAME"
                $DOCKER start "$CONTAINER_NAME" >/dev/null
                return 0
                ;;
            *)
                warn "Container $CONTAINER_NAME is in state '$state'; removing and recreating"
                $DOCKER rm -f "$CONTAINER_NAME" >/dev/null
                ;;
        esac
    fi

    $DOCKER run -d \
        --name "$CONTAINER_NAME" \
        --restart=unless-stopped \
        -v "$DATA_DIR:/data" \
        -p 127.0.0.1:9090:9090 \
        -p 127.0.0.1:9292:9292 \
        -p 9191:9191 \
        "$IMAGE_TAG" \
        >/dev/null
    ok "Container started: $CONTAINER_NAME"
}

wait_for_status() {
    say "Waiting for node to come up"
    local i max=30
    for i in $(seq 1 "$max"); do
        local body
        body="$(curl -fsS --max-time 1 http://127.0.0.1:9292 2>/dev/null || true)"
        if [ -n "$body" ] && printf '%s' "$body" | grep -q '"node_running":true'; then
            ok "Status endpoint up (after ${i}s)"
            printf '\n%s\n\n' "$body"
            return 0
        fi
        sleep 1
    done
    warn "Node did not respond on 127.0.0.1:9292 within ${max}s."
    warn "Check logs:  docker logs $CONTAINER_NAME"
}

print_next_steps() {
    cat <<EOF

  =====================================================================
  deadmkt-node is running.

  Status:        curl 127.0.0.1:9292
  Status (CLI):  docker exec $CONTAINER_NAME deadmkt-node status --json
  Logs:          docker logs -f $CONTAINER_NAME
  Stop:          docker stop $CONTAINER_NAME
  Restart:       docker restart $CONTAINER_NAME
  Data dir:      $DATA_DIR (back up keystore.json -- it is your trading identity)

  Strategy WebSocket: ws://127.0.0.1:9090 (auth token in $DATA_DIR/config.json)
  Gossip port:        9191 (public; peers dial in)

  Next: connect your agent to ws://127.0.0.1:9090. The starter bot in
  $REPO_DIR/starter_bot/strategy.py is a good place to start.

  =====================================================================

EOF
}

# ── Main ─────────────────────────────────────────────────────────────────
main() {
    say "deadmkt-node installer (MR4a)"
    check_prereqs
    ensure_git
    ensure_docker
    # If ensure_docker didn't set DOCKER (clean install with group not yet
    # applied to current shell), default to sudo docker.
    DOCKER="${DOCKER:-$SUDO docker}"
    clone_or_pull
    build_image
    run_setup
    start_container
    wait_for_status
    print_next_steps
}

main "$@"
