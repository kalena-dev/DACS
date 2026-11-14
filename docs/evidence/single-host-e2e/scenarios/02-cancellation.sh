#!/usr/bin/env bash
# Scenario 2: cancellation of a running attempt, and cancellation before and
# after publication entry. Real dacs serve / dacs worker run; FAKE OMP.
source "$(dirname "$0")/lib.sh"
scenario_init cancellation "${DACS_E2E_PORT_BASE:-27520}"
setup_coordinator
enroll_worker a e2e-host-a 1
start_worker a 1
wait_worker_ready a || die "worker a ready" "$(tail -5 "$RUN/logs/worker-a-1.log")"

# --- 2a: cancel a running attempt; Cancelled only after the processes are gone ---
HOLD="$RUN/gates/c1-hold.json"
create_task c1 "$(task_spec "cancel while running" "$SEED_SHA" "Write cancel.txt and hold.
FAKE_OMP_EDIT_FILE=cancel.txt
FAKE_OMP_EDIT_TEXT=never committed (cancelled while running; fake OMP)
FAKE_OMP_HOLD_MARKER=$HOLD
FAKE_OMP_HOLD_MAX_SECONDS=300")"
C1=$TASK
admit c1 "$C1" a; C1A=$ATTEMPT
wait_gate_marker "$HOLD" 60 || die "2a fake OMP holding" "hold marker never appeared"
wait_for 30 "c1 executing" "$(attempt_json "$C1A").state == \"executing\"" || die "2a attempt executing" "$(sj "$(attempt_json "$C1A")")"
OMP=$(jq -r .fake_omp_pid "$HOLD"); PG=$(jq -r .fake_omp_pgid "$HOLD"); CHILD=$(jq -r .child_pid "$HOLD")
pgroup "$PG" >"$RUN/evidence/2a-ps-before-cancel.txt"
ls "$(wstate a)/processes" >"$RUN/evidence/2a-receipts-before-cancel.txt"
log "2a: fake OMP pid $OMP (pgid $PG) and child $CHILD are running attempt $C1A"
T0=$(now)
request c1-cancel "{\"type\":\"cancel_task\",\"task_id\":\"$C1\"}" >/dev/null || die "2a cancel_task" "$(cat "$RUN/evidence/response-c1-cancel.stderr")"
gate passed "2a cancel_task accepted while running" "response: state=$(jq -r .result.state "$RUN/evidence/response-c1-cancel.json") cancel_requested=$(jq -r .result.cancel_requested "$RUN/evidence/response-c1-cancel.json") blocked_reason=\"$(jq -r .result.blocked_reason "$RUN/evidence/response-c1-cancel.json")\""
# Poll: process liveness is sampled BEFORE each snapshot, so "cancelled while a
# process was alive" cannot be hidden by the processes dying after the read.
: >"$RUN/evidence/2a-timeline.tsv"
VIOLATION=""; GONE_AT=""; CANCELLED_AT=""
for _ in $(seq 1 300); do
  t=$(now); o=dead; c=dead
  alive "$OMP" && o=alive; alive "$CHILD" && c=alive
  [[ -z "$GONE_AT" && $o == dead && $c == dead ]] && GONE_AT=$t
  snapshot
  st=$(sj "$(task_json "$C1").state"); ast=$(sj "$(attempt_json "$C1A").state"); res=$(sj "$(worker_json a).reserved.agent_slots")
  printf '%s\t+%.3fs\tomp=%s\tchild=%s\ttask=%s\tattempt=%s\treserved_slots=%s\n' "$t" "$(echo "$t - $T0" | bc)" "$o" "$c" "$st" "$ast" "$res" >>"$RUN/evidence/2a-timeline.tsv"
  if [[ "$st" == cancelled ]]; then
    CANCELLED_AT=$t
    [[ $o == alive || $c == alive ]] && VIOLATION="task cancelled while omp=$o child=$c"
    break
  fi
  sleep 0.1
done
save_snapshot 2a-after
[[ -n "$CANCELLED_AT" ]] || die "2a task reaches cancelled" "$(tail -3 "$RUN/evidence/2a-timeline.tsv")"
if [[ -z "$VIOLATION" && -n "$GONE_AT" ]]; then
  gate passed "2a cancelled reported only after OMP + child pids are gone" "processes gone at +$(echo "$GONE_AT - $T0" | bc)s, first snapshot showing cancelled at +$(echo "$CANCELLED_AT - $T0" | bc)s (liveness sampled before each snapshot; $(wc -l <"$RUN/evidence/2a-timeline.tsv" | tr -d ' ') samples)"
