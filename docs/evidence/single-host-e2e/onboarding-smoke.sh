#!/usr/bin/env bash
# One-host smoke of the owner path through the real onboarding API in local-only
# mode (loopback coordinator, no network infrastructure): inspect -> register ->
# approve -> configure_worker -> `dacs worker run` -> admit -> candidate ->
# review -> publish -> drain. Real coordinator/worker binaries and Git; the OMP
# is the labelled FAKE stand-in unless OMP=/path/to/omp is given (a real OMP
# runs a provider-backed readiness turn and task and needs a logged-in profile).
# Usage: cargo build -p dacs && docs/evidence/single-host-e2e/onboarding-smoke.sh
set -uo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
DACS=${DACS_BIN:-$REPO/target/debug/dacs}
OMP=${OMP:-$REPO/docs/evidence/single-host-e2e/fake-omp}
PORT=${PORT:-27650}
RUN=${RUN:-$(mktemp -d "${TMPDIR:-/tmp}/dacs-onboarding-smoke.XXXXXX")}
rm -rf "$RUN"; mkdir -p "$RUN"/{logs,ev,home}
export HOME=$RUN/home GIT_CONFIG_NOSYSTEM=1
git config --global user.name "DACS smoke"; git config --global user.email smoke@dacs.invalid; git config --global init.defaultBranch main
URL=http://127.0.0.1:$PORT
COORD=$RUN/coord
ok() { echo "PASS  $*"; }
bad() { echo "FAIL  $*"; exit 1; }
cleanup() { [[ -n "${WPID:-}" ]] && kill -INT "$WPID" 2>/dev/null; sleep 1; [[ -n "${CPID:-}" ]] && kill -INT "$CPID" 2>/dev/null; sleep 1; pkill -f "$RUN" 2>/dev/null; true; }
trap cleanup EXIT

"$DACS" serve --listen 127.0.0.1:$PORT --data-dir "$COORD" --local-setup >"$RUN/logs/coord.log" 2>&1 & CPID=$!
for _ in $(seq 60); do curl -sf "$URL/health" >/dev/null && break; sleep 0.5; done
curl -sf "$URL/health" >/dev/null && ok "coordinator serving on $URL" || bad "coordinator did not start: $(tail -3 "$RUN/logs/coord.log")"
ADMIN=$COORD/admin.token
AUTH=(-H "Authorization: Bearer $(cat "$ADMIN")")
req() { printf '%s' "$2" >"$RUN/ev/req-$1.json"; "$DACS" request --url "$URL/" --token-file "$ADMIN" --file "$RUN/ev/req-$1.json" >"$RUN/ev/res-$1.json" 2>&1 || bad "$1: $(cat "$RUN/ev/res-$1.json")"; }
snap() { "$DACS" snapshot --url "$URL/" --token-file "$ADMIN" >"$RUN/ev/snapshot.json"; }
waitfor() { local d=$((SECONDS+$1)); while ((SECONDS<d)); do snap; [[ "$(jq -r "$2" "$RUN/ev/snapshot.json")" == true ]] && return 0; sleep 1; done; return 1; }
onboard() { # onboard <op> <json> -> job result in ev/onb-<op>.json
  printf '%s' "$2" >"$RUN/ev/onb-$1-req.json"
  local job; job=$(curl -sS "${AUTH[@]}" -H 'Content-Type: application/json' --data-binary @"$RUN/ev/onb-$1-req.json" "$URL/api/onboarding/$1" | jq -r .id)
  for _ in $(seq 120); do
    curl -sS "${AUTH[@]}" "$URL/api/setup/jobs/$job" >"$RUN/ev/onb-$1.json"
    jq -e '.state != "running" and .state != "queued"' "$RUN/ev/onb-$1.json" >/dev/null && break; sleep 0.5
  done
  jq -e '.state == "completed" and .error == null' "$RUN/ev/onb-$1.json" >/dev/null || bad "onboarding $1: $(jq -c '{state,error}' "$RUN/ev/onb-$1.json")"
  REVIEW=$(jq -r '.result.review // empty' "$RUN/ev/onb-$1.json"); ok "onboarding $1"
}

