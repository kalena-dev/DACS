#!/usr/bin/env bash
# Local DACS network e2e harness (WSL, root). Throwaway; not a deployment script.
# Topology: root ns = "infra host" (Headscale B = DACS, Headscale A = owner's
# pre-existing personal control server, TLS fronts, tag:dacs-infra node, netadmin).
# netns dacs-c = coordinator PC, netns dacs-g = Linux compile guest.
set -euo pipefail
E=/root/dacs-e2e
SRC=/root/dacs-src
HSBIN=/root/hs/headscale_0.29.7_linux_amd64
TSDIR=/root/ts/tailscale_1.104.7_amd64
export TS_NO_LOGS_NO_SUPPORT=true
mkdir -p "$E"/{pki,hsA,logs,secrets}
chmod 700 "$E/secrets"
cd "$E"

log() { printf '== %s\n' "$*"; }

log "namespaces"
if ! ip link show br-dacs >/dev/null 2>&1; then
  ip link add br-dacs type bridge
  ip addr add 10.99.0.1/24 dev br-dacs
  ip link set br-dacs up
fi
for spec in "c 11" "g 12"; do
  set -- $spec; ns="dacs-$1"
  if ! ip netns list | grep -qw "$ns"; then
    ip netns add "$ns"
    ip link add "v-$1" type veth peer name eth0 netns "$ns"
    ip link set "v-$1" master br-dacs up
    ip -n "$ns" addr add "10.99.0.$2/24" dev eth0
    ip -n "$ns" link set eth0 up
    ip -n "$ns" link set lo up
  fi
done
# Per-namespace resolv.conf: otherwise tailscaled (accept-dns) in a netns
# rewrites the distro-wide /etc/resolv.conf.
for ns in dacs-c dacs-g; do
  mkdir -p "/etc/netns/$ns"
  [ -f "/etc/netns/$ns/resolv.conf" ] || cp /etc/resolv.conf "/etc/netns/$ns/resolv.conf"
done
grep -q 'hs.dacs.test' /etc/hosts || echo '10.99.0.1 hs.dacs.test hs.personal.test' >>/etc/hosts

log "test CA and Headscale front certificate"
cd "$E/pki"
if [ ! -f ca.pem ]; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout ca.key -out ca.pem \
    -days 2 -subj "/CN=DACS e2e throwaway CA" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout hs.key -out hs.csr \
    -subj "/CN=hs.dacs.test" 2>/dev/null
  printf 'subjectAltName=DNS:hs.dacs.test,DNS:hs.personal.test\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >hs.ext
  openssl x509 -req -in hs.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out hs.pem -days 2 -extfile hs.ext 2>/dev/null
  cp ca.pem /usr/local/share/ca-certificates/dacs-e2e-ca.crt
  update-ca-certificates >/dev/null
fi
chmod 600 ca.key hs.key
cd "$E"

log "Headscale configs"
cp "$SRC/net/deploy/headscale/policy.hujson" "$E/policy.hujson"
sed -e 's#^server_url: .*#server_url: https://hs.dacs.test:8443#' \
    -e 's#^listen_addr: .*#listen_addr: 127.0.0.1:8080#' \
    -e 's#stun_listen_addr: .*#stun_listen_addr: "10.99.0.1:3478"#' \
    -e 's#^    ipv4: .*#    ipv4: 10.99.0.1#' \
    -e "s#path: /root/hs/policy.hujson#path: $E/policy.hujson#" \
    /root/hs/config.yaml >"$E/hsB.yaml"
sed -e 's#^server_url: .*#server_url: https://hs.personal.test:8444#' \
    -e 's#^listen_addr: .*#listen_addr: 127.0.0.1:8081#' \
    -e 's#^metrics_listen_addr: .*#metrics_listen_addr: 127.0.0.1:9091#' \
    -e 's#^grpc_listen_addr: .*#grpc_listen_addr: 127.0.0.1:50444#' \
    -e "s#/root/hs/data/noise_private.key#$E/hsA/noise_private.key#" \
    -e "s#/root/hs/data/derp_server_private.key#$E/hsA/derp_server_private.key#" \
    -e "s#/root/hs/data/db.sqlite#$E/hsA/db.sqlite#" \
    -e 's#region_id: 999#region_id: 998#' \
    -e 's#region_code: "dacs"#region_code: "personal"#' \
    -e 's#region_name: .*#region_name: "Owner personal test DERP"#' \
    -e 's#stun_listen_addr: .*#stun_listen_addr: "10.99.0.1:3479"#' \
    -e 's#^    ipv4: .*#    ipv4: 10.99.0.1#' \
    -e 's#base_domain: dacs.internal#base_domain: personal.internal#' \
    -e "s#unix_socket: .*#unix_socket: $E/hsA/headscale.sock#" \
    -e 's#path: /root/hs/policy.hujson#path: ""#' \
    /root/hs/config.yaml >"$E/hsA.yaml"