else
  gate failed "2a cancelled reported only after processes are gone" "${VIOLATION:-processes never observed gone}"
fi
worker_events "$C1A" >"$RUN/evidence/2a-worker-events.json"
pgroup "$PG" >"$RUN/evidence/2a-ps-after.txt" || true
ls "$(wstate a)/processes" >"$RUN/evidence/2a-receipts-after.txt"
fake_notes a "$C1A" >"$RUN/evidence/2a-fake-notes.jsonl"
check "2a no process receipts or group members remain; reservation released" \
  "receipts before=$(wc -l <"$RUN/evidence/2a-receipts-before-cancel.txt" | tr -d ' ') after=$(wc -l <"$RUN/evidence/2a-receipts-after.txt" | tr -d ' '); group members after=$(wc -l <"$RUN/evidence/2a-ps-after.txt" | tr -d ' '); worker reserved slots=$(sj "$(worker_json a).reserved.agent_slots"); attempt=$(sj "$(attempt_json "$C1A").state"); events=$(jq -c '[.[]|.event.type]' "$RUN/evidence/2a-worker-events.json")" \
  test ! -s "$RUN/evidence/2a-receipts-after.txt" -a ! -s "$RUN/evidence/2a-ps-after.txt" -a "$(sj "$(worker_json a).reserved.agent_slots")" = 0
grep -q 'abort requested' "$RUN/evidence/2a-fake-notes.jsonl" && ABORT=yes || ABORT=no
worker_status a 2a >/dev/null
gate passed "2a worker-side stop record" "fake OMP saw RPC abort: $ABORT; worker status stops=$(jq -c '.stops // null' "$RUN/evidence/worker-status-a-2a.json")"

# Helper: run a normal fake task to a submitted candidate with the worker stopped.
submitted_task() { # submitted_task <label> <file> <text>; sets T TA
  create_task "$1" "$(task_spec "$1" "$(main_head)" "Write $2, commit it and submit.
FAKE_OMP_EDIT_FILE=$2
FAKE_OMP_EDIT_TEXT=$3")"
  T=$TASK
  admit "$1" "$T" a; TA=$ATTEMPT
  wait_for 90 "$1 validating + stopped" "($(task_json "$T").state == \"validating\") and ($(attempt_json "$TA").settled == true) and ($(worker_json a).reserved.agent_slots == 0)" \
    || die "$1 candidate submitted and worker stopped" "$(sj "$(task_json "$T")")"
}

