#!/usr/bin/env bash
# DACS infrastructure host setup (Person 3: network & enrollment).
#
# Deploys the design in net/deploy/ onto a public Debian/Ubuntu server:
#   Headscale v0.29.7 (loopback) + embedded DERP/STUN, nginx public edge
#   (stream SNI router + Let's Encrypt TLS), the host's own Tailscale node
#   (tag:dacs-infra) and dacs-netadmin on the overlay (:9443).
#
# This is the infrastructure host, NOT an agent PC. Idempotent: safe to re-run.
#
# Required environment:
#   HS_DOMAIN          public name of Headscale control + DERP (e.g. hs.example.org)
#   BOOTSTRAP_DOMAIN   public name of the coordinator bootstrap relay
#   PUBLIC_IPV4        this host's public IPv4 (both names must resolve to it)
#   ACME_EMAIL         Let's Encrypt account e-mail
#   NETADMIN_BIN       path to the built Linux dacs-netadmin binary
# Optional environment:
#   PUBLIC_IPV6            this host's public IPv6 (empty: DERP advertises IPv4 only)
#   COORDINATOR_OVERLAY_IP coordinator's overlay IPv4 (100.64.0.0/10); empty until
#                          the coordinator has joined. Re-run with it set to
#                          activate the BOOTSTRAP_DOMAIN TLS passthrough.
#   ACME_AGREE_TOS=yes     accept the Let's Encrypt terms non-interactively
#                          (otherwise certbot asks on the terminal)
#   HS_API_KEY_EXPIRATION  lifetime of the netadmin Headscale API key (default 365d)
#   DACS_DEPLOY_DIR        directory holding the templates (default: this script's dir)
#
# Usage: sudo -E HS_DOMAIN=... BOOTSTRAP_DOMAIN=... PUBLIC_IPV4=... \
#            ACME_EMAIL=... NETADMIN_BIN=./dacs-netadmin ./infra-setup.sh
#
# Secrets (Headscale API key, pre-auth keys) are never printed. The operation
# token is printed exactly once, directly to the controlling terminal.

set -euo pipefail

readonly HEADSCALE_VERSION="0.29.7"
readonly HEADSCALE_RELEASE_URL="https://github.com/juanfont/headscale/releases/download/v${HEADSCALE_VERSION}"
readonly HS_ADMIN_USER="dacs-admin"
readonly NETADMIN_USER="dacs-netadmin"
readonly NETADMIN_PORT=9443
readonly NETADMIN_ETC="/etc/dacs-netadmin"
readonly NETADMIN_STATE="/var/lib/dacs-netadmin"
readonly NETADMIN_BIN_DEST="/usr/local/bin/dacs-netadmin"
readonly HS_API_KEY_FILE="${NETADMIN_ETC}/headscale-api-key"
readonly TOKEN_HASH_FILE="${NETADMIN_ETC}/operation-tokens.sha256"
readonly NETADMIN_TLS_DIR="${NETADMIN_ETC}/tls"
readonly NGINX_CONF="/etc/nginx/nginx.conf"
readonly NGINX_STREAM_CONF="/etc/nginx/dacs.conf"
readonly NGINX_HTTP_CONF="/etc/nginx/conf.d/dacs-http.conf"
readonly ACME_WEBROOT="/var/www/certbot"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="${DACS_DEPLOY_DIR:-$SCRIPT_DIR}"

HS_DOMAIN="${HS_DOMAIN:-}"
BOOTSTRAP_DOMAIN="${BOOTSTRAP_DOMAIN:-}"
PUBLIC_IPV4="${PUBLIC_IPV4:-}"
PUBLIC_IPV6="${PUBLIC_IPV6:-}"
ACME_EMAIL="${ACME_EMAIL:-}"
NETADMIN_BIN="${NETADMIN_BIN:-}"
COORDINATOR_OVERLAY_IP="${COORDINATOR_OVERLAY_IP:-}"
ACME_AGREE_TOS="${ACME_AGREE_TOS:-}"
HS_API_KEY_EXPIRATION="${HS_API_KEY_EXPIRATION:-365d}"

INFRA_OVERLAY_IP=""
NETADMIN_CERT_FP=""
NEW_TOKEN_ISSUED=0

WORK_DIR="$(mktemp -d)"
cleanup() { rm -rf -- "$WORK_DIR"; }
trap cleanup EXIT

export DEBIAN_FRONTEND=noninteractive

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

step() { printf '\n==> [%s] %s\n' "$1" "$2"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '    WARNING: %s\n' "$*" >&2; }
die() {
    printf '\nERROR: %s\n' "$*" >&2
    exit 1
}