log "start Headscale A/B and TLS fronts"
pkill -f headscale_0.29.7 || true
pkill -f 'socat OPENSSL-LISTEN' || true
sleep 1
setsid -f "$HSBIN" -c "$E/hsB.yaml" serve </dev/null >"$E/logs/hsB.log" 2>&1
setsid -f "$HSBIN" -c "$E/hsA.yaml" serve </dev/null >"$E/logs/hsA.log" 2>&1
setsid -f socat -d -d OPENSSL-LISTEN:8443,bind=10.99.0.1,reuseaddr,fork,cert="$E/pki/hs.pem",key="$E/pki/hs.key",verify=0 TCP:127.0.0.1:8080 </dev/null >>"$E/logs/frontB.log" 2>&1
setsid -f socat -d -d OPENSSL-LISTEN:8444,bind=10.99.0.1,reuseaddr,fork,cert="$E/pki/hs.pem",key="$E/pki/hs.key",verify=0 TCP:127.0.0.1:8081 </dev/null >>"$E/logs/frontA.log" 2>&1
for u in https://hs.dacs.test:8443/health https://hs.personal.test:8444/health; do
  for _ in $(seq 30); do curl -fsS "$u" >/dev/null 2>&1 && break; sleep 1; done
  printf '%s -> %s\n' "$u" "$(curl -fsS "$u")"
done

log "tailscaled: infra (root ns), coordinator (dacs-c), compile guest (dacs-g)"
pkill -f "$TSDIR/tailscaled" || true
sleep 1
start_tsd() { # name netns-or-empty tun
  local name=$1 ns=$2 tun=$3 pre=()
  [ -n "$ns" ] && pre=(ip netns exec "$ns")
  mkdir -p "$E/state-$name"
  nohup "${pre[@]}" env TS_NO_LOGS_NO_SUPPORT=true "$TSDIR/tailscaled" --statedir="$E/state-$name" \
    --socket="$E/ts-$name.sock" --tun="$tun" --port=41641 >"$E/logs/tsd-$name.log" 2>&1 &
  printf '#!/bin/sh\nexec %s/tailscale --socket=%s/ts-%s.sock "$@"\n' "$TSDIR" "$E" "$name" >"$E/ts-$name"
  chmod 755 "$E/ts-$name"
}
start_tsd infra "" ts-infra
start_tsd c dacs-c tailscale0
start_tsd g dacs-g tailscale0
sleep 3

log "infra node joins as tag:dacs-infra (admin-side key, infra host only)"
if ! "$E/ts-infra" status >/dev/null 2>&1; then
  ( umask 077; "$HSBIN" -c "$E/hsB.yaml" preauthkeys create --tags tag:dacs-infra --expiration 10m -o json \
      | jq -r '.key' >"$E/secrets/infra.key" )
  "$E/ts-infra" up --login-server https://hs.dacs.test:8443 --auth-key "file:$E/secrets/infra.key" \
    --hostname dacs-infra --accept-dns=false --accept-routes=false
  rm -f "$E/secrets/infra.key"
fi
INFRA_IP=$("$E/ts-infra" ip -4)
echo "infra overlay IP: $INFRA_IP"

log "netadmin TLS certificate for the infra overlay IP"
cd "$E/pki"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout netadmin.key -out netadmin.csr \
  -subj "/CN=dacs-netadmin" 2>/dev/null
printf 'subjectAltName=IP:%s\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' "$INFRA_IP" >netadmin.ext
openssl x509 -req -in netadmin.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out netadmin.pem -days 2 \
  -extfile netadmin.ext 2>/dev/null
chmod 600 netadmin.key
cd "$E"

log "netadmin config, Headscale API key and operation token"
install -m 600 /root/hs/apikey "$E/secrets/headscale-api-key"
if [ ! -f "$E/secrets/op-token" ]; then
  ( umask 077; "$SRC/target/debug/dacs-netadmin" gen-token >"$E/secrets/op-token" 2>"$E/secrets/op-token.sha256" )
fi
chmod 600 "$E/secrets/op-token" "$E/secrets/op-token.sha256"
cat >"$E/netadmin.json" <<JSON
{
  "listen": "$INFRA_IP:9443",
  "tls": { "cert_path": "$E/pki/netadmin.pem", "key_path": "$E/pki/netadmin.key" },
  "headscale_url": "http://127.0.0.1:8080",
  "headscale_api_key_path": "$E/secrets/headscale-api-key",
  "operation_token_hashes_path": "$E/secrets/op-token.sha256",
  "login_server": "https://hs.dacs.test:8443",
  "coordinator": {
    "bootstrap_url": "https://bootstrap.dacs.test",
    "overlay_url": "https://coordinator.dacs.internal:7443"
  },
  "key_ttl_minutes": 15,
  "audit_log_path": "$E/audit.jsonl",
  "compiler_ports": { "scheduler": 10600, "server": 10501 }
}
JSON
"$SRC/target/debug/dacs-netadmin" check --config "$E/netadmin.json"
pkill -f 'dacs-netadmin serve' || true
nohup "$SRC/target/debug/dacs-netadmin" serve --config "$E/netadmin.json" >"$E/logs/netadmin.log" 2>&1 &
sleep 2
curl -fsS --cacert "$E/pki/ca.pem" "https://$INFRA_IP:9443/v1/health"; echo
echo "setup complete"