# project + seeded repository
req project '{"type":"create_project","name":"smoke","target_branch":"main","max_tasks":10,"max_depth":2}'
PROJECT=$(jq -r .result.id "$RUN/ev/res-project.json")
SRC=$RUN/src; git init -q -b main "$SRC"; echo seed >"$SRC/hello.txt"; git -C "$SRC" add .; git -C "$SRC" commit -qm seed
SEED=$(git -C "$SRC" rev-parse HEAD); git -C "$SRC" bundle create -q "$RUN/seed.bundle" main
"$DACS" source import --url "$URL/" --token-file "$ADMIN" --project "$PROJECT" --file "$RUN/seed.bundle" --seed >"$RUN/ev/import.json" || bad "source import"
ok "project $PROJECT seeded at $SEED"

# invitation -> owner onboarding (local-only)
req invite "{\"type\":\"create_invitation\",\"project_id\":\"$PROJECT\",\"host_name\":\"smoke-coordinator\",\"expires_in_seconds\":3600}"
TOKEN=$(jq -r .result.token "$RUN/ev/res-invite.json")
onboard inspect "$(jq -n --arg e "$URL" --arg t "$TOKEN" '{endpoint:$e,invitation_token:$t}')"
[[ "$(jq -r .result.local_only "$RUN/ev/onb-inspect.json")" == true ]] && ok "inspect marked local-only (no network contract)" || bad "not local-only"
REG=$(jq -n --arg arch "$(uname -m)" '{name:"smoke-worker",physical_host_id:"smoke-host",os:"macos",architecture:$arch,
  capabilities:["git"],capacity:{agent_slots:1,cpu_millis:4000,memory_mb:8192,disk_mb:20480},
  omp_version:"0.0.0-dacs-fake-omp",policy_revision:1,owner_consented:true,consent_host:"smoke-coordinator"}')
onboard register "$(jq -n --arg r "$REVIEW" --argjson reg "$REG" '{review:$r,registration:$reg,owner_approved:"on"}')"
WID=$(jq -r .result.device_id "$RUN/ev/onb-register.json")
onboard refresh "$(jq -n --arg r "$REVIEW" '{review:$r}')"
[[ "$(jq -r .result.network_state.approved "$RUN/ev/onb-refresh.json")" == false ]] && ok "pending device not yet approved" || bad "approval state wrong before approval"
req approve "{\"type\":\"approve_worker\",\"worker_id\":\"$WID\"}"
onboard refresh "$(jq -n --arg r "$REVIEW" '{review:$r}')"
[[ "$(jq -r .result.network_state.approved "$RUN/ev/onb-refresh.json")" == true ]] && ok "owner approval observed by onboarding" || bad "approval not observed"
onboard configure_worker "$(jq -n --arg r "$REVIEW" --arg omp "$OMP" '{review:$r,omp_executable:$omp,worker_config_approved:"on"}')"
CFG=$(jq -r .result.worker_config_file "$RUN/ev/onb-configure_worker.json")
[[ "$(jq -r .coordinator_url "$CFG")" == "$URL" ]] && ok "worker.json targets $URL" || bad "worker.json url $(jq -r .coordinator_url "$CFG")"

# manual worker launch
"$DACS" worker run --config "$CFG" >"$RUN/logs/worker.log" 2>&1 & WPID=$!
waitfor 90 "[.runtime_status[]|select(.worker_id==\"$WID\")][0].provider_authenticated == true and ([.workers[]|select(.id==\"$WID\")][0].draining == false)" \
  && ok "worker heartbeating and admitted (FAKE OMP readiness)" || bad "worker not ready: $(tail -5 "$RUN/logs/worker.log")"
