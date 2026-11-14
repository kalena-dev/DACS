# Shared helpers for the DACS single-host failure scenarios. Source, do not run.
#
# Same rules as ../run.sh: real `dacs serve`, real `dacs worker run`, real TLS,
# SQLite coordinator, bare Git integration repository, worker journals and
# supervised process groups. FAKE (labelled everywhere): the OMP executable is
# ../fake-omp; it never contacts a model provider. Every scenario runs with an
# isolated HOME and GIT_CONFIG_NOSYSTEM=1, loopback ports 27500-27599 only, and
# stops every process it started on exit.
#
# A scenario script calls: scenario_init <name> <port base 27500..27590, step 10>
# then setup_coordinator, enroll_worker ..., start_worker ..., and ends with finish.
set -euo pipefail

SCEN_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
E2E_DIR=$(cd "$SCEN_DIR/.." && pwd)
REPO=$(cd "$E2E_DIR/../../.." && pwd)
FAKE_OMP="$E2E_DIR/fake-omp"
TCP_PROXY="$SCEN_DIR/tcp-proxy"

for tool in git jq curl python3 openssl sqlite3 perl; do
  command -v "$tool" >/dev/null || { echo "missing prerequisite: $tool" >&2; exit 2; }
done

scenario_init() { # scenario_init <name> <port base>
  SCENARIO=$1
  local base=$2
  [[ "$base" =~ ^275[0-9]0$ ]] || { echo "port base must be 27500..27590 (step 10)" >&2; exit 2; }
  PC=$base; PB=$((base + 1)); PX=$((base + 2))
  if [[ -z "${DACS_BIN:-}" ]]; then
    (cd "$REPO" && cargo build -q -p dacs)
    DACS_BIN="$REPO/target/debug/dacs"
  fi
  DACS=$(cd "$(dirname "$DACS_BIN")" && pwd)/$(basename "$DACS_BIN")
  RUN=${DACS_E2E_RUN_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/dacs-$SCENARIO.XXXXXX")}
  mkdir -p "$RUN"
  RUN=$(cd "$RUN" && pwd -P)
  mkdir -p "$RUN"/{logs,evidence,secrets,home,workers,gates}
  chmod 700 "$RUN/secrets" "$RUN/home"
  export HOME="$RUN/home"
  export GIT_CONFIG_NOSYSTEM=1
  unset OMP_PROFILE PI_CODING_AGENT_DIR || true
  CONTROL="https://127.0.0.1:$PC"
  BOOTSTRAP="https://127.0.0.1:$PB"
  CA="$RUN/tls/server.pem"
  COORD_DATA="$RUN/coordinator"
  ADMIN_TOKEN="$COORD_DATA/admin.token"
  GATES="$RUN/evidence/gates.tsv"
  : >"$GATES"
  COORD_PID=""
  PROXY_PID=""
  trap cleanup EXIT
  log "scenario $SCENARIO; run directory $RUN; $("$DACS" --version); code $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null || echo unknown)"
}

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RUN/logs/harness.log" >&2; }
gate() { # gate <passed|failed|not-run|blocked> <name> <detail>
  printf '%s\t%s\t%s\n' "$2" "$1" "$3" >>"$GATES"
  log "GATE [$1] $2 -- $3"
}
die() { gate failed "$1" "$2"; exit 1; }
check() { # check <name> <detail> <command...>: gate passed if the command succeeds
  local name=$1 detail=$2; shift 2
  if "$@"; then gate passed "$name" "$detail"; else gate failed "$name" "$detail"; fi
}

cleanup() {
  set +e
  local f pid pgid
  for f in "$RUN"/workers/*/pid; do
    [[ -f "$f" ]] || continue
    pid=$(cat "$f")
    if kill -0 "$pid" 2>/dev/null; then kill -INT "$pid"; sleep 2; kill -KILL "$pid" 2>/dev/null; fi
  done
  # Any fake OMP still alive under this run (orphaned by a drill or a harness failure).
  for f in "$RUN"/gates/*.json "$RUN"/gates/*.waiting; do
    [[ -f "$f" ]] || continue
    pgid=$(jq -r '.fake_omp_pgid // empty' "$f" 2>/dev/null)
    [[ -n "$pgid" ]] && pgroup "$pgid" | grep -q fake-omp && kill -KILL -- "-$pgid"
  done
  pkill -KILL -f -- "--session-dir $RUN/" 2>/dev/null
  [[ -n "$PROXY_PID" ]] && kill -KILL "$PROXY_PID" 2>/dev/null
  if [[ -n "$COORD_PID" ]] && kill -0 "$COORD_PID" 2>/dev/null; then
    kill -INT "$COORD_PID"; sleep 2; kill -KILL "$COORD_PID" 2>/dev/null
  fi
  # Publication fault-injection hooks may have left a Git child behind.
  touch "$RUN/gates/hook-release" 2>/dev/null
  pkill -KILL -f -- "$RUN/coordinator/source" 2>/dev/null
  log "run directory: $RUN"
  if [[ -s "$GATES" ]]; then
    printf '\n%-72s %s\n' "GATE ($SCENARIO)" RESULT >&2
    awk -F'\t' '{printf "%-72s %s\n", $1, $2}' "$GATES" >&2
    { printf '| Gate | Result | Observation |\n|---|---|---|\n'
      awk -F'\t' '{gsub(/\|/,"\\|",$3); printf "| %s | %s | %s |\n", $1, $2, $3}' "$GATES"; } >"$RUN/evidence/gates.md"
  fi
}

finish() {
  log "scenario $SCENARIO finished"
  if grep -q $'\tfailed\t' "$GATES"; then exit 1; fi
  exit 0
}

# --- coordinator -----------------------------------------------------------
admin_header() { printf 'Authorization: Bearer %s\n' "$(cat "$ADMIN_TOKEN")" >"$RUN/secrets/admin.header"; }
api() { # api <METHOD> <url> [json-body-file]; body to stdout, HTTP code to $RUN/evidence/last-http-code
  local args=(-sS --cacert "$CA" -H @"$RUN/secrets/admin.header" -X "$1" "$2" -w '%{stderr}%{http_code}')
  [[ $# -ge 3 ]] && args+=(-H 'Content-Type: application/json' --data-binary @"$3")
  curl "${args[@]}" 2>"$RUN/evidence/last-http-code"
}
request() { # request <name> <command-json>; prints CommandResponse or error body; returns dacs exit code
  printf '%s' "$2" >"$RUN/evidence/request-$1.json"
  local rc=0
  "$DACS" request --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" \
    --file "$RUN/evidence/request-$1.json" >"$RUN/evidence/response-$1.json" 2>"$RUN/evidence/response-$1.stderr" || rc=$?
  cat "$RUN/evidence/response-$1.json" "$RUN/evidence/response-$1.stderr"
  return $rc
}
snapshot() { "$DACS" snapshot --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" >"$RUN/evidence/snapshot.json"; }
save_snapshot() { snapshot; cp "$RUN/evidence/snapshot.json" "$RUN/evidence/snapshot-$1.json"; }
sj() { jq -r "$@" "$RUN/evidence/snapshot.json"; }
wait_for() { # wait_for <seconds> <description> <jq boolean filter over snapshot>
  local deadline=$((SECONDS + $1))
  while ((SECONDS < deadline)); do
    if snapshot 2>/dev/null && [[ "$(jq -r "$3" "$RUN/evidence/snapshot.json")" == "true" ]]; then return 0; fi
    sleep 0.5
  done
  log "timed out waiting for: $2"
  return 1
}
task_json() { echo "[.tasks[]|select(.id==\"$1\")][0]"; }
attempt_json() { echo "[.attempts[]|select(.id==\"$1\")][0]"; }
worker_json() { echo "[.workers[]|select(.id==\"$(wid "$1")\")][0]"; }

start_coordinator() { # start_coordinator <label>
  "$DACS" serve --listen "127.0.0.1:$PC" --bootstrap-listen "127.0.0.1:$PB" \
    --public-url "$CONTROL" --host-name dacs-e2e-coordinator --data-dir "$COORD_DATA" \
    --local-setup --tls-cert "$CA" --tls-key "$RUN/tls/server.key" >"$RUN/logs/coordinator-$1.log" 2>&1 &
  COORD_PID=$!
  echo "$COORD_PID" >"$RUN/evidence/coordinator-$1.pid"
  local _
  for _ in $(seq 1 60); do
    curl -sS --cacert "$CA" "$CONTROL/health" >/dev/null 2>&1 && { log "coordinator $1 up (pid $COORD_PID)"; return 0; }
    kill -0 "$COORD_PID" 2>/dev/null || die "coordinator start ($1)" "dacs serve exited: $(tail -3 "$RUN/logs/coordinator-$1.log")"
    sleep 0.5
  done
  die "coordinator start ($1)" "no /health"
}
kill9_coordinator() {
  log "kill -9 coordinator pid $COORD_PID"
  kill -KILL "$COORD_PID"; wait "$COORD_PID" 2>/dev/null || true
  COORD_PID=""
}

setup_coordinator() { # TLS, coordinator, project, seed commit, invitation
  "$DACS" tls --hostname localhost --hostname 127.0.0.1 --output-dir "$RUN/tls" >"$RUN/evidence/tls.json"
  start_coordinator 1
  admin_header
  request create-project '{"type":"create_project","name":"single-host-failures","target_branch":"main","max_tasks":50,"max_depth":2}' >/dev/null
  PROJECT=$(jq -r .result.id "$RUN/evidence/response-create-project.json")
  local seed="$RUN/seed-src"
  git init -q -b main "$seed"
  printf '# DACS single-host failure scenarios fixture\n' >"$seed/README.md"
  printf 'seed\n' >"$seed/hello.txt"
  git -C "$seed" add README.md hello.txt
  GIT_AUTHOR_DATE=2026-11-14T00:00:00+0000 GIT_COMMITTER_DATE=2026-11-14T00:00:00+0000 \
    git -C "$seed" -c user.name="DACS e2e seed" -c user.email=seed@dacs.invalid -c commit.gpgsign=false \
    commit -q -m "Seed single-host failure scenarios fixture"
  SEED_SHA=$(git -C "$seed" rev-parse HEAD)
  git -C "$seed" bundle create -q "$RUN/seed.bundle" main
  "$DACS" source import --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" \
    --project "$PROJECT" --file "$RUN/seed.bundle" --seed >"$RUN/evidence/source-import.json"
  [[ "$(jq -r .target_head "$RUN/evidence/source-import.json")" == "$SEED_SHA" ]] \
    || die "project seeded" "$(cat "$RUN/evidence/source-import.json")"
  BARE="$COORD_DATA/source/projects/$PROJECT/repository.git"
  request invitation "{\"type\":\"create_invitation\",\"project_id\":\"$PROJECT\",\"host_name\":\"dacs-e2e-coordinator\",\"expires_in_seconds\":7200}" >/dev/null
  ( umask 077; jq -r .result.token "$RUN/evidence/response-invitation.json" >"$RUN/secrets/invitation.token" )
  jq '.result.token="<redacted>"' "$RUN/evidence/response-invitation.json" >"$RUN/evidence/response-invitation.redacted.json"
  rm -f "$RUN/evidence/response-invitation.json"
  log "project $PROJECT seeded at $SEED_SHA"
}
main_head() { git --git-dir "$BARE" rev-parse refs/heads/main; }

# --- workers ---------------------------------------------------------------
# Owner onboarding is blocked without a network contract (see ../../single-host-e2e.md),
# so workers use raw /api/enroll + approve_worker + the WorkerConfig schema, as run.sh.
wdir() { echo "$RUN/workers/$1"; }
wid() { cat "$RUN/workers/$1/id"; }
wpid() { cat "$RUN/workers/$1/pid" 2>/dev/null || true; }
wstate() { echo "$RUN/workers/$1/state"; }
enroll_worker() { # enroll_worker <name> <physical host id> <agent slots> [coordinator url]
  local name=$1 host=$2 slots=$3 url=${4:-$CONTROL} dir
  dir=$(wdir "$name"); mkdir -p "$dir"
  jq -n --arg n "e2e-$name" --arg host "$host" --arg arch "$(uname -m)" --argjson s "$slots" '{
    name:$n, physical_host_id:$host, os:"macos", architecture:$arch,
    capabilities:["git"], capacity:{agent_slots:$s,cpu_millis:4000,memory_mb:8192,disk_mb:20480},
    omp_version:"0.0.0-dacs-fake-omp", policy_revision:1, owner_consented:true,
    consent_host:"dacs-e2e-coordinator"}' >"$dir/registration.json"
  jq -n --arg t "$(cat "$RUN/secrets/invitation.token")" --slurpfile r "$dir/registration.json" \
    '{invitation_token:$t,registration:$r[0]}' >"$RUN/secrets/enroll-$name.json"
  curl -sS --fail-with-body --cacert "$CA" -H 'Content-Type: application/json' \
    --data-binary @"$RUN/secrets/enroll-$name.json" "$BOOTSTRAP/api/enroll" >"$RUN/secrets/enrollment-$name.json" \
    || die "enroll worker $name" "$(cat "$RUN/secrets/enrollment-$name.json")"
  ( umask 077; jq -r .device_token "$RUN/secrets/enrollment-$name.json" | tr -d '\n' >"$dir/device.token" )
  jq '.device_token="<redacted>"' "$RUN/secrets/enrollment-$name.json" >"$dir/enrollment.redacted.json"
  rm -f "$RUN/secrets/enrollment-$name.json"
  jq -r .worker.id "$dir/enrollment.redacted.json" >"$dir/id"
  printf 'Authorization: Bearer %s\n' "$(cat "$dir/device.token")" >"$RUN/secrets/device-$name.header"
  request "approve-$name" "{\"type\":\"approve_worker\",\"worker_id\":\"$(wid "$name")\"}" >/dev/null \
    || die "approve worker $name" "$(cat "$RUN/evidence/response-approve-$name.stderr")"
  jq -n --arg url "$url" --arg cred "$dir/device.token" --arg id "$(wid "$name")" --arg omp "$FAKE_OMP" \
    --arg data "$dir/state" --arg ws "$dir/workspaces" --arg ca "$CA" --slurpfile reg "$dir/enrollment.redacted.json" '{
    coordinator_url:$url, credential_file:$cred, worker_id:$id, registration:$reg[0].worker.registration,
    omp_executable:$omp, omp_version:$reg[0].worker.registration.omp_version, omp_profile:null,
    data_dir:$data, workspaces_dir:$ws, ca_certificate:$ca, poll_interval_ms:500}' >"$dir/worker.json"
  chmod 600 "$dir/worker.json"
  log "worker $name = $(wid "$name") (physical host $host, $slots agent slots, coordinator $url)"
}
start_worker() { # start_worker <name> <label>
  local dir; dir=$(wdir "$1")
  RUST_LOG=dacs=info,dacs_worker=info "$DACS" worker run --config "$dir/worker.json" >"$RUN/logs/worker-$1-$2.log" 2>&1 &
  echo $! >"$dir/pid"
  date +%s >"$dir/started-at"
  log "worker $1 run '$2' started (pid $!)"
}
wait_worker_ready() { # wait_worker_ready <name> [seconds]: fresh heartbeat + runtime status after start
  local since; since=$(cat "$(wdir "$1")/started-at")
  wait_for "${2:-90}" "worker $1 ready" "([.runtime_status[]|select(.worker_id==\"$(wid "$1")\")][0] | (.provider_authenticated == true and .observed_at >= $since)) and ($(worker_json "$1").draining == false) and ($(worker_json "$1").last_seen >= $since)"
}
kill9_worker() { # kill9_worker <name>
  local pid; pid=$(wpid "$1")
  log "kill -9 worker $1 pid $pid"
  kill -KILL "$pid"; wait "$pid" 2>/dev/null || true
  rm -f "$(wdir "$1")/pid"
}
drain_worker() { # drain_worker <name>: owner drain through the real local control, wait for exit
  local pid _; pid=$(wpid "$1")
  "$DACS" worker control --data-dir "$(wstate "$1")" --action drain >"$RUN/evidence/drain-$1.json"
  for _ in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
  if kill -0 "$pid" 2>/dev/null; then return 1; fi
  wait "$pid" 2>/dev/null || true
  rm -f "$(wdir "$1")/pid"
}
worker_status() { # worker_status <name> <label>
  "$DACS" worker status --data-dir "$(wstate "$1")" >"$RUN/evidence/worker-status-$1-$2.json" 2>&1 || true
  cat "$RUN/evidence/worker-status-$1-$2.json"
}
journal() { # journal <name> <sql>: read-only query of the worker's own journal
  sqlite3 -readonly -json "file:$(wstate "$1")/worker.sqlite?mode=ro" "$2"
}
pgroup() { ps -axo pid=,pgid=,ppid=,stat=,command= | awk -v g="$1" '$2==g'; }
alive() { kill -0 "$1" 2>/dev/null; }
fake_notes() { # fake_notes <worker> <attempt>: fake OMP trace notes for an attempt (all sessions)
  find "$(wstate "$1")/attempts/$2" -name fake-omp-trace.jsonl -exec cat {} + 2>/dev/null \
    | jq -c 'select(.dir=="note")|.frame' || true
}
worker_events() { # worker_events <attempt>: coordinator-accepted worker events for an attempt
  sj "[.events[]|select(.kind==\"worker_event\" and .payload.attempt_id==\"$1\")|{sequence:.payload.sequence,generation:.payload.generation,event:.payload.event}]"
}
device_post() { # device_post <worker> <label> <envelope-json>: POST /api/command with that worker's device token
  printf '%s' "$3" >"$RUN/evidence/device-$2.request.json"
  curl -sS --cacert "$CA" -H @"$RUN/secrets/device-$1.header" -H 'Content-Type: application/json' \
    --data-binary @"$RUN/evidence/device-$2.request.json" -o "$RUN/evidence/device-$2.response.json" \
    -w '%{http_code}' "$CONTROL/api/command"
}

# --- tasks and publication -------------------------------------------------
task_spec() { # task_spec <title> <source sha> <instructions> [effect] [lease s] [retries] [slots]
  jq -n --arg p "$PROJECT" --arg title "$1" --arg sha "$2" --arg ins "$3" --arg effect "${4:-workspace}" \
    --argjson lease "${5:-300}" --argjson retries "${6:-1}" --argjson slots "${7:-1}" '{type:"create_task",spec:{
    project_id:$p, title:$title, instructions:$ins, source_commit:$sha, execution:{kind:"agent"},
    dependencies:[], required_capabilities:["git"], resources:{agent_slots:$slots,cpu_millis:1000,memory_mb:512,disk_mb:256},
    effect_class:$effect, lease_seconds:$lease, max_runtime_seconds:1800, contract_revision:1, policy_revision:1, max_retries:$retries}}'
}
create_task() { # create_task <label> <spec json>; sets TASK
  request "$1" "$2" >/dev/null || die "$1 create_task" "$(cat "$RUN/evidence/response-$1.stderr")"
  TASK=$(jq -r .result.id "$RUN/evidence/response-$1.json")
}
admit() { # admit <label> <task> [worker name]; sets ATTEMPT GEN
  local w=null
  [[ -n "${3:-}" ]] && w="\"$(wid "$3")\""
  request "admit-$1" "{\"type\":\"admit_task\",\"task_id\":\"$2\",\"worker_id\":$w}" >/dev/null \
    || die "$1 admission" "$(cat "$RUN/evidence/response-admit-$1.stderr")"
  ATTEMPT=$(jq -r .result.id "$RUN/evidence/response-admit-$1.json")
  GEN=$(jq -r .result.generation "$RUN/evidence/response-admit-$1.json")
}
wait_gate_marker() { # wait_gate_marker <path> [seconds]
  local _
  for _ in $(seq 1 $((${2:-60} * 2))); do [[ -f "$1" ]] && return 0; sleep 0.5; done
  return 1
}
review_and_publish() { # review_and_publish <label> <task> <file> <text>; sets INTEGRATED, PUBLISH_STATE
  local label=$1 task=$2 file=$3 text=$4 cand isha passed=false
  snapshot
  cand=$(sj "[.candidates[]|select(.submission.task_id==\"$task\")][-1].id")
  api POST "$CONTROL/api/candidates/$cand/prepare" >"$RUN/evidence/$label-prepare.json"
  isha=$(jq -r .integration_sha "$RUN/evidence/$label-prepare.json")
  [[ "$isha" =~ ^[0-9a-f]{40}$ ]] || { PUBLISH_STATE="prepare refused: $(cat "$RUN/evidence/$label-prepare.json")"; INTEGRATED=""; return 1; }
  [[ "$(git --git-dir "$BARE" show "$isha:$file" 2>/dev/null)" == "$text" ]] && passed=true
  request "$label-review" "$(jq -n --arg c "$cand" --arg s "$isha" --arg cmd "git --git-dir <coordinator bare repo> show $isha:$file" --argjson ok $passed '{type:"review_candidate",candidate_id:$c,decision:{
    candidate_sha:$s, approve:$ok, reason:"failure-scenario harness: exact staged integration commit contains the expected file content",
    checks_run:[{name:"reviewer content check",command:$cmd,source_sha:$s,passed:$ok,artifact_digest:null}], skipped_checks:[]}}')" >/dev/null || true
  api POST "$CONTROL/api/candidates/$cand/publish" >"$RUN/evidence/$label-publish.json"
  PUBLISH_STATE=$(jq -r '.state // .message // .' "$RUN/evidence/$label-publish.json")
  snapshot
  INTEGRATED=$(sj "$(task_json "$task").integrated_sha")
  [[ "$PUBLISH_STATE" == published && "$INTEGRATED" == "$isha" && "$(main_head)" == "$isha" ]]
}
stage_and_review() { # stage_and_review <label> <task> <file> <text>; sets CAND ISHA (no publish)
  local label=$1 task=$2 file=$3 text=$4 passed=false
  snapshot
  CAND=$(sj "[.candidates[]|select(.submission.task_id==\"$task\")][-1].id")
  api POST "$CONTROL/api/candidates/$CAND/prepare" >"$RUN/evidence/$label-prepare.json"
  ISHA=$(jq -r .integration_sha "$RUN/evidence/$label-prepare.json")
  [[ "$ISHA" =~ ^[0-9a-f]{40}$ ]] || die "$label prepare" "$(cat "$RUN/evidence/$label-prepare.json")"
  [[ "$(git --git-dir "$BARE" show "$ISHA:$file" 2>/dev/null)" == "$text" ]] && passed=true
  request "$label-review" "$(jq -n --arg c "$CAND" --arg s "$ISHA" --arg cmd "git --git-dir <coordinator bare repo> show $ISHA:$file" --argjson ok $passed '{type:"review_candidate",candidate_id:$c,decision:{
    candidate_sha:$s, approve:$ok, reason:"failure-scenario harness: exact staged integration commit contains the expected file content",
    checks_run:[{name:"reviewer content check",command:$cmd,source_sha:$s,passed:$ok,artifact_digest:null}], skipped_checks:[]}}')" >/dev/null \
    || die "$label review" "$(cat "$RUN/evidence/response-$label-review.stderr")"
}

# --- publication-boundary fault injection ------------------------------------
# dacs-source runs Git with core.hooksPath=<project>/disabled-hooks (created empty).
# The harness, as the coordinator's owner, places a reference-transaction hook
# there so the real `update-ref` of a publication pauses (or fails) on request.
# Gate files in $RUN/gates: hook-armed (pause in "prepared", before the ref
# moves), hook-armed-committed (pause in "committed", after the ref moved),
# hook-abort (fail the prepared transaction), hook-release (continue).
install_publication_hook() {
  local hooks="$COORD_DATA/source/projects/$PROJECT/disabled-hooks"
  sed "s#@GATES@#$RUN/gates#" "$SCEN_DIR/reference-transaction.hook" >"$hooks/reference-transaction"
  chmod 755 "$hooks/reference-transaction"
  log "publication fault-injection hook installed at $hooks/reference-transaction"
}
hook_reset() { rm -f "$RUN"/gates/hook-*; }
