#!/usr/bin/env bash
# Stages of the local DACS network e2e run. Usage: scenario.sh <stage>
set -uo pipefail
E=/root/dacs-e2e
SRC=/root/dacs-src
HSBIN=/root/hs/headscale_0.29.7_linux_amd64
NET=$SRC/target/debug/dacs-net
NETADMIN=$SRC/target/debug/dacs-netadmin
LOGIN=https://hs.dacs.test:8443
PERSONAL=https://hs.personal.test:8444
INFRA_IP=$("$E/ts-infra" ip -4)
NA="https://$INFRA_IP:9443"
now() { date -u +%Y-%m-%dT%H:%M:%SZ; }
in_c() { ip netns exec dacs-c env DACS_TAILSCALE_BIN="$E/ts-c" "$@"; }
in_g() { ip netns exec dacs-g env DACS_TAILSCALE_BIN="$E/ts-g" "$@"; }
# Coordinator-side netadmin call over the overlay (operation token from a private header file).
na_c() { # method path [body-file]
  local args=(-sS --cacert "$E/pki/ca.pem" -H @"$E/secrets/auth-header" -X "$1" -w '\nHTTP %{http_code}\n')
  [ -n "${3:-}" ] && args+=(-H 'Content-Type: application/json' --data @"$3")
  ip netns exec dacs-c curl "${args[@]}" "$NA$2"
}
redact() { sed -E 's/"auth_key":"[^"]*"/"auth_key":"<redacted>"/g'; }
step() { printf '\n### %s\n' "$*"; }

consent() { # device host switch-json-or-null
  printf '{"device_id":"%s","accepted_host":"%s","accepted_by":"e2e-owner","accepted_at":"%s","disclosure_revision":"host-admin-access/1","tailnet_switch":%s}' \
    "$1" "$2" "$(now)" "$3"
}
request() { # device host kind-json roles-json consent-json
  printf '{"device_id":"%s","host_name":"%s","host_kind":%s,"roles":%s,"consent":%s,"approval":{"device_id":"%s","approved_by":"e2e-project-owner","approved_at":"%s","authorization_ref":"e2e-approval-%s"}}' \
    "$1" "$2" "$3" "$4" "$5" "$1" "$(now)" "$1"
}

case "${1:-}" in
coord)
  step "coordinator-first bootstrap: assess (no change), issue on infra host, join"
  in_c "$NET" assess "$LOGIN"; echo "exit $?"
  consent coord-e2e coordinator null >"$E/consent-c.json"
  request coord-e2e coordinator '{"kind":"physical"}' '["coordinator","worker"]' "$(cat "$E/consent-c.json")" >"$E/req-c.json"
  rm -f "$E/secrets/grant-c.json"
  "$NETADMIN" issue --config "$E/netadmin.json" --request "$E/req-c.json" --out "$E/secrets/grant-c.json"; echo "exit $?"
  stat -c 'grant file mode %a' "$E/secrets/grant-c.json"
  in_c "$NET" join --grant "$E/secrets/grant-c.json" --consent "$E/consent-c.json"; echo "exit $?"
  rm -f "$E/secrets/grant-c.json"
  ( umask 077; printf 'Authorization: Bearer %s\n' "$(cat "$E/secrets/op-token")" >"$E/secrets/auth-header" )
  step "coordinator reaches netadmin over the overlay with TLS"
  na_c GET /v1/health
  na_c GET /v1/contract | head -c 400; echo
  ;;
personal)
  step "compile guest is already logged in to the owner's personal control server"
  "$HSBIN" -c "$E/hsA.yaml" users create owner >/dev/null 2>&1 || true
  uid=$("$HSBIN" -c "$E/hsA.yaml" users list -o json | jq -r '.[] | select(.name=="owner") | .id')
  ( umask 077; "$HSBIN" -c "$E/hsA.yaml" preauthkeys create -u "$uid" --expiration 10m -o json | jq -r .key >"$E/secrets/personal.key" )
  "$E/ts-g" up --login-server "$PERSONAL" --auth-key "file:$E/secrets/personal.key" --hostname owner-linux-vm --accept-dns=false
  rm -f "$E/secrets/personal.key"
  in_g "$NET" status; echo "exit $?"
  ;;
assess)
  step "invitation review: assess DACS login server (read-only)"
  in_g "$NET" assess "$LOGIN"; echo "exit $?"
  in_g "$NET" status | jq -c '{state, control_url, backend_state}'
  ;;