"$DACS" worker status --data-dir "$(dirname "$CFG")" >"$RUN/ev/worker-status.json" 2>&1 && ok "dacs worker status: running=$(jq -r .running "$RUN/ev/worker-status.json")" || bad "worker status: $(cat "$RUN/ev/worker-status.json")"

# task -> candidate -> publish
req task "$(jq -n --arg p "$PROJECT" --arg sha "$SEED" '{type:"create_task",spec:{project_id:$p,title:"smoke task",
  instructions:"Write greeting.txt, commit and submit.\nFAKE_OMP_EDIT_FILE=greeting.txt\nFAKE_OMP_EDIT_TEXT=smoke greeting",source_commit:$sha,execution:{kind:"agent"},dependencies:[],required_capabilities:["git"],
  resources:{agent_slots:1,cpu_millis:1000,memory_mb:512,disk_mb:256},effect_class:"workspace",lease_seconds:300,
  max_runtime_seconds:1800,contract_revision:1,policy_revision:1,max_retries:1}}')"
TASK=$(jq -r .result.id "$RUN/ev/res-task.json")
req admit "{\"type\":\"admit_task\",\"task_id\":\"$TASK\",\"worker_id\":null}"
[[ "$(jq -r .result.worker_id "$RUN/ev/res-admit.json")" == "$WID" ]] && ok "task admitted to this worker (owner 'Request admission')" || bad "admit: $(cat "$RUN/ev/res-admit.json")"
waitfor 120 "[.tasks[]|select(.id==\"$TASK\")][0].state == \"validating\"" && ok "task dispatched, worked and submitted" \
  || bad "task stuck: $(jq -c "[.tasks[]|select(.id==\"$TASK\")][0]|"'{state,blocked_reason}' "$RUN/ev/snapshot.json")"
CAND=$(jq -r "[.candidates[]|select(.submission.task_id==\"$TASK\")][0].id" "$RUN/ev/snapshot.json")
curl -sS "${AUTH[@]}" -X POST "$URL/api/candidates/$CAND/prepare" >"$RUN/ev/prepare.json"
ISHA=$(jq -r .integration_sha "$RUN/ev/prepare.json"); [[ "$ISHA" =~ ^[0-9a-f]{40}$ ]] || bad "prepare: $(cat "$RUN/ev/prepare.json")"
BARE=$COORD/source/projects/$PROJECT/repository.git
GOT=$(git --git-dir "$BARE" show "$ISHA:greeting.txt" 2>/dev/null); [[ "$GOT" == "smoke greeting" ]] && PASSED=true || PASSED=false
req review "$(jq -n --arg c "$CAND" --arg s "$ISHA" --argjson ok $PASSED '{type:"review_candidate",candidate_id:$c,decision:{candidate_sha:$s,approve:$ok,
  reason:"smoke: staged integration commit has the expected file",checks_run:[{name:"content check",command:"git show",source_sha:$s,passed:$ok,artifact_digest:null}],skipped_checks:[]}}')"
$PASSED && ok "review ran against the exact staged commit $ISHA" || bad "staged content wrong: '$GOT'"
curl -sS "${AUTH[@]}" -X POST "$URL/api/candidates/$CAND/publish" >"$RUN/ev/publish.json"
waitfor 60 "[.tasks[]|select(.id==\"$TASK\")][0].state == \"accepted\"" && ok "candidate published; task accepted" \
  || bad "publish: $(cat "$RUN/ev/publish.json" | head -c 400) state=$(jq -r "[.tasks[]|select(.id==\"$TASK\")][0].state" "$RUN/ev/snapshot.json")"
git -C "$BARE" log --oneline -2 main | sed 's/^/      /'
# drain
"$DACS" worker control --data-dir "$(dirname "$CFG")" --action drain >/dev/null && for _ in $(seq 30); do kill -0 "$WPID" 2>/dev/null || break; sleep 1; done
kill -0 "$WPID" 2>/dev/null && bad "worker did not exit after drain" || { wait "$WPID"; ok "worker drained and exited ($?)"; WPID=; }
echo "ALL PASSED  (run dir $RUN)"