# --- 2b: cancel after prepare + review, before publication entry --------------
T2B_TEXT="2b candidate (cancelled before publication; fake OMP)"
submitted_task c2b c2b.txt "$T2B_TEXT"; C2B=$T; C2BA=$TA
stage_and_review c2b "$C2B" c2b.txt "$T2B_TEXT"
HEAD_BEFORE=$(main_head)
request c2b-cancel "{\"type\":\"cancel_task\",\"task_id\":\"$C2B\"}" >/dev/null || die "2b cancel_task" "$(cat "$RUN/evidence/response-c2b-cancel.stderr")"
api POST "$CONTROL/api/candidates/$CAND/publish" >"$RUN/evidence/c2b-publish.json"; CODE=$(cat "$RUN/evidence/last-http-code")
snapshot
check "2b cancel before publish entry: task cancelled at once, publication refused, main unchanged" \
  "cancel response state=$(jq -r .result.state "$RUN/evidence/response-c2b-cancel.json") (\"$(jq -r .result.blocked_reason "$RUN/evidence/response-c2b-cancel.json")\"); attempt=$(sj "$(attempt_json "$C2BA").state"); publish -> HTTP $CODE $(jq -c . "$RUN/evidence/c2b-publish.json"); publications=$(sj "[.publications[]|select(.task_id==\"$C2B\")]|length"); main $(main_head | cut -c1-12) (unchanged)" \
  test "$(jq -r .result.state "$RUN/evidence/response-c2b-cancel.json")" = cancelled -a "$CODE" != 200 -a "$(main_head)" = "$HEAD_BEFORE" -a "$(sj "[.publications[]|select(.task_id==\"$C2B\")]|length")" = 0

# --- 2c: cancel after publication entry; the Git update then succeeds ---------
install_publication_hook
T2C_TEXT="2c candidate (cancel arrived during publication; fake OMP)"
submitted_task c2c c2c.txt "$T2C_TEXT"; C2C=$T; C2CA=$TA
stage_and_review c2c "$C2C" c2c.txt "$T2C_TEXT"
HEAD_BEFORE=$(main_head)
hook_reset; touch "$RUN/gates/hook-armed"
api POST "$CONTROL/api/candidates/$CAND/publish" >"$RUN/evidence/c2c-publish.json" &
PUB=$!
wait_gate_marker "$RUN/gates/hook-entered-prepared" 30 || die "2c publication paused in update-ref" "hook never entered"
snapshot
INFLIGHT="task=$(sj "$(task_json "$C2C").state") publication=$(sj "[.publications[]|select(.task_id==\"$C2C\")][0].state") main=$(main_head | cut -c1-12)"
request c2c-cancel "{\"type\":\"cancel_task\",\"task_id\":\"$C2C\"}" >/dev/null || die "2c cancel_task" "$(cat "$RUN/evidence/response-c2c-cancel.stderr")"
ABANDON_RC=0
request c2c-abandon "{\"type\":\"abandon_attempt\",\"attempt_id\":\"$C2CA\",\"reason\":\"harness probe during publication\"}" >/dev/null || ABANDON_RC=$?
gate passed "2c cancel during publication is deferred" "in flight: $INFLIGHT; cancel response state=$(jq -r .result.state "$RUN/evidence/response-c2c-cancel.json") cancel_requested=$(jq -r .result.cancel_requested "$RUN/evidence/response-c2c-cancel.json") \"$(jq -r .result.blocked_reason "$RUN/evidence/response-c2c-cancel.json")\"; abandon probe exit $ABANDON_RC: $(tr -d '\n' <"$RUN/evidence/response-c2c-abandon.stderr" | cut -c1-160)"
touch "$RUN/gates/hook-release"
wait "$PUB" || true
snapshot
check "2c publication completes; task accepted, deferred cancellation recorded" \
  "publish -> $(jq -c '{state}' "$RUN/evidence/c2c-publish.json"); task=$(sj "$(task_json "$C2C").state") cancel_requested=$(sj "$(task_json "$C2C").cancel_requested") \"$(sj "$(task_json "$C2C").blocked_reason")\"; integrated=$(sj "$(task_json "$C2C").integrated_sha" | cut -c1-12); main $(main_head | cut -c1-12) (was $(echo "$HEAD_BEFORE" | cut -c1-12))" \
  test "$(sj "$(task_json "$C2C").state")" = accepted -a "$(main_head)" = "$ISHA"

# --- 2d: cancel after publication entry; the Git update then fails ------------
T2D_TEXT="2d candidate (cancel during a failed publication; fake OMP)"
submitted_task c2d c2d.txt "$T2D_TEXT"; C2D=$T; C2DA=$TA
stage_and_review c2d "$C2D" c2d.txt "$T2D_TEXT"
HEAD_BEFORE=$(main_head)
hook_reset; touch "$RUN/gates/hook-armed" "$RUN/gates/hook-abort"
api POST "$CONTROL/api/candidates/$CAND/publish" >"$RUN/evidence/c2d-publish.json" &
PUB=$!
wait_gate_marker "$RUN/gates/hook-entered-prepared" 30 || die "2d publication paused in update-ref" "hook never entered"
request c2d-cancel "{\"type\":\"cancel_task\",\"task_id\":\"$C2D\"}" >/dev/null || die "2d cancel_task" "$(cat "$RUN/evidence/response-c2d-cancel.stderr")"
touch "$RUN/gates/hook-release"
wait "$PUB" || true
CODE=$(cat "$RUN/evidence/last-http-code")
snapshot
check "2d failed publication: not_published, deferred cancellation applied, main unchanged" \
  "cancel response state=$(jq -r .result.state "$RUN/evidence/response-c2d-cancel.json"); publish -> HTTP $CODE $(jq -c . "$RUN/evidence/c2d-publish.json" | cut -c1-200); publication=$(sj "[.publications[]|select(.task_id==\"$C2D\")][0].state"); task=$(sj "$(task_json "$C2D").state") \"$(sj "$(task_json "$C2D").blocked_reason")\"; main $(main_head | cut -c1-12) (unchanged)" \
  test "$(sj "[.publications[]|select(.task_id==\"$C2D\")][0].state")" = not_published -a "$(sj "$(task_json "$C2D").state")" = cancelled -a "$(main_head)" = "$HEAD_BEFORE"
hook_reset

drain_worker a && gate passed "owner drain after scenario" "worker a exited" || gate failed "owner drain after scenario" "worker a still running"
check "no fake OMP processes remain" "pgrep -f --session-dir $RUN/" test -z "$(pgrep -f -- "--session-dir $RUN/" || true)"
save_snapshot final
finish
