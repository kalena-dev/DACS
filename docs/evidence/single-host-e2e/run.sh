#!/usr/bin/env bash
# DACS single-host control-plane end-to-end harness.
#
# Real: dacs binary, TLS listeners (control + bootstrap), device-token auth,
# SQLite coordinator, bare Git integration repository, worker journal,
# supervised process groups, isolated workspace clones, kill -9 recovery.
# FAKE (labelled everywhere): the OMP executable is ./fake-omp. It never
# contacts a model provider; its "provider readiness" answer is local.
#
# Not a multi-PC proof, not a provider-backed run, no network enrollment.
#
# Usage: docs/evidence/single-host-e2e/run.sh            (builds target/debug/dacs)
#   DACS_BIN=/path/to/dacs      use an existing binary
#   DACS_E2E_RUN_DIR=/path      run directory (default: new mktemp dir)
#   DACS_E2E_CONTROL_PORT / DACS_E2E_BOOTSTRAP_PORT   loopback ports (27400-27499)
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
FAKE_OMP="$HERE/fake-omp"
PC=${DACS_E2E_CONTROL_PORT:-27443}
PB=${DACS_E2E_BOOTSTRAP_PORT:-27444}
for port in "$PC" "$PB"; do
  [[ "$port" =~ ^274[0-9][0-9]$ ]] || { echo "ports must be within 27400-27499" >&2; exit 2; }
done
for tool in git jq curl python3 openssl; do
  command -v "$tool" >/dev/null || { echo "missing prerequisite: $tool" >&2; exit 2; }
done

if [[ -z "${DACS_BIN:-}" ]]; then
  (cd "$REPO" && cargo build -q -p dacs)
  DACS_BIN="$REPO/target/debug/dacs"
fi
DACS=$(cd "$(dirname "$DACS_BIN")" && pwd)/$(basename "$DACS_BIN")