is_ipv4() {
    local ip=$1 o
    [[ $ip =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
    for o in "${BASH_REMATCH[@]:1}"; do
        ((10#$o <= 255)) || return 1
    done
}

is_overlay_ipv4() {
    is_ipv4 "$1" || return 1
    local a b
    IFS=. read -r a b _ _ <<<"$1"
    ((10#$a == 100 && 10#$b >= 64 && 10#$b <= 127))
}

is_ipv6() {
    [[ $1 =~ ^[0-9A-Fa-f:]+$ && $1 == *:*:* && ${#1} -le 39 ]]
}

is_domain() {
    [[ $1 =~ ^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$ ]]
}

# install_file SRC DEST MODE OWNER GROUP -> 0 if DEST was created/changed, 1 if identical.
install_file() {
    local src=$1 dest=$2 mode=$3 owner=$4 group=$5
    if [[ -f $dest ]] && cmp -s -- "$src" "$dest"; then
        chmod "$mode" -- "$dest"
        chown "$owner:$group" -- "$dest"
        return 1
    fi
    install -D -m "$mode" -o "$owner" -g "$group" -- "$src" "$dest"
    info "wrote $dest"
    return 0
}

# Fail if a rendered file still contains a placeholder outside comments.
assert_no_placeholders() {
    local file=$1
    shift
    local p
    for p in "$@"; do
        if grep -vE '^[[:space:]]*(#|//)' -- "$file" | grep -q -- "$p"; then
            die "rendering $file left placeholder $p"
        fi
    done
}

# Process names listening on PROTO (t|u) PORT, one per line.
listeners_on() {
    local proto=$1 port=$2
    ss -H -l -n -p "-$proto" "sport = :$port" 2>/dev/null |
        grep -oE '\("[^"]+"' | tr -d '("' | sort -u || true
}

port_is_ours() {
    local proto=$1 port=$2 allowed=$3 any procs p
    any="$(ss -H -l -n "-$proto" "sport = :$port" 2>/dev/null || true)"
    [[ -z $any ]] && return 0
    procs="$(listeners_on "$proto" "$port")"
    [[ -z $procs ]] && return 1
    while IFS= read -r p; do
        [[ $p == "$allowed" ]] || return 1
    done <<<"$procs"
    return 0
}

headscale_healthy() {
    curl -fsS --max-time 3 http://127.0.0.1:8080/health >/dev/null 2>&1 ||
        headscale health >/dev/null 2>&1
}

tailscale_control_url() {
    tailscale debug prefs 2>/dev/null | jq -r '.ControlURL // empty' 2>/dev/null || true
}

tailscale_backend_state() {
    tailscale status --json 2>/dev/null | jq -r '.BackendState // empty' 2>/dev/null || true
}

ensure_netadmin_user() {
    if ! getent passwd "$NETADMIN_USER" >/dev/null; then
        useradd --system --user-group --home-dir "$NETADMIN_STATE" --no-create-home \
            --shell /usr/sbin/nologin "$NETADMIN_USER"
        info "created system user $NETADMIN_USER"
    fi
    install -d -m 0750 -o root -g "$NETADMIN_USER" "$NETADMIN_ETC"
}

# ---------------------------------------------------------------------------
# input validation
# ---------------------------------------------------------------------------

validate_inputs() {
    step 0 "Validating inputs"
    local errors=()

    HS_DOMAIN="${HS_DOMAIN,,}"
    BOOTSTRAP_DOMAIN="${BOOTSTRAP_DOMAIN,,}"

    is_domain "$HS_DOMAIN" || errors+=("HS_DOMAIN must be a DNS name (got '${HS_DOMAIN}')")
    is_domain "$BOOTSTRAP_DOMAIN" || errors+=("BOOTSTRAP_DOMAIN must be a DNS name (got '${BOOTSTRAP_DOMAIN}')")
    if [[ -n $HS_DOMAIN && $HS_DOMAIN == "$BOOTSTRAP_DOMAIN" ]]; then
        errors+=("HS_DOMAIN and BOOTSTRAP_DOMAIN must differ (SNI routing)")
    fi
    is_ipv4 "$PUBLIC_IPV4" || errors+=("PUBLIC_IPV4 must be an IPv4 address (got '${PUBLIC_IPV4}')")
    if [[ -n $PUBLIC_IPV6 ]] && ! is_ipv6 "$PUBLIC_IPV6"; then
        errors+=("PUBLIC_IPV6 must be an IPv6 address or empty (got '${PUBLIC_IPV6}')")
    fi
    [[ $ACME_EMAIL =~ ^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$ ]] ||
        errors+=("ACME_EMAIL must be an e-mail address (got '${ACME_EMAIL}')")
    if [[ -z $NETADMIN_BIN ]]; then
        errors+=("NETADMIN_BIN must point to the built Linux dacs-netadmin binary")
    elif [[ ! -f $NETADMIN_BIN ]]; then
        errors+=("NETADMIN_BIN '${NETADMIN_BIN}' does not exist")
    elif [[ "$(od -An -tx1 -N4 -- "$NETADMIN_BIN" | tr -d ' \n')" != "7f454c46" ]]; then
        errors+=("NETADMIN_BIN '${NETADMIN_BIN}' is not a Linux ELF binary")
    fi
    if [[ -n $COORDINATOR_OVERLAY_IP ]] && ! is_overlay_ipv4 "$COORDINATOR_OVERLAY_IP"; then
        errors+=("COORDINATOR_OVERLAY_IP must be an overlay IPv4 in 100.64.0.0/10 (got '${COORDINATOR_OVERLAY_IP}')")
    fi
    [[ $HS_API_KEY_EXPIRATION =~ ^[0-9]+[hdwy]$ ]] ||
        errors+=("HS_API_KEY_EXPIRATION must look like 365d / 52w / 8760h (got '${HS_API_KEY_EXPIRATION}')")

    local f
    for f in headscale/config.yaml headscale/policy.hujson nginx/dacs.conf \
        netadmin/config.example.json netadmin/dacs-netadmin.service; do
        [[ -f $DEPLOY_DIR/$f ]] || errors+=("template $DEPLOY_DIR/$f is missing (set DACS_DEPLOY_DIR)")
    done

    if ((${#errors[@]})); then
        printf '    - %s\n' "${errors[@]}" >&2
        die "invalid inputs; see the header of $0 for the required environment"
    fi
    info "HS_DOMAIN=$HS_DOMAIN BOOTSTRAP_DOMAIN=$BOOTSTRAP_DOMAIN"
    info "PUBLIC_IPV4=$PUBLIC_IPV4 PUBLIC_IPV6=${PUBLIC_IPV6:-<none>}"
    info "COORDINATOR_OVERLAY_IP=${COORDINATOR_OVERLAY_IP:-<not set: bootstrap relay rejects>}"
}

# ---------------------------------------------------------------------------
# 1. preflight
# ---------------------------------------------------------------------------

preflight() {
    step 1 "Preflight checks"

    ((EUID == 0)) || die "run as root (sudo -E $0)"

    [[ -r /etc/os-release ]] || die "/etc/os-release missing; Debian/Ubuntu required"
    # shellcheck disable=SC1091
    . /etc/os-release
    case "${ID:-}" in
        debian | ubuntu) ;;
        *) die "unsupported OS '${ID:-unknown}'; Debian or Ubuntu required" ;;
    esac
    [[ -n ${VERSION_CODENAME:-} ]] || die "cannot determine the release codename from /etc/os-release"
    OS_ID="$ID"
    OS_CODENAME="$VERSION_CODENAME"
    info "OS: ${PRETTY_NAME:-$ID $VERSION_CODENAME}"

    [[ -d /run/systemd/system ]] || die "systemd is required"

    ARCH="$(dpkg --print-architecture)"
    case "$ARCH" in
        amd64 | arm64) ;;
        *) die "unsupported architecture '$ARCH' (amd64 or arm64)" ;;
    esac
    info "architecture: $ARCH"

    local cmd
    for cmd in getent ss od cmp install; do
        command -v "$cmd" >/dev/null || die "required command '$cmd' not found"
    done

    local name resolved dns_ok=1
    for name in "$HS_DOMAIN" "$BOOTSTRAP_DOMAIN"; do
        resolved="$(getent ahostsv4 "$name" 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ' || true)"
        if [[ " $resolved " == *" $PUBLIC_IPV4 "* ]]; then
            info "DNS: $name -> $PUBLIC_IPV4 ok"
        else
            warn "DNS: $name resolves to '${resolved:-nothing}', expected $PUBLIC_IPV4"
            dns_ok=0
        fi
    done
    if ((dns_ok == 0)); then
        die "create A records for $HS_DOMAIN and $BOOTSTRAP_DOMAIN pointing at $PUBLIC_IPV4, wait for propagation, then re-run (ACME HTTP-01 and the Tailscale login server depend on it)"
    fi
    if [[ -n $PUBLIC_IPV6 ]]; then
        resolved="$(getent ahostsv6 "$HS_DOMAIN" 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ' || true)"
        [[ " $resolved " == *" $PUBLIC_IPV6 "* ]] ||
            warn "DNS: $HS_DOMAIN has no AAAA record for $PUBLIC_IPV6 (IPv6 clients will fall back to IPv4)"
    fi

    local bad=0
    port_is_ours t 80 nginx || {
        warn "tcp/80 is used by: $(listeners_on t 80 | tr '\n' ' ')"
        bad=1
    }
    port_is_ours t 443 nginx || {
        warn "tcp/443 is used by: $(listeners_on t 443 | tr '\n' ' ')"
        bad=1
    }
    port_is_ours u 3478 headscale || {
        warn "udp/3478 is used by: $(listeners_on u 3478 | tr '\n' ' ')"
        bad=1
    }
    ((bad == 0)) || die "stop the services above (ports 80/443 tcp and 3478 udp belong to nginx/headscale) and re-run"
    info "ports 80/tcp, 443/tcp, 3478/udp: free or owned by nginx/headscale"
}

# ---------------------------------------------------------------------------
# 2. packages
# ---------------------------------------------------------------------------

install_packages() {
    step 2 "Installing packages"
    local pkgs=(nginx libnginx-mod-stream certbot jq curl ca-certificates openssl gnupg)
    local missing=() p
    for p in "${pkgs[@]}"; do
        dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || missing+=("$p")
    done
    if ((${#missing[@]})); then
        info "installing: ${missing[*]}"
        apt-get update -q
        apt-get install -y -q --no-install-recommends "${missing[@]}"
    else
        info "all packages already installed"
    fi
}

# ---------------------------------------------------------------------------
# 3. headscale
# ---------------------------------------------------------------------------

install_headscale() {
    step 3 "Installing and configuring Headscale v${HEADSCALE_VERSION}"

    local current
    current="$(dpkg-query -W -f='${Version}' headscale 2>/dev/null || true)"
    if [[ $current == "$HEADSCALE_VERSION" ]]; then
        info "headscale $HEADSCALE_VERSION already installed"
    else
        local deb="headscale_${HEADSCALE_VERSION}_linux_${ARCH}.deb"
        local dl="$WORK_DIR/headscale-dl"
        mkdir -p -- "$dl"
        info "downloading $deb and checksums.txt"
        curl -fsSL --retry 3 -o "$dl/$deb" "$HEADSCALE_RELEASE_URL/$deb"
        curl -fsSL --retry 3 -o "$dl/checksums.txt" "$HEADSCALE_RELEASE_URL/checksums.txt"
        local expected actual
        expected="$(awk -v f="$deb" '$2 == f || $2 == "*" f {print $1}' "$dl/checksums.txt")"
        [[ $expected =~ ^[0-9a-f]{64}$ ]] || die "no sha256 for $deb in checksums.txt"
        actual="$(sha256sum -- "$dl/$deb" | awk '{print $1}')"
        [[ $actual == "$expected" ]] || die "sha256 mismatch for $deb (expected $expected, got $actual)"
        info "sha256 verified: $actual"
        chmod 0755 -- "$dl"
        chmod 0644 -- "$dl/$deb"
        apt-get install -y -q -o Dpkg::Options::=--force-confold "$dl/$deb"
    fi
    command -v headscale >/dev/null || die "headscale binary not found after install"

    local cfg="$WORK_DIR/headscale-config.yaml"
    if [[ -n $PUBLIC_IPV6 ]]; then
        sed -e "s/PUBLIC_IPV6/${PUBLIC_IPV6}/g" "$DEPLOY_DIR/headscale/config.yaml" >"$cfg.tmp"
    else
        sed -e '/^[[:space:]]*ipv6:[[:space:]]*PUBLIC_IPV6[[:space:]]*$/d' \
            -e 's/PUBLIC_IPV6/<none>/g' "$DEPLOY_DIR/headscale/config.yaml" >"$cfg.tmp"
    fi
    sed -e "s/HS_DOMAIN/${HS_DOMAIN}/g" -e "s/PUBLIC_IPV4/${PUBLIC_IPV4}/g" "$cfg.tmp" >"$cfg"
    assert_no_placeholders "$cfg" HS_DOMAIN PUBLIC_IPV4 PUBLIC_IPV6

    local pol="$WORK_DIR/policy.hujson"
    sed -e "s/HS_DOMAIN/${HS_DOMAIN}/g" "$DEPLOY_DIR/headscale/policy.hujson" >"$pol"

    local changed=0 hs_group=root
    getent group headscale >/dev/null && hs_group=headscale
    install -d -m 0755 /etc/headscale
    if install_file "$cfg" /etc/headscale/config.yaml 0640 root "$hs_group"; then changed=1; fi
    if install_file "$pol" /etc/headscale/policy.hujson 0640 root "$hs_group"; then changed=1; fi

    if headscale policy check --help >/dev/null 2>&1; then
        headscale policy check --file /etc/headscale/policy.hujson >/dev/null ||
            die "headscale rejected /etc/headscale/policy.hujson"
        info "policy validated"
    fi

    systemctl enable headscale >/dev/null 2>&1
    if ((changed)) || ! systemctl is-active --quiet headscale; then
        info "(re)starting headscale"
        systemctl restart headscale
    fi

    local i
    for ((i = 0; i < 60; i++)); do
        headscale_healthy && break
        sleep 1
    done
    if ! headscale_healthy; then
        journalctl -u headscale -n 40 --no-pager >&2 || true
        die "headscale did not become healthy within 60s"
    fi
    info "headscale healthy on 127.0.0.1:8080"
}

# ---------------------------------------------------------------------------
# 4. TLS + nginx
# ---------------------------------------------------------------------------

# True if this nginx understands the `http2 on;` directive (>= 1.25.1).
nginx_has_http2_directive() {
    local v
    v="$(nginx -v 2>&1 | sed -n 's|.*nginx/\([0-9][0-9.]*\).*|\1|p')"
    [[ -n $v ]] || return 1
    printf '1.25.1\n%s\n' "$v" | sort -V -C
}

# Split a rendered dacs.conf into its top-level stream {} part (plus header
# comments) and the *contents* of its http {} block: Debian's nginx.conf
# already has an http {} block and nginx rejects a second one, so the http
# servers go to conf.d/ (included inside Debian's http {}).
split_dacs_conf() {
    local src=$1 stream_out=$2 http_out=$3
    : >"$stream_out"
    : >"$http_out"
    awk -v sfile="$stream_out" -v hfile="$http_out" '
        {
            code = $0
            sub(/#.*/, "", code)
            o = gsub(/\{/, "{", code)
            c = gsub(/\}/, "}", code)
            if (depth == 0 && mode == "") {
                if (code ~ /^[[:space:]]*stream[[:space:]]*\{/) { mode = "s"; ns++ }
                else if (code ~ /^[[:space:]]*http[[:space:]]*\{/) { mode = "h"; nh++; depth += o - c; next }
            }
            nd = depth + o - c
            if (mode == "h") {
                if (nd == 0) { mode = ""; depth = 0; next }
                print > hfile
            } else {
                print > sfile
                if (mode == "s" && nd == 0) mode = ""
            }
            depth = nd
        }
        END { if (ns != 1 || nh != 1 || depth != 0) exit 1 }
    ' "$src" || die "could not split $src into stream/http parts"
}

render_nginx_full() {
    local stream_out=$1 http_out=$2
    local full="$WORK_DIR/dacs.conf"
    local sed_args=()
    if [[ -n $COORDINATOR_OVERLAY_IP ]]; then
        sed_args+=(-e "/^[[:space:]]*#/!s/COORDINATOR_OVERLAY_IP/${COORDINATOR_OVERLAY_IP}/g")
    else
        # Coordinator not enrolled yet: BOOTSTRAP_DOMAIN is refused like any
        # unknown SNI name, and the placeholder upstream points nowhere.
        sed_args+=(
            -e 's/^\([[:space:]]*BOOTSTRAP_DOMAIN[[:space:]]\{1,\}\)coordinator_bootstrap;/\1reject;  # coordinator not enrolled yet/'
            -e 's/^\([[:space:]]*server[[:space:]]\{1,\}\)COORDINATOR_OVERLAY_IP:7444;/\1127.0.0.1:9;  # placeholder until the coordinator overlay IP is known/'
        )
    fi
    if [[ ! -e /proc/net/if_inet6 ]]; then
        sed_args+=(-e '/^[[:space:]]*listen[[:space:]]\{1,\}\[::\]/d')
    fi
    if ! nginx_has_http2_directive; then
        # nginx < 1.25.1 (Debian bookworm, Ubuntu noble) lacks `http2 on;`:
        # use the older `listen ... ssl http2;` form instead.
        sed_args+=(
            -e '/^[[:space:]]*http2[[:space:]]\{1,\}on;/d'
            -e 's/^\([[:space:]]*listen[[:space:]][^;#]*[[:space:]]ssl\);/\1 http2;/'
        )
    fi
    sed "${sed_args[@]}" \
        -e "/^[[:space:]]*#/!s/HS_DOMAIN/${HS_DOMAIN}/g" \
        -e "/^[[:space:]]*#/!s/BOOTSTRAP_DOMAIN/${BOOTSTRAP_DOMAIN}/g" \
        "$DEPLOY_DIR/nginx/dacs.conf" >"$full"
    assert_no_placeholders "$full" HS_DOMAIN BOOTSTRAP_DOMAIN COORDINATOR_OVERLAY_IP
    split_dacs_conf "$full" "$stream_out" "$http_out"
}

render_nginx_bootstrap() {
    local stream_out=$1 http_out=$2
    local v6listen=""
    [[ -e /proc/net/if_inet6 ]] && v6listen="    listen [::]:80;"
    cat >"$stream_out" <<EOF
# DACS public edge: temporary ACME bootstrap (no certificate for ${HS_DOMAIN}
# yet). infra-setup.sh replaces this with the stream SNI router.
EOF
    cat >"$http_out" <<EOF
# DACS temporary HTTP-only bootstrap for ACME HTTP-01; managed by infra-setup.sh.
server {
    listen 80;
${v6listen}
    server_name ${HS_DOMAIN} ${BOOTSTRAP_DOMAIN};

    location /.well-known/acme-challenge/ {
        root ${ACME_WEBROOT};
    }
    location / {
        return 503;
    }
}
EOF
}

ensure_nginx_includes() {
    grep -qE '^[[:space:]]*include[[:space:]]+/etc/nginx/modules-enabled/\*\.conf[[:space:]]*;' "$NGINX_CONF" ||
        die "$NGINX_CONF does not include /etc/nginx/modules-enabled/*.conf (needed to load the stream module)"
    grep -lqs 'ngx_stream_module' /etc/nginx/modules-enabled/*.conf ||
        die "the nginx stream module is not enabled (libnginx-mod-stream)"
    grep -qE '^[[:space:]]*include[[:space:]]+/etc/nginx/conf\.d/\*\.conf[[:space:]]*;' "$NGINX_CONF" ||
        die "$NGINX_CONF does not include /etc/nginx/conf.d/*.conf inside http {}"

    local top indented
    top="$(grep -cE '^include[[:space:]]+/etc/nginx/dacs\.conf[[:space:]]*;' "$NGINX_CONF" || true)"
    indented="$(grep -cE '^[[:space:]]+include[[:space:]]+/etc/nginx/dacs\.conf[[:space:]]*;' "$NGINX_CONF" || true)"
    if [[ $top != 1 || $indented != 0 ]]; then
        sed -i -E '/^[[:space:]]*include[[:space:]]+\/etc\/nginx\/dacs\.conf[[:space:]]*;/d' "$NGINX_CONF"
        sed -i -E '/^# DACS public edge \(stream SNI router\); managed by infra-setup\.sh$/d' "$NGINX_CONF"
        printf '\n# DACS public edge (stream SNI router); managed by infra-setup.sh\ninclude /etc/nginx/dacs.conf;\n' >>"$NGINX_CONF"
        info "added top-level 'include /etc/nginx/dacs.conf;' to $NGINX_CONF"
    fi
}

# Install rendered nginx files, validate with nginx -t (rolling back on
# failure), then reload.
apply_nginx() {
    local stream_src=$1 http_src=$2
    local backup="$WORK_DIR/nginx-backup"
    mkdir -p -- "$backup"
    cp -p -- "$NGINX_CONF" "$backup/nginx.conf"
    [[ -f $NGINX_STREAM_CONF ]] && cp -p -- "$NGINX_STREAM_CONF" "$backup/dacs.conf"
    [[ -f $NGINX_HTTP_CONF ]] && cp -p -- "$NGINX_HTTP_CONF" "$backup/dacs-http.conf"

    local changed=0
    if install_file "$stream_src" "$NGINX_STREAM_CONF" 0644 root root; then changed=1; fi
    if install_file "$http_src" "$NGINX_HTTP_CONF" 0644 root root; then changed=1; fi
    if ! cmp -s "$NGINX_CONF" "$backup/nginx.conf"; then changed=1; fi
    ensure_nginx_includes
    if ! cmp -s "$NGINX_CONF" "$backup/nginx.conf"; then changed=1; fi

    if [[ -L /etc/nginx/sites-enabled/default ]]; then
        rm -f -- /etc/nginx/sites-enabled/default
        info "disabled the stock nginx default site"
        changed=1
    fi

    if ! nginx -t; then
        warn "nginx -t failed; restoring the previous configuration"
        cp -p -- "$backup/nginx.conf" "$NGINX_CONF"
        if [[ -f $backup/dacs.conf ]]; then cp -p -- "$backup/dacs.conf" "$NGINX_STREAM_CONF"; else rm -f -- "$NGINX_STREAM_CONF"; fi
        if [[ -f $backup/dacs-http.conf ]]; then cp -p -- "$backup/dacs-http.conf" "$NGINX_HTTP_CONF"; else rm -f -- "$NGINX_HTTP_CONF"; fi
        die "nginx configuration invalid"
    fi
    systemctl enable nginx >/dev/null 2>&1
    if ((changed)) || ! systemctl is-active --quiet nginx; then
        systemctl reload-or-restart nginx
        info "nginx reloaded"
    else
        info "nginx configuration unchanged"
    fi
}

setup_tls() {
    step 4 "TLS certificate and nginx public edge"
    install -d -m 0755 "$ACME_WEBROOT"

    local stream_r="$WORK_DIR/stream.conf" http_r="$WORK_DIR/http.conf"
    local live="/etc/letsencrypt/live/$HS_DOMAIN"

    if [[ ! -s $live/fullchain.pem || ! -s $live/privkey.pem ]]; then
        info "no certificate for $HS_DOMAIN yet: applying the HTTP-only ACME bootstrap"
        render_nginx_bootstrap "$stream_r" "$http_r"
        apply_nginx "$stream_r" "$http_r"

        local certbot_args=(certonly --webroot -w "$ACME_WEBROOT" -d "$HS_DOMAIN"
            --email "$ACME_EMAIL" --no-eff-email --keep-until-expiring)
        if [[ $ACME_AGREE_TOS == "yes" ]]; then
            certbot_args+=(--agree-tos --non-interactive)
        elif [[ ! -t 0 ]]; then
            die "certbot needs you to accept the Let's Encrypt terms: run interactively or set ACME_AGREE_TOS=yes"
        fi
        info "requesting a Let's Encrypt certificate for $HS_DOMAIN"
        certbot "${certbot_args[@]}" || die "certbot failed (check DNS and that tcp/80 is reachable from the internet)"
        [[ -s $live/fullchain.pem ]] || die "certbot finished but $live/fullchain.pem is missing"
    else
        info "certificate for $HS_DOMAIN present"
    fi

    install -d -m 0755 /etc/letsencrypt/renewal-hooks/deploy
    local hook="$WORK_DIR/reload-nginx"
    printf '#!/bin/sh\n# managed by DACS infra-setup.sh\nsystemctl reload nginx\n' >"$hook"
    install_file "$hook" /etc/letsencrypt/renewal-hooks/deploy/dacs-reload-nginx 0755 root root || true

    render_nginx_full "$stream_r" "$http_r"
    apply_nginx "$stream_r" "$http_r"
    if [[ -n $COORDINATOR_OVERLAY_IP ]]; then
        info "bootstrap relay: $BOOTSTRAP_DOMAIN -> $COORDINATOR_OVERLAY_IP:7444 (TLS passthrough)"
    else
        info "bootstrap relay: $BOOTSTRAP_DOMAIN rejected until COORDINATOR_OVERLAY_IP is set"
    fi
}

# ---------------------------------------------------------------------------
# 5. headscale objects
# ---------------------------------------------------------------------------

setup_headscale_objects() {
    step 5 "Headscale user and API key"

    if headscale users list -o json 2>/dev/null |
        jq -e --arg u "$HS_ADMIN_USER" 'any(.[]?; .name == $u)' >/dev/null; then
        info "user $HS_ADMIN_USER exists"
    else
        headscale users create "$HS_ADMIN_USER" >/dev/null
        info "created user $HS_ADMIN_USER"
    fi

    ensure_netadmin_user
    if [[ -e $HS_API_KEY_FILE ]]; then
        info "API key file $HS_API_KEY_FILE present (left unchanged)"
    else
        local raw key
        raw="$(headscale apikeys create --expiration "$HS_API_KEY_EXPIRATION" -o json)"
        key="$(jq -r 'if type == "string" then . else (.key // .api_key // empty) end' <<<"$raw" 2>/dev/null || true)"
        [[ -n $key ]] || key="$(printf '%s' "$raw" | tr -d '[:space:]"')"
        unset raw
        [[ -n $key && $key != "null" ]] || die "could not parse the API key from 'headscale apikeys create'"
        (
            umask 077
            printf '%s\n' "$key" >"$HS_API_KEY_FILE.tmp"
        )
        unset key
        chown "$NETADMIN_USER:$NETADMIN_USER" -- "$HS_API_KEY_FILE.tmp"
        chmod 0600 -- "$HS_API_KEY_FILE.tmp"
        mv -f -- "$HS_API_KEY_FILE.tmp" "$HS_API_KEY_FILE"
        info "created Headscale API key (expires in $HS_API_KEY_EXPIRATION) -> $HS_API_KEY_FILE"
    fi
    chown "$NETADMIN_USER:$NETADMIN_USER" -- "$HS_API_KEY_FILE"
    chmod 0600 -- "$HS_API_KEY_FILE"
}

# ---------------------------------------------------------------------------
# 6. join the infra host to the overlay
# ---------------------------------------------------------------------------

install_tailscale() {
    if command -v tailscale >/dev/null && command -v tailscaled >/dev/null; then
        info "tailscale already installed ($(tailscale version 2>/dev/null | awk 'NR==1'))"
    else
        info "installing Tailscale from pkgs.tailscale.com ($OS_ID $OS_CODENAME)"
        install -d -m 0755 /usr/share/keyrings
        curl -fsSL --retry 3 -o /usr/share/keyrings/tailscale-archive-keyring.gpg \
            "https://pkgs.tailscale.com/stable/${OS_ID}/${OS_CODENAME}.noarmor.gpg"
        chmod 0644 /usr/share/keyrings/tailscale-archive-keyring.gpg
        curl -fsSL --retry 3 -o /etc/apt/sources.list.d/tailscale.list \
            "https://pkgs.tailscale.com/stable/${OS_ID}/${OS_CODENAME}.tailscale-keyring.list"
        apt-get update -q
        apt-get install -y -q tailscale
    fi
    systemctl enable --now tailscaled >/dev/null 2>&1
}

join_overlay() {
    step 6 "Joining this host to the overlay as tag:dacs-infra"
    install_tailscale

    local i state url
    for ((i = 0; i < 20; i++)); do
        state="$(tailscale_backend_state)"
        [[ -n $state && $state != "NoState" && $state != "Starting" ]] && break
        sleep 1
    done
    url="$(tailscale_control_url)"
    url="${url%/}"

    if [[ $state == "Running" && $url == "https://$HS_DOMAIN" ]]; then
        info "already joined to https://$HS_DOMAIN"
    else
        if [[ $state == "Running" ]]; then
            die "tailscale on this host is logged in to '${url}', not https://$HS_DOMAIN; run 'tailscale logout' first if that is intended"
        fi
        local keyfile
        keyfile="$(mktemp "$WORK_DIR/authkey.XXXXXX")"
        chmod 0600 -- "$keyfile"
        headscale preauthkeys create --tags tag:dacs-infra --expiration 10m -o json |
            jq -r '.key // empty' >"$keyfile"
        if [[ ! -s $keyfile ]] || grep -qx 'null' "$keyfile"; then
            rm -f -- "$keyfile"
            die "could not create a tag:dacs-infra pre-auth key"
        fi
        info "created a single-use 10-minute pre-auth key (not shown)"
        local rc=0
        tailscale up --reset \
            --login-server "https://$HS_DOMAIN" \
            --auth-key "file:$keyfile" \
            --hostname dacs-infra \
            --accept-dns=false \
            --timeout 90s || rc=$?
        rm -f -- "$keyfile"
        ((rc == 0)) || die "tailscale up failed (exit $rc); check 'journalctl -u headscale' and 'journalctl -u tailscaled'"
        info "joined https://$HS_DOMAIN as dacs-infra"
    fi

    for ((i = 0; i < 30; i++)); do
        INFRA_OVERLAY_IP="$(tailscale ip -4 2>/dev/null | awk 'NR==1' || true)"
        [[ -n $INFRA_OVERLAY_IP ]] && break
        sleep 1
    done
    is_overlay_ipv4 "$INFRA_OVERLAY_IP" || die "no overlay IPv4 from 'tailscale ip -4' (got '${INFRA_OVERLAY_IP}')"
    info "infra overlay IPv4: $INFRA_OVERLAY_IP"

    if [[ -n $COORDINATOR_OVERLAY_IP ]]; then
        if ! headscale nodes list -o json 2>/dev/null |
            jq -e --arg ip "$COORDINATOR_OVERLAY_IP" '[.. | strings] | index($ip) != null' >/dev/null; then
            warn "COORDINATOR_OVERLAY_IP $COORDINATOR_OVERLAY_IP is not assigned to any Headscale node yet"
        fi
    fi
}

# ---------------------------------------------------------------------------
# 7. dacs-netadmin
# ---------------------------------------------------------------------------

setup_netadmin() {
    step 7 "Installing dacs-netadmin"
    ensure_netadmin_user
    install -d -m 0750 -o "$NETADMIN_USER" -g "$NETADMIN_USER" "$NETADMIN_STATE"
    [[ -e $NETADMIN_STATE/audit.jsonl ]] && chown "$NETADMIN_USER:$NETADMIN_USER" -- "$NETADMIN_STATE/audit.jsonl"

    local changed=0
    if install_file "$NETADMIN_BIN" "$NETADMIN_BIN_DEST" 0755 root root; then changed=1; fi
    "$NETADMIN_BIN_DEST" --help >/dev/null || die "$NETADMIN_BIN_DEST does not run on this host (wrong target/architecture?)"

    # config.json
    local cfg="$WORK_DIR/netadmin-config.json"
    sed -e "s/INFRA_OVERLAY_IP/${INFRA_OVERLAY_IP}/g" \
        -e "s/HS_DOMAIN/${HS_DOMAIN}/g" \
        -e "s/BOOTSTRAP_DOMAIN/${BOOTSTRAP_DOMAIN}/g" \
        "$DEPLOY_DIR/netadmin/config.example.json" >"$cfg"
    assert_no_placeholders "$cfg" INFRA_OVERLAY_IP HS_DOMAIN BOOTSTRAP_DOMAIN
    jq -e . "$cfg" >/dev/null || die "rendered netadmin config is not valid JSON"
    [[ "$(jq -r .listen "$cfg")" == "${INFRA_OVERLAY_IP}:${NETADMIN_PORT}" ]] ||
        die "netadmin listen address is not ${INFRA_OVERLAY_IP}:${NETADMIN_PORT}"
    if install_file "$cfg" "$NETADMIN_ETC/config.json" 0640 root "$NETADMIN_USER"; then changed=1; fi

    # self-signed TLS
    install -d -m 0750 -o root -g "$NETADMIN_USER" "$NETADMIN_TLS_DIR"
    local cert="$NETADMIN_TLS_DIR/cert.pem" key="$NETADMIN_TLS_DIR/key.pem"
    if [[ ! -s $cert || ! -s $key ]]; then
        info "generating a self-signed TLS certificate for dacs-netadmin"
        (
            umask 077
            openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
                -days 825 -subj "/CN=dacs-netadmin" \
                -addext "subjectAltName=IP:${INFRA_OVERLAY_IP},DNS:dacs-infra.dacs.internal,DNS:dacs-infra" \
                -addext "extendedKeyUsage=serverAuth" \
                -keyout "$key.tmp" -out "$cert.tmp" 2>/dev/null
        ) || die "openssl failed to create the netadmin certificate"
        mv -f -- "$key.tmp" "$key"
        mv -f -- "$cert.tmp" "$cert"
        changed=1
    elif ! openssl x509 -in "$cert" -noout -ext subjectAltName 2>/dev/null | grep -q "IP Address:${INFRA_OVERLAY_IP}\$\|IP Address:${INFRA_OVERLAY_IP},"; then
        warn "$cert does not cover $INFRA_OVERLAY_IP; delete $cert and $key and re-run to regenerate (Person 1 must re-pin)"
    fi
    chown "$NETADMIN_USER:$NETADMIN_USER" -- "$key"
    chmod 0600 -- "$key"
    chown root:"$NETADMIN_USER" -- "$cert"
    chmod 0644 -- "$cert"
    NETADMIN_CERT_FP="$(openssl x509 -in "$cert" -noout -fingerprint -sha256 | sed 's/^[^=]*=//')"
    info "netadmin TLS certificate SHA-256: $NETADMIN_CERT_FP"

    # operation token
    local has_hash=0
    if [[ -f $TOKEN_HASH_FILE ]] && grep -qvE '^[[:space:]]*(#.*)?$' "$TOKEN_HASH_FILE"; then
        has_hash=1
    fi
    if ((has_hash)); then
        info "operation token hash present in $TOKEN_HASH_FILE (no new token issued)"
    else
        { : >/dev/tty; } 2>/dev/null ||
            die "a new operation token must be shown on an interactive terminal; re-run from a terminal (not via a pipe/cron)"
        local hash_tmp token hash
        hash_tmp="$(mktemp "$WORK_DIR/tokenhash.XXXXXX")"
        token="$("$NETADMIN_BIN_DEST" gen-token 2>"$hash_tmp")"
        hash="$(tr -d '[:space:]' <"$hash_tmp")"
        rm -f -- "$hash_tmp"
        [[ $token == dacs-op-* && $hash =~ ^[0-9a-f]{64}$ ]] || die "unexpected output from 'dacs-netadmin gen-token'"
        printf '%s\n' "$hash" >"$WORK_DIR/tokenhash"
        install -m 0600 -o "$NETADMIN_USER" -g "$NETADMIN_USER" "$WORK_DIR/tokenhash" "$TOKEN_HASH_FILE"
        {
            printf '\n'
            printf '    ************************************************************************\n'
            printf '    * NEW dacs-netadmin OPERATION TOKEN - shown ONCE, not stored anywhere. *\n'
            printf '    * Hand it to Person 1 (coordinator) over a private channel only.       *\n'
            printf '    * Only its SHA-256 hash is kept in %s\n' "$TOKEN_HASH_FILE"
            printf '    ************************************************************************\n'
            printf '\n    %s\n\n' "$token"
        } >/dev/tty
        unset token
        NEW_TOKEN_ISSUED=1
        changed=1
    fi
    chown "$NETADMIN_USER:$NETADMIN_USER" -- "$TOKEN_HASH_FILE"
    chmod 0600 -- "$TOKEN_HASH_FILE"

    # systemd unit
    if install_file "$DEPLOY_DIR/netadmin/dacs-netadmin.service" /etc/systemd/system/dacs-netadmin.service 0644 root root; then
        changed=1
    fi
    systemctl daemon-reload

    info "running dacs-netadmin check"
    runuser -u "$NETADMIN_USER" -- "$NETADMIN_BIN_DEST" check --config "$NETADMIN_ETC/config.json" ||
        die "dacs-netadmin check failed"

    systemctl enable dacs-netadmin >/dev/null 2>&1
    if ((changed)) || ! systemctl is-active --quiet dacs-netadmin; then
        systemctl restart dacs-netadmin
    fi
    local i
    for ((i = 0; i < 15; i++)); do
        systemctl is-active --quiet dacs-netadmin && break
        sleep 1
    done
    if ! systemctl is-active --quiet dacs-netadmin; then
        journalctl -u dacs-netadmin -n 40 --no-pager >&2 || true
        die "dacs-netadmin is not running"
    fi
    info "dacs-netadmin active on https://${INFRA_OVERLAY_IP}:${NETADMIN_PORT}"
}

# ---------------------------------------------------------------------------
# 8. summary
# ---------------------------------------------------------------------------

summary() {
    step 8 "Summary"
    local nginx_v certbot_v ts_v
    nginx_v="$(nginx -v 2>&1 | sed 's/^nginx version: //')"
    certbot_v="$(certbot --version 2>&1 | awk 'NR==1')"
    ts_v="$(tailscale version 2>/dev/null | awk 'NR==1')"
    cat <<EOF

  Versions
    headscale      $(headscale version 2>/dev/null | awk 'NR==1' || echo "$HEADSCALE_VERSION")
    tailscale      ${ts_v}
    nginx          ${nginx_v}
    certbot        ${certbot_v}
    dacs-netadmin  sha256 $(sha256sum "$NETADMIN_BIN_DEST" | awk '{print $1}')

  Endpoints
    Login server (Headscale + DERP)  https://${HS_DOMAIN}
    STUN                             ${PUBLIC_IPV4}:3478/udp${PUBLIC_IPV6:+, [${PUBLIC_IPV6}]:3478/udp}
    Coordinator bootstrap URL        https://${BOOTSTRAP_DOMAIN}  ($(
        if [[ -n $COORDINATOR_OVERLAY_IP ]]; then
            printf 'passthrough -> %s:7444' "$COORDINATOR_OVERLAY_IP"
        else
            printf 'REJECTING until COORDINATOR_OVERLAY_IP is set'
        fi
    ))
    dacs-netadmin (overlay only)     https://${INFRA_OVERLAY_IP}:${NETADMIN_PORT}
                                     (https://dacs-infra.dacs.internal:${NETADMIN_PORT})
    netadmin cert SHA-256 (Person 1 pins this)
                                     ${NETADMIN_CERT_FP}
    Operation token                  $(if ((NEW_TOKEN_ISSUED)); then echo "NEW token printed above - hand it to Person 1 privately"; else echo "unchanged (hash in ${TOKEN_HASH_FILE})"; fi)

  Firewall (cloud security group / host firewall)
    PUBLIC:  443/tcp, 80/tcp (ACME + redirect), 3478/udp (STUN)
    NEVER public: ${NETADMIN_PORT}/tcp (dacs-netadmin), 8080/9090/50443 (Headscale, loopback)
    Admin credentials (Headscale API key, CLI socket) stay on this host only.

EOF
    if [[ -z $COORDINATOR_OVERLAY_IP ]]; then
        cat <<EOF
  Next step: enroll the coordinator
    1. Write the coordinator's EnrollmentRequest (consent + approval, role
       coordinator; template: net/deploy/netadmin/enrollment-request.example.json)
       to ${NETADMIN_STATE}/coordinator-request.json
       (owner ${NETADMIN_USER}, mode 600).
    2. sudo -u ${NETADMIN_USER} ${NETADMIN_BIN_DEST} issue \\
           --request ${NETADMIN_STATE}/coordinator-request.json \\
           --out ${NETADMIN_STATE}/coordinator-grant.json
       and hand the grant to Person 1 privately (it holds a single-use key).
    3. After the coordinator joins, find its overlay IPv4
       (headscale nodes list) and re-run this script with
       COORDINATOR_OVERLAY_IP=<100.x.y.z> to activate https://${BOOTSTRAP_DOMAIN}.

EOF
    else
        printf '  Bootstrap relay active. Re-run this script whenever the coordinator overlay IP changes.\n\n'
    fi
}

main() {
    validate_inputs
    preflight
    install_packages
    install_headscale
    setup_tls
    setup_headscale_objects
    join_overlay
    setup_netadmin
    summary
}

main "$@"