enroll-guest)
  step "coordinator requests guest enrollment over the overlay"
  sw=$(printf '{"previous_control_url":"%s","previous_tailnet":null,"accepted_at":"%s"}' "$PERSONAL" "$(now)")
  consent guest-e2e compile-guest "$sw" >"$E/consent-g.json"
  consent guest-e2e compile-guest null >"$E/consent-g-noswitch.json"
  wrong=$(printf '{"previous_control_url":"https://other.example","previous_tailnet":null,"accepted_at":"%s"}' "$(now)")
  consent guest-e2e compile-guest "$wrong" >"$E/consent-g-wrong.json"
  request guest-e2e compile-guest '{"kind":"guest","physical_host":"coord-e2e"}' '["compile_scheduler","compile_server"]' \
    "$(cat "$E/consent-g.json")" >"$E/req-g.json"
  ( umask 077; na_c POST /v1/enrollments "$E/req-g.json" >"$E/secrets/enroll-g.out" )
  redact <"$E/secrets/enroll-g.out"
  ( umask 077; head -n1 "$E/secrets/enroll-g.out" >"$E/secrets/grant-g.json" )
  step "worker attempting infra role is refused"
  request bad-e2e badhost '{"kind":"physical"}' '["infra"]' "$(consent bad-e2e badhost null)" >"$E/req-bad.json"
  na_c POST /v1/enrollments "$E/req-bad.json"
  ;;
join-guest)
  step "join without tailnet-switch consent (must refuse, no change)"
  in_g "$NET" join --grant "$E/secrets/grant-g.json" --consent "$E/consent-g-noswitch.json"; echo "exit $?"
  in_g "$NET" status | jq -c '{state, control_url, backend_state}'
  step "join with switch consent for a different previous server (must refuse)"
  in_g "$NET" join --grant "$E/secrets/grant-g.json" --consent "$E/consent-g-wrong.json"; echo "exit $?"
  in_g "$NET" status | jq -c '{state, control_url, backend_state}'
  step "join with explicit switch consent"
  in_g "$NET" join --grant "$E/secrets/grant-g.json" --consent "$E/consent-g.json"; echo "exit $?"
  rm -f "$E/secrets/grant-g.json"
  "$E/ts-g" switch --list
  ;;
reach)
  G_IP=$("$E/ts-g" ip -4); C_IP=$("$E/ts-c" ip -4)
  echo "coordinator $C_IP  guest $G_IP  infra $INFRA_IP"
  step "membership and compiler endpoints (coordinator -> netadmin)"
  printf '{"device_id":"guest-e2e","host_name":"compile-guest","auth_key_id":%s,"node_id":null}' "$(head -n1 "$E/secrets/enroll-g.out" | jq .auth_key_id)" >"$E/query-g.json"
  na_c POST /v1/memberships/query "$E/query-g.json" | tee "$E/membership-g.json"
  na_c GET /v1/compiler-endpoints | tee "$E/endpoints.raw"
  head -n1 "$E/endpoints.raw" >"$E/endpoints.json"
  step "stand-in compiler listeners on the guest (sccache ports), sink on coordinator"
  pkill -f 'socat -u TCP-LISTEN' || true
  setsid -f ip netns exec dacs-g socat -u TCP-LISTEN:10600,bind=$G_IP,reuseaddr,fork OPEN:/dev/null </dev/null >/dev/null 2>&1
  setsid -f ip netns exec dacs-g socat -u TCP-LISTEN:10501,bind=$G_IP,reuseaddr,fork OPEN:/dev/null </dev/null >/dev/null 2>&1
  setsid -f ip netns exec dacs-c socat -u TCP-LISTEN:10501,bind=$C_IP,reuseaddr,fork OPEN:/dev/null </dev/null >/dev/null 2>&1
  sleep 1
  step "compiler-check from the coordinator (ACL: coordinator -> scheduler/server)"
  in_c "$NET" compiler-check --endpoints "$E/endpoints.json"; echo "exit $?"
  step "ACL default deny: guest -> coordinator:10501, guest -> infra:9443 (must fail)"
  in_g "$NET" probe "$C_IP:10501" "$INFRA_IP:9443" --timeout-ms 3000; echo "exit $?"
  step "compiler listeners bound to the overlay IP are not reachable on the underlay (must fail)"
  in_c "$NET" probe "10.99.0.12:10501" --timeout-ms 2000 | jq -c '.[]'; echo "exit $?"
  ;;