RUN=${DACS_E2E_RUN_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/dacs-e2e.XXXXXX")}
mkdir -p "$RUN"
RUN=$(cd "$RUN" && pwd -P)
mkdir -p "$RUN"/{logs,evidence,secrets,home,worker}
chmod 700 "$RUN/secrets" "$RUN/home"
# Hermetic owner profile: the worker, fake OMP and Git never see the real HOME
# (no ~/.omp lock files, no global gitconfig, no provider credentials).
export HOME="$RUN/home"
export GIT_CONFIG_NOSYSTEM=1
unset OMP_PROFILE PI_CODING_AGENT_DIR || true

CONTROL="https://127.0.0.1:$PC"
BOOTSTRAP="https://127.0.0.1:$PB"
CA="$RUN/tls/server.pem"
COORD_DATA="$RUN/coordinator"
ADMIN_TOKEN="$COORD_DATA/admin.token"
WORKER_STATE="$RUN/worker/state"
WORKER_CONFIG="$RUN/worker/worker.json"
DEVICE_TOKEN="$RUN/worker/device.token"
GATES="$RUN/evidence/gates.tsv"
: >"$GATES"
COORD_PID=""
WORKER_PID=""

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RUN/logs/harness.log" >&2; }
gate() { # gate <passed|failed|not-run|blocked> <name> <detail>
  printf '%s\t%s\t%s\n' "$2" "$1" "$3" >>"$GATES"
  log "GATE [$1] $2 -- $3"
}
die() { gate failed "$1" "$2"; exit 1; }

cleanup() {
  set +e
  if [[ -n "$WORKER_PID" ]] && kill -0 "$WORKER_PID" 2>/dev/null; then
    kill -INT "$WORKER_PID"; sleep 3; kill -KILL "$WORKER_PID" 2>/dev/null
  fi
  # Any fake OMP still alive under this run (crash drill or harness failure).
  for marker in "$RUN"/evidence/hold-*.json; do
    [[ -f "$marker" ]] || continue
    pgid=$(jq -r .fake_omp_pgid "$marker")
    if pgroup "$pgid" | grep -q fake-omp; then kill -KILL -- "-$pgid"; fi
  done
  pkill -KILL -f -- "--session-dir $RUN/" 2>/dev/null
  if [[ -n "$COORD_PID" ]] && kill -0 "$COORD_PID" 2>/dev/null; then
    kill -INT "$COORD_PID"; sleep 2; kill -KILL "$COORD_PID" 2>/dev/null
  fi
  log "run directory: $RUN"
  if [[ -s "$GATES" ]]; then
    printf '\n%-58s %s\n' GATE RESULT >&2
    awk -F'\t' '{printf "%-58s %s\n", $1, $2}' "$GATES" >&2
  fi
}
trap cleanup EXIT

# --- helpers -----------------------------------------------------------------
admin_header() { printf 'Authorization: Bearer %s\n' "$(cat "$ADMIN_TOKEN")" >"$RUN/secrets/admin.header"; }
api() { # api <METHOD> <url> [json-body-file]
  local args=(-sS --cacert "$CA" -H @"$RUN/secrets/admin.header" -X "$1" "$2")
  [[ $# -ge 3 ]] && args+=(-H 'Content-Type: application/json' --data-binary @"$3")
  curl "${args[@]}"
}
request() { # request <name> <command-json>; prints CommandResponse
  printf '%s' "$2" >"$RUN/evidence/request-$1.json"
  "$DACS" request --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" \
    --file "$RUN/evidence/request-$1.json" | tee "$RUN/evidence/response-$1.json"
}
snapshot() { "$DACS" snapshot --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" >"$RUN/evidence/snapshot.json"; }
wait_for() { # wait_for <seconds> <description> <jq boolean filter over snapshot>
  local deadline=$((SECONDS + $1))
  while ((SECONDS < deadline)); do
    snapshot
    if [[ "$(jq -r "$3" "$RUN/evidence/snapshot.json")" == "true" ]]; then return 0; fi
    sleep 1
  done
  log "timed out waiting for: $2"
  return 1
}
worker_status() { "$DACS" worker status --data-dir "$WORKER_STATE" >"$RUN/evidence/worker-status-$1.json" 2>&1 || true; cat "$RUN/evidence/worker-status-$1.json"; }
start_worker() { # start_worker <n>
  RUST_LOG=dacs=info,dacs_worker=info "$DACS" worker run --config "$WORKER_CONFIG" >"$RUN/logs/worker-$1.log" 2>&1 &
  WORKER_PID=$!
  echo "$WORKER_PID" >"$RUN/evidence/worker-$1.pid"
  log "worker run #$1 started (pid $WORKER_PID)"
}
bare_repo() { echo "$COORD_DATA/source/projects/$PROJECT/repository.git"; }
pgroup() { ps -axo pid=,pgid=,ppid=,stat=,command= | awk -v g="$1" '$2==g'; }

# --- 1. development TLS ------------------------------------------------------
log "harness run directory $RUN; dacs $("$DACS" --version)"
"$DACS" tls --hostname localhost --hostname 127.0.0.1 --output-dir "$RUN/tls" >"$RUN/evidence/tls.json"
openssl x509 -in "$CA" -noout -ext subjectAltName >"$RUN/evidence/tls-san.txt" 2>&1 || true
gate passed "dacs tls dev certificate" "$(jq -r .certificate_sha256 "$RUN/evidence/tls.json" | cut -c1-16)... SAN: $(tr -d '\n' <"$RUN/evidence/tls-san.txt" | sed 's/.*Name: *//')"

# --- 2. coordinator: control + bootstrap TLS listeners ----------------------
"$DACS" serve --listen "127.0.0.1:$PC" --bootstrap-listen "127.0.0.1:$PB" \
  --public-url "$CONTROL" --host-name dacs-e2e-coordinator --data-dir "$COORD_DATA" \
  --local-setup --tls-cert "$CA" --tls-key "$RUN/tls/server.key" >"$RUN/logs/coordinator.log" 2>&1 &
COORD_PID=$!
for _ in $(seq 1 60); do
  curl -sS --cacert "$CA" "$CONTROL/health" >"$RUN/evidence/health-control.json" 2>/dev/null && break
  kill -0 "$COORD_PID" 2>/dev/null || die "coordinator start" "dacs serve exited: $(tail -3 "$RUN/logs/coordinator.log")"
  sleep 0.5
done
curl -sS --cacert "$CA" "$BOOTSTRAP/health" >"$RUN/evidence/health-bootstrap.json" || die "bootstrap TLS listener" "no TLS health on $BOOTSTRAP"
gate passed "coordinator TLS control + bootstrap listeners" "control $(jq -c . "$RUN/evidence/health-control.json"); bootstrap healthy"
code=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CA" "$BOOTSTRAP/api/state")
if [[ "$code" == 404 ]]; then gate passed "bootstrap listener omits control routes" "GET $BOOTSTRAP/api/state -> 404"
else gate failed "bootstrap listener omits control routes" "GET /api/state on bootstrap -> $code"; fi
if curl -s -o /dev/null "$CONTROL/health" 2>/dev/null; then gate failed "TLS trust is explicit" "request without --cacert succeeded"
else gate passed "TLS trust is explicit" "request without the dev CA is refused by certificate verification"; fi
admin_header
noauth=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$CA" "$CONTROL/api/state")
[[ "$noauth" == 401 ]] && gate passed "unauthenticated state read refused" "GET /api/state without bearer -> 401" \
  || gate failed "unauthenticated state read refused" "GET /api/state without bearer -> $noauth"

# --- 3. project + seed integration repository --------------------------------
request create-project '{"type":"create_project","name":"single-host-e2e","target_branch":"main","max_tasks":10,"max_depth":2}' >/dev/null
PROJECT=$(jq -r .result.id "$RUN/evidence/response-create-project.json")
SEED_SRC="$RUN/seed-src"
git init -q -b main "$SEED_SRC"
printf '# DACS single-host e2e fixture\n\nSeeded by docs/evidence/single-host-e2e/run.sh.\n' >"$SEED_SRC/README.md"
printf 'seed\n' >"$SEED_SRC/hello.txt"
git -C "$SEED_SRC" add README.md hello.txt
GIT_AUTHOR_DATE=2026-11-14T00:00:00+0000 GIT_COMMITTER_DATE=2026-11-14T00:00:00+0000 \
  git -C "$SEED_SRC" -c user.name="DACS e2e seed" -c user.email=seed@dacs.invalid -c commit.gpgsign=false \
  commit -q -m "Seed single-host e2e fixture"
SEED_SHA=$(git -C "$SEED_SRC" rev-parse HEAD)
git -C "$SEED_SRC" bundle create -q "$RUN/seed.bundle" main
"$DACS" source import --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" \
  --project "$PROJECT" --file "$RUN/seed.bundle" --seed >"$RUN/evidence/source-import.json"
[[ "$(jq -r .target_head "$RUN/evidence/source-import.json")" == "$SEED_SHA" ]] \
  && gate passed "project created and target seeded" "project $PROJECT; refs/heads/main = $SEED_SHA" \
  || die "project created and target seeded" "$(cat "$RUN/evidence/source-import.json")"
BARE=$(bare_repo)
echo "$BARE" >"$RUN/evidence/bare-repo-path.txt"

# --- 4. invitation; onboarding path; enrollment via bootstrap listener -------
request invitation "{\"type\":\"create_invitation\",\"project_id\":\"$PROJECT\",\"host_name\":\"dacs-e2e-coordinator\",\"expires_in_seconds\":3600}" >/dev/null
( umask 077; jq -r .result.token "$RUN/evidence/response-invitation.json" >"$RUN/secrets/invitation.token" )
jq '.result.token="<redacted>"' "$RUN/evidence/response-invitation.json" >"$RUN/evidence/response-invitation.redacted.json"
rm -f "$RUN/evidence/response-invitation.json"
INVITE=$(cat "$RUN/secrets/invitation.token")

# The supported owner path is /api/onboarding (inspect -> register -> configure_worker).
jq -n --arg e "$CONTROL" --arg t "$INVITE" --arg ca "$CA" '{endpoint:$e,invitation_token:$t,ca_file:$ca}' >"$RUN/secrets/inspect.json"
api POST "$CONTROL/api/onboarding/inspect" "$RUN/secrets/inspect.json" >"$RUN/evidence/onboarding-inspect-job.json"
JOB=$(jq -r .id "$RUN/evidence/onboarding-inspect-job.json")
for _ in $(seq 1 30); do
  api GET "$CONTROL/api/setup/jobs/$JOB" >"$RUN/evidence/onboarding-inspect-result.json"
  jq -e '.state != "running" and .state != "queued"' "$RUN/evidence/onboarding-inspect-result.json" >/dev/null && break
  sleep 0.5
done
gate blocked "owner onboarding inspect/register/configure_worker" \
  "job $(jq -r .state "$RUN/evidence/onboarding-inspect-result.json"): $(jq -r '.error.code + ": " + .error.message' "$RUN/evidence/onboarding-inspect-result.json")"

curl -sS --cacert "$CA" "$BOOTSTRAP/api/invitations/$INVITE" | jq '.token="<redacted>"' >"$RUN/evidence/invitation-display.json"
HOST_ID="single-host-e2e-$(uname -n | tr -c 'A-Za-z0-9\n' '-' | cut -c1-40)"
jq -n --arg host "$HOST_ID" --arg arch "$(uname -m)" '{
  name:"single-host-e2e-worker", physical_host_id:$host, os:"macos", architecture:$arch,
  capabilities:["git"], capacity:{agent_slots:1,cpu_millis:4000,memory_mb:8192,disk_mb:20480},
  omp_version:"0.0.0-dacs-fake-omp", policy_revision:1, owner_consented:true,
  consent_host:"dacs-e2e-coordinator"}' >"$RUN/evidence/registration.json"
jq -n --arg t "$INVITE" --slurpfile r "$RUN/evidence/registration.json" '{invitation_token:$t,registration:$r[0]}' >"$RUN/secrets/enroll.json"
curl -sS --fail-with-body --cacert "$CA" -H 'Content-Type: application/json' --data-binary @"$RUN/secrets/enroll.json" \
  "$BOOTSTRAP/api/enroll" >"$RUN/secrets/enrollment.json" || die "enrollment via bootstrap" "$(cat "$RUN/secrets/enrollment.json")"
( umask 077; jq -r .device_token "$RUN/secrets/enrollment.json" | tr -d '\n' >"$DEVICE_TOKEN" )
jq '.device_token="<redacted>"' "$RUN/secrets/enrollment.json" >"$RUN/evidence/enrollment.redacted.json"
rm -f "$RUN/secrets/enrollment.json"
WORKER_ID=$(jq -r .worker.id "$RUN/evidence/enrollment.redacted.json")
gate passed "enrollment via TLS bootstrap /api/enroll" "worker $WORKER_ID approved=$(jq -r .worker.approved "$RUN/evidence/enrollment.redacted.json") draining=$(jq -r .worker.draining "$RUN/evidence/enrollment.redacted.json")"
printf 'Authorization: Bearer %s\n' "$(cat "$DEVICE_TOKEN")" >"$RUN/secrets/device.header"
pending=$(curl -s -o "$RUN/evidence/pending-device-state.json" -w '%{http_code}' --cacert "$CA" -H @"$RUN/secrets/device.header" "$CONTROL/api/state")
[[ "$pending" == 403 ]] && gate passed "unapproved device cannot read project state" "GET /api/state with pending device token -> 403" \
  || gate failed "unapproved device cannot read project state" "-> $pending"
request approve "{\"type\":\"approve_worker\",\"worker_id\":\"$WORKER_ID\"}" >/dev/null
[[ "$(jq -r .result.approved "$RUN/evidence/response-approve.json")" == true ]] \
  && gate passed "owner approval (ApproveWorker)" "worker $WORKER_ID approved" || die "owner approval" "$(cat "$RUN/evidence/response-approve.json")"

# --- 5. worker configuration (WorkerConfig schema written by configure_worker) -
jq -n --arg url "$CONTROL" --arg cred "$DEVICE_TOKEN" --arg id "$WORKER_ID" --arg omp "$FAKE_OMP" \
  --arg data "$WORKER_STATE" --arg ws "$RUN/worker/workspaces" --arg ca "$CA" \
  --slurpfile reg "$RUN/evidence/enrollment.redacted.json" '{
  coordinator_url:$url, credential_file:$cred, worker_id:$id, registration:$reg[0].worker.registration,
  omp_executable:$omp, omp_version:$reg[0].worker.registration.omp_version, omp_profile:null,
  data_dir:$data, workspaces_dir:$ws, ca_certificate:$ca, poll_interval_ms:500}' >"$WORKER_CONFIG"
chmod 600 "$WORKER_CONFIG"
cp "$WORKER_CONFIG" "$RUN/evidence/worker.json"

# --- 6. worker run #1 --------------------------------------------------------
start_worker 1
wait_for 90 "worker ready" "[.runtime_status[]|select(.worker_id==\"$WORKER_ID\")][0].provider_authenticated == true and ([.workers[]|select(.id==\"$WORKER_ID\")][0].draining == false)" \
  || die "worker start + heartbeat" "$(tail -5 "$RUN/logs/worker-1.log")"
worker_status ready >/dev/null
gate passed "dacs worker run: heartbeat, runtime status, undrain" "running=$(jq -r .running "$RUN/evidence/worker-status-ready.json") provider_authenticated=$(jq -r .provider_authenticated "$RUN/evidence/worker-status-ready.json") (FAKE: readiness answered locally, not provider evidence)"

# --- 7. task 1: dispatch -> isolated workspace -> candidate -> publication ---
task_spec() { # task_spec <title> <source sha> <instructions> [effect class]
  jq -n --arg p "$PROJECT" --arg title "$1" --arg sha "$2" --arg ins "$3" --arg effect "${4:-workspace}" '{type:"create_task",spec:{
    project_id:$p, title:$title, instructions:$ins, source_commit:$sha, execution:{kind:"agent"},
    dependencies:[], required_capabilities:["git"], resources:{agent_slots:1,cpu_millis:1000,memory_mb:512,disk_mb:256},
    effect_class:$effect, lease_seconds:300, max_runtime_seconds:1800, contract_revision:1, policy_revision:1, max_retries:1}}'
}
publish_candidate() { # publish_candidate <label> <task id> <file> <expected text>; sets INTEGRATED
  local label=$1 task=$2 file=$3 text=$4 cand csha isha
  wait_for 120 "$label candidate" "[.tasks[]|select(.id==\"$task\")][0].state == \"validating\"" \
    || die "$label candidate submission" "$(jq -c "[.tasks[]|select(.id==\"$task\")][0]" "$RUN/evidence/snapshot.json")"
  cand=$(jq -r "[.candidates[]|select(.submission.task_id==\"$task\")][0].id" "$RUN/evidence/snapshot.json")
  csha=$(jq -r "[.candidates[]|select(.id==\"$cand\")][0].submission.commit_sha" "$RUN/evidence/snapshot.json")
  jq "[.candidates[]|select(.id==\"$cand\")][0]" "$RUN/evidence/snapshot.json" >"$RUN/evidence/$label-candidate.json"
  gate passed "$label explicit dacs_submit -> CandidateSubmission" "candidate $cand commit $csha evidence=$(jq -c '[.submission.evidence[]|{name,passed}]' "$RUN/evidence/$label-candidate.json")"
  wait_for 60 "$label stop reconciliation" "([.attempts[]|select(.task_id==\"$task\")][-1].settled == true) and ([.workers[]|select(.id==\"$WORKER_ID\")][0].reserved.agent_slots == 0)" \
    && gate passed "$label Settled + Stopped reconciled, reservation released" "attempt state $(jq -r "[.attempts[]|select(.task_id==\"$task\")][-1].state" "$RUN/evidence/snapshot.json"), worker reserved agent_slots 0" \
    || gate failed "$label Settled + Stopped reconciled" "$(jq -c "[.attempts[]|select(.task_id==\"$task\")][-1]" "$RUN/evidence/snapshot.json")"
  api GET "$CONTROL/api/candidates/$cand/diff" >"$RUN/evidence/$label-diff.json"
  api POST "$CONTROL/api/candidates/$cand/prepare" >"$RUN/evidence/$label-prepare.json"
  isha=$(jq -r .integration_sha "$RUN/evidence/$label-prepare.json")
  [[ "$isha" =~ ^[0-9a-f]{40}$ ]] || die "$label prepare exact integration SHA" "$(cat "$RUN/evidence/$label-prepare.json")"
  # Reviewer check actually run here against the exact staged integration commit.
  local check_cmd="git --git-dir <coordinator bare repo> show $isha:$file"
  local passed=false
  [[ "$(git --git-dir "$BARE" show "$isha:$file")" == "$text" ]] && passed=true
  request "$label-review" "$(jq -n --arg c "$cand" --arg s "$isha" --arg cmd "$check_cmd" --argjson ok $passed '{type:"review_candidate",candidate_id:$c,decision:{
    candidate_sha:$s, approve:$ok, reason:"single-host e2e harness: exact staged integration commit contains the expected file content",
    checks_run:[{name:"reviewer content check",command:$cmd,source_sha:$s,passed:$ok,artifact_digest:null}], skipped_checks:[]}}')" >/dev/null
  api POST "$CONTROL/api/candidates/$cand/publish" >"$RUN/evidence/$label-publish.json"
  local state; state=$(jq -r .state "$RUN/evidence/$label-publish.json")
  snapshot
  INTEGRATED=$(jq -r "[.tasks[]|select(.id==\"$task\")][0].integrated_sha" "$RUN/evidence/snapshot.json")
  local head; head=$(git --git-dir "$BARE" rev-parse refs/heads/main)
  if [[ "$state" == published && "$INTEGRATED" == "$isha" && "$head" == "$isha" ]]; then
    gate passed "$label prepare -> review -> publish into bare repo" "publication $state; task $(jq -r "[.tasks[]|select(.id==\"$task\")][0].state" "$RUN/evidence/snapshot.json"); refs/heads/main = $head (candidate $csha)"
  else
    die "$label publication" "state=$state integrated=$INTEGRATED head=$head"
  fi
}

T1_TEXT="Hello from a DACS-dispatched attempt (fake OMP stand-in)"
request task1 "$(task_spec "single-host e2e: write greeting" "$SEED_SHA" "Write greeting.txt in the repository root, commit it and submit.
FAKE_OMP_EDIT_FILE=greeting.txt
FAKE_OMP_EDIT_TEXT=$T1_TEXT")" >/dev/null
TASK1=$(jq -r .result.id "$RUN/evidence/response-task1.json")
request admit1 "{\"type\":\"admit_task\",\"task_id\":\"$TASK1\",\"worker_id\":null}" >/dev/null
ATTEMPT1=$(jq -r .result.id "$RUN/evidence/response-admit1.json")
[[ "$(jq -r .result.worker_id "$RUN/evidence/response-admit1.json")" == "$WORKER_ID" ]] \
  && gate passed "task 1 admitted (dispatch + reservation)" "task $TASK1 attempt $ATTEMPT1 generation $(jq -r .result.generation "$RUN/evidence/response-admit1.json") -> worker $WORKER_ID" \
  || die "task 1 admission" "$(cat "$RUN/evidence/response-admit1.json")"
wait_for 60 "task 1 started" "[.attempts[]|select(.id==\"$ATTEMPT1\")][0].state != \"admitted\"" \
  && gate passed "task 1 assignment delivered; worker reported Started" "attempt state $(jq -r "[.attempts[]|select(.id==\"$ATTEMPT1\")][0].state" "$RUN/evidence/snapshot.json")" \
  || die "task 1 delivery" "attempt never left admitted"
publish_candidate task1 "$TASK1" greeting.txt "$T1_TEXT"
TASK1_INTEGRATED=$INTEGRATED
WS1="$RUN/worker/workspaces/$ATTEMPT1"
{
  echo "workspace: $WS1"
  echo "branch: $(git -C "$WS1" rev-parse --abbrev-ref HEAD)"
  echo "HEAD: $(git -C "$WS1" rev-parse HEAD)"
  echo "source pin (.dacs-source-pin): $(cat "$WS1/.dacs-source-pin")"
  echo "origin: $(git -C "$WS1" remote get-url origin)"
  echo "objects/info/alternates: $(cat "$WS1/.git/objects/info/alternates" 2>/dev/null || echo none)"
  echo "status --porcelain: [$(git -C "$WS1" status --porcelain)]"
  git -C "$WS1" log --format='%H %an <%ae> %s' -2
} >"$RUN/evidence/task1-workspace.txt"
[[ "$(git -C "$WS1" rev-parse HEAD)" == "$TASK1_INTEGRATED" && "$(cat "$WS1/.dacs-source-pin")" == "$SEED_SHA" && ! -e "$WS1/.git/objects/info/alternates" ]] \
  && gate passed "isolated workspace clone per attempt" "$(git -C "$WS1" rev-parse --abbrev-ref HEAD) in workspaces/$ATTEMPT1, own object store, pinned to seed" \
  || gate failed "isolated workspace clone per attempt" "see evidence/task1-workspace.txt"
"$DACS" source export --url "$CONTROL/" --token-file "$ADMIN_TOKEN" --ca "$CA" --project "$PROJECT" --output "$RUN/export-1.bundle" >/dev/null
git clone -q -b main "$RUN/export-1.bundle" "$RUN/export-1" 2>"$RUN/evidence/export-clone.stderr"
[[ "$(cat "$RUN/export-1/greeting.txt")" == "$T1_TEXT" ]] \
  && gate passed "authenticated source export contains integrated change" "export HEAD $(git -C "$RUN/export-1" rev-parse HEAD)" \
  || gate failed "authenticated source export contains integrated change" "greeting.txt mismatch"
worker_status after-task1

# --- 8. crash drills: kill -9 the worker mid-attempt, then restart -----------
RUNS=1
crash_drill() { # crash_drill <label> <effect class> <source sha> <file> <text> <check stale status: yes|no>
  local label=$1 effect=$2 base=$3 file=$4 text=$5 stale=$6
  local hold="$RUN/evidence/hold-$label.json" ws omp_pid omp_pgid child_pid old_worker
  request "$label" "$(task_spec "single-host e2e: $label ($effect crash drill)" "$base" "Write $file, commit it and submit.
FAKE_OMP_EDIT_FILE=$file
FAKE_OMP_EDIT_TEXT=$text
FAKE_OMP_HOLD_MARKER=$hold
FAKE_OMP_HOLD_MAX_SECONDS=300" "$effect")" >/dev/null
  DRILL_TASK=$(jq -r .result.id "$RUN/evidence/response-$label.json")
  request "admit-$label" "{\"type\":\"admit_task\",\"task_id\":\"$DRILL_TASK\",\"worker_id\":null}" >/dev/null
  DRILL_ATTEMPT=$(jq -r .result.id "$RUN/evidence/response-admit-$label.json")
  DRILL_GEN=$(jq -r .result.generation "$RUN/evidence/response-admit-$label.json")
  for _ in $(seq 1 120); do [[ -f "$hold" ]] && break; sleep 0.5; done
  [[ -f "$hold" ]] || die "$label: fake OMP mid-attempt" "hold marker never appeared"
  wait_for 30 "$label executing" "[.attempts[]|select(.id==\"$DRILL_ATTEMPT\")][0].state == \"executing\"" || die "$label" "attempt not executing"
  omp_pid=$(jq -r .fake_omp_pid "$hold"); omp_pgid=$(jq -r .fake_omp_pgid "$hold"); child_pid=$(jq -r .child_pid "$hold")
  ws="$RUN/worker/workspaces/$DRILL_ATTEMPT"
  worker_status "$label-before-kill" >/dev/null
  ls "$WORKER_STATE/processes" >"$RUN/evidence/$label-receipts-before-kill.txt"
  pgroup "$omp_pgid" >"$RUN/evidence/$label-ps-before-kill.txt"
  git -C "$ws" status --porcelain >"$RUN/evidence/$label-workspace-before-kill.txt"
  log "kill -9 worker pid $WORKER_PID (fake OMP pid $omp_pid pgid $omp_pgid holding $effect attempt $DRILL_ATTEMPT)"
  kill -KILL "$WORKER_PID"; wait "$WORKER_PID" 2>/dev/null || true
  old_worker=$WORKER_PID; WORKER_PID=""
  sleep 1
  pgroup "$omp_pgid" >"$RUN/evidence/$label-ps-after-kill.txt" || true
  if kill -0 "$omp_pid" 2>/dev/null && kill -0 "$child_pid" 2>/dev/null; then
    gate passed "$label crash: worker killed, supervised OMP group orphaned" "worker $old_worker gone; fake OMP $omp_pid + child $child_pid alive, ppid now $(ps -o ppid= -p "$omp_pid" | tr -d ' ')"
  else
    gate failed "$label crash: orphaned OMP group observed" "fake OMP or its child already exited"
  fi
  if [[ "$stale" == yes ]]; then
    sleep 16  # local status treats observations older than 15 s as not running
    worker_status "$label-after-kill" >/dev/null
    [[ "$(jq -r .running "$RUN/evidence/worker-status-$label-after-kill.json")" == false ]] \
      && gate passed "$label dacs worker status after crash" "running=false (stale observation), active_attempt_ids=$(jq -c .active_attempt_ids "$RUN/evidence/worker-status-$label-after-kill.json")" \
      || gate failed "$label dacs worker status after crash" "$(cat "$RUN/evidence/worker-status-$label-after-kill.json")"
  fi
  RUNS=$((RUNS + 1))
  start_worker "$RUNS"
  for _ in $(seq 1 60); do kill -0 "$omp_pid" 2>/dev/null || break; sleep 0.5; done
  pgroup "$omp_pgid" >"$RUN/evidence/$label-ps-after-restart.txt" 2>&1 || true
  if ! kill -0 "$omp_pid" 2>/dev/null && ! kill -0 "$child_pid" 2>/dev/null; then
    gate passed "$label restart: orphaned process group reconciled (killed)" "fake OMP $omp_pid and child $child_pid gone after worker run #$RUNS startup"
  else
    gate failed "$label restart: orphaned process group reconciled" "orphan still alive"
  fi
}
attempt_events() { # attempt_events <label> <attempt>
  snapshot
  jq "[.events[]|select(.kind==\"worker_event\" and .payload.attempt_id==\"$2\")|{sequence:.payload.sequence,generation:.payload.generation,event:.payload.event}]" \
    "$RUN/evidence/snapshot.json" >"$RUN/evidence/$1-worker-events.json"
  find "$WORKER_STATE/attempts/$2/session" -name fake-omp-trace.jsonl -exec cat {} + \
    | jq -c 'select(.dir=="note")|.frame' >"$RUN/evidence/$1-fake-omp-notes.jsonl" 2>/dev/null || true
}

# Drill A: replayable workspace attempt -> the same attempt/generation is reattached.
T2_TEXT="Recovered after kill -9 of the worker (fake OMP stand-in)"
crash_drill task2 workspace "$TASK1_INTEGRATED" recovery.txt "$T2_TEXT" yes
TASK2=$DRILL_TASK; ATTEMPT2=$DRILL_ATTEMPT; GEN2=$DRILL_GEN
publish_candidate task2 "$TASK2" recovery.txt "$T2_TEXT"
TASK2_INTEGRATED=$INTEGRATED
attempt_events task2 "$ATTEMPT2"
attempts2=$(jq "[.attempts[]|select(.task_id==\"$TASK2\")]|length" "$RUN/evidence/snapshot.json")
reattach=$(jq '[.[]|select(.event.type=="progress" and (.event.message|test("Reattached")))]|length' "$RUN/evidence/task2-worker-events.json")
if [[ "$attempts2" == 1 && "$reattach" == 1 ]]; then
  gate passed "task2 recovery: same attempt/generation resumed, not replaced" "attempt $ATTEMPT2 generation $GEN2 (1 attempt for task); events $(jq -c '[.[]|.event.type]' "$RUN/evidence/task2-worker-events.json")"
else
  gate failed "task2 recovery: same attempt resumed" "attempts=$attempts2 reattach_events=$reattach"
fi
grep -q 'not repeated' "$RUN/evidence/task2-fake-omp-notes.jsonl" && grep -q 'open_session' "$RUN/evidence/task2-fake-omp-notes.jsonl" \
  && gate passed "task2 resumed session: open_session + restart prompt, edit not repeated" "fake OMP trace (labelled stand-in)" \
  || gate failed "task2 resumed session trace" "see evidence/task2-fake-omp-notes.jsonl"

# Drill B: external-effect attempt -> no automatic replay; explicit failure + stop.
T3_TEXT="External-effect attempt interrupted by kill -9 (fake OMP stand-in)"
crash_drill task3 external "$TASK2_INTEGRATED" external.txt "$T3_TEXT" no
TASK3=$DRILL_TASK; ATTEMPT3=$DRILL_ATTEMPT
wait_for 60 "task3 failure reconciled" "([.tasks[]|select(.id==\"$TASK3\")][0].state == \"failed\") and ([.attempts[]|select(.id==\"$ATTEMPT3\")][0].settled == true) and ([.workers[]|select(.id==\"$WORKER_ID\")][0].reserved.agent_slots == 0)" \
  || gate failed "task3 external attempt failed + stopped" "$(jq -c "[.tasks[]|select(.id==\"$TASK3\")][0]" "$RUN/evidence/snapshot.json")"
attempt_events task3 "$ATTEMPT3"
reason=$(jq -r '[.[]|select(.event.type=="failed")][0].event.message // ""' "$RUN/evidence/task3-worker-events.json")
cands3=$(jq "[.candidates[]|select(.submission.task_id==\"$TASK3\")]|length" "$RUN/evidence/snapshot.json")
sessions3=$(grep -c 'assignment prompt received' "$RUN/evidence/task3-fake-omp-notes.jsonl" || true)
git -C "$RUN/worker/workspaces/$ATTEMPT3" status --porcelain >"$RUN/evidence/task3-workspace-after-restart.txt"
if [[ "$reason" == *"automatic replay is forbidden"* && "$cands3" == 0 && "$sessions3" == 1 \
      && "$(git --git-dir "$BARE" rev-parse refs/heads/main)" == "$TASK2_INTEGRATED" ]]; then
  gate passed "task3 external attempt not replayed after crash" "task failed; events $(jq -c '[.[]|.event.type]' "$RUN/evidence/task3-worker-events.json"); OMP not relaunched; no candidate; main unchanged; workspace kept: $(tr '\n' ' ' <"$RUN/evidence/task3-workspace-after-restart.txt")"
else
  gate failed "task3 external attempt not replayed" "reason='$reason' candidates=$cands3 sessions=$sessions3"
fi

# --- 9. owner drain and shutdown ---------------------------------------------
worker_status before-drain >/dev/null
"$DACS" worker control --data-dir "$WORKER_STATE" --action drain >"$RUN/evidence/drain-request.json"
for _ in $(seq 1 60); do kill -0 "$WORKER_PID" 2>/dev/null || break; sleep 0.5; done
if ! kill -0 "$WORKER_PID" 2>/dev/null; then
  wait "$WORKER_PID" && rc=0 || rc=$?; WORKER_PID=""
  worker_status after-drain >/dev/null
  snapshot
  gate passed "owner drain: worker exits after reconciling" "exit $rc; status running=$(jq -r .running "$RUN/evidence/worker-status-after-drain.json") draining=$(jq -r .draining "$RUN/evidence/worker-status-after-drain.json"); coordinator worker draining=$(jq -r "[.workers[]|select(.id==\"$WORKER_ID\")][0].draining" "$RUN/evidence/snapshot.json")"
else
  gate failed "owner drain" "worker still running 30 s after drain request"
fi
[[ -z "$(ls "$WORKER_STATE/processes")" ]] && gate passed "no supervised process receipts remain" "processes/ empty" \
  || gate failed "no supervised process receipts remain" "$(ls "$WORKER_STATE/processes")"
leftover=$(pgrep -f -- "--session-dir $RUN/" || true)
[[ -z "$leftover" ]] && gate passed "no fake OMP processes remain" "pgrep empty" || gate failed "no fake OMP processes remain" "$leftover"

snapshot
cp "$RUN/evidence/snapshot.json" "$RUN/evidence/final-snapshot.json"
jq '[.events[]|{sequence,kind,attempt:(.payload.attempt_id // .payload.id // null),event:(.payload.event.type // null)}]' \
  "$RUN/evidence/final-snapshot.json" >"$RUN/evidence/event-timeline.json"
git --git-dir "$BARE" log --format='%H %an <%ae> %s' refs/heads/main >"$RUN/evidence/integration-log.txt"
kill -INT "$COORD_PID"; for _ in $(seq 1 40); do kill -0 "$COORD_PID" 2>/dev/null || break; sleep 0.5; done
if ! kill -0 "$COORD_PID" 2>/dev/null; then wait "$COORD_PID" && crc=0 || crc=$?; COORD_PID=""
  gate passed "coordinator graceful shutdown (SIGINT)" "exit $crc"
else gate failed "coordinator graceful shutdown" "still running"; fi

jq -n --arg run "$RUN" --arg project "$PROJECT" --arg worker "$WORKER_ID" --arg seed "$SEED_SHA" \
  --arg t1 "$TASK1" --arg a1 "$ATTEMPT1" --arg i1 "$TASK1_INTEGRATED" \
  --arg t2 "$TASK2" --arg a2 "$ATTEMPT2" --arg i2 "$TASK2_INTEGRATED" --arg t3 "$TASK3" --arg a3 "$ATTEMPT3" \
  --arg head "$(git --git-dir "$BARE" rev-parse refs/heads/main)" \
  '{run_dir:$run,project:$project,worker:$worker,seed_sha:$seed,final_main:$head,
    task1:{task:$t1,attempt:$a1,integrated_sha:$i1},task2_workspace_crash_drill:{task:$t2,attempt:$a2,integrated_sha:$i2},
    task3_external_crash_drill:{task:$t3,attempt:$a3,state:"failed (no replay)"}}' \
  | tee "$RUN/evidence/summary.json"
if grep -q $'\tfailed\t' "$GATES"; then exit 1; fi