transfer)  # transfer <label>
  G_IP=$("$E/ts-g" ip -4)
  step "path + transfer: ${2:-direct}"
  in_c "$NET" ping compile-guest --count 5; echo "exit $?"
  in_c "$NET" peers >"$E/peers-before.json"
  s=$(date +%s.%N)
  head -c 67108864 /dev/zero | ip netns exec dacs-c socat -u - "TCP:$G_IP:10501"
  e=$(date +%s.%N)
  sleep 1
  in_c "$NET" peers >"$E/peers-after.json"
  awk -v s="$s" -v e="$e" 'BEGIN{d=e-s; printf "64 MiB in %.2f s = %.1f MiB/s\n", d, 64/d}'
  in_c "$NET" traffic --before "$E/peers-before.json" --after "$E/peers-after.json" | jq -c '.[]? // .'
  in_c "$NET" compiler-check --endpoints "$E/endpoints.json" | jq -c '.'
  ;;
block-udp)
  step "block direct UDP between coordinator and guest (STUN/DERP to infra still allowed)"
  for ns in dacs-c dacs-g; do
    peer=$([ $ns = dacs-c ] && echo 10.99.0.12 || echo 10.99.0.11)
    ip netns exec $ns iptables -I OUTPUT -p udp -d "$peer" -j DROP
    ip netns exec $ns iptables -I INPUT -p udp -s "$peer" -j DROP
  done
  ;;
unblock-udp)
  for ns in dacs-c dacs-g; do
    peer=$([ $ns = dacs-c ] && echo 10.99.0.12 || echo 10.99.0.11)
    ip netns exec $ns iptables -D OUTPUT -p udp -d "$peer" -j DROP
    ip netns exec $ns iptables -D INPUT -p udp -s "$peer" -j DROP
  done
  ;;
revoke)
  step "revoke the guest (coordinator -> netadmin)"
  printf '{"device_id":"guest-e2e","auth_key_id":null,"node_id":%s,"requested_by":"e2e-project-owner","reason":"e2e revocation"}' \
    "$(jq -r '.state.node_id' <(head -n1 "$E/membership-g.json"))" >"$E/revoke-g.json"
  cat "$E/revoke-g.json"; echo
  na_c POST /v1/revocations "$E/revoke-g.json"
  na_c POST /v1/memberships/query "$E/query-g.json"
  sleep 5
  step "guest after revocation"
  in_g "$NET" status | jq -c '{state, control_url, backend_state}'
  in_c "$NET" compiler-check --endpoints "$E/endpoints.json" --timeout-ms 3000 | jq -c '.'; echo "exit $?"
  step "reconnect attempt: restart guest tailscaled with its old state"
  pkill -f "statedir=$E/state-g" ; sleep 2
  setsid -f ip netns exec dacs-g env TS_NO_LOGS_NO_SUPPORT=true /root/ts/tailscale_1.104.7_amd64/tailscaled \
    --statedir="$E/state-g" --socket="$E/ts-g.sock" --tun=tailscale0 --port=41641 </dev/null >>"$E/logs/tsd-g.log" 2>&1
  sleep 8
  in_g "$NET" status | jq -c '{state, control_url, backend_state}'
  "$E/ts-g" switch --list
  in_g "$NET" assess "$LOGIN" | jq -c '{already_dacs, requires_switch_consent, previous_control_url, client: .client.state}'
  ;;
rejoin)
  step "fresh approved enrollment after revocation, then rejoin"
  consent guest-e2e compile-guest null >"$E/consent-g2.json"
  request guest-e2e compile-guest '{"kind":"guest","physical_host":"coord-e2e"}' '["compile_scheduler","compile_server"]' \
    "$(cat "$E/consent-g2.json")" >"$E/req-g2.json"
  ( umask 077; na_c POST /v1/enrollments "$E/req-g2.json" >"$E/secrets/enroll-g2.out" )
  redact <"$E/secrets/enroll-g2.out"
  ( umask 077; head -n1 "$E/secrets/enroll-g2.out" >"$E/secrets/grant-g2.json" )
  in_g "$NET" join --grant "$E/secrets/grant-g2.json" --consent "$E/consent-g2.json"; echo "exit $?"
  rm -f "$E/secrets/grant-g2.json"
  sleep 3
  in_c "$NET" compiler-check --endpoints <(na_c GET /v1/compiler-endpoints | head -n1) | jq -c '.'
  ;;
*) echo "unknown stage"; exit 64 ;;
esac
