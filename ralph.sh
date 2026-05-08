#!/usr/bin/env bash
# ralph.sh — minimal Ralph loop runner for Claude Code, with cost guardrails.
#
# Usage:
#   ./ralph.sh                # 30 iters, $50 cost cap
#   ./ralph.sh 50             # 50 iters, $50 cap
#   ./ralph.sh 50 100         # 50 iters, $100 cap
#
# Env overrides:
#   PROMPT_FILE       default: PROMPT.md
#   SPEC_FILE         default: SPEC.md     (existence-checked at startup)
#   COMPLETION_TOKEN  default: <promise>COMPLETE</promise>
#   LOG_DIR           default: .ralph
#   CLAUDE_MODEL      default: claude-opus-4-7   (alias 'opus' also works)
#   CLAUDE_EFFORT     default: medium            (low | medium | high | xhigh | max)
#   CLAUDE_FLAGS      extra flags appended to the claude invocation
#
# Each iteration:
#   1. Snapshots git HEAD (to detect whether the iter committed anything).
#   2. Pipes PROMPT.md to `claude -p --output-format stream-json --verbose`,
#      capturing the full event stream to .ralph/iter-NNN.jsonl.
#   3. Parses the final `result` event for cost/duration/text.
#   4. Greps the result text for the completion sentinel.
#   5. Prints a one-line summary; updates cumulative cost.
# Stops on: sentinel found, claude exit != 0, cost cap hit, or max iters.

set -euo pipefail

# ── Config ──────────────────────────────────────────────────────────────
MAX_ITERATIONS="${1:-30}"
MAX_COST_USD="${2:-50}"
PROMPT_FILE="${PROMPT_FILE:-PROMPT.md}"
SPEC_FILE="${SPEC_FILE:-SPEC.md}"
COMPLETION_TOKEN="${COMPLETION_TOKEN:-<promise>COMPLETE</promise>}"
LOG_DIR="${LOG_DIR:-.ralph}"
CLAUDE_MODEL="${CLAUDE_MODEL:-claude-opus-4-7}"
CLAUDE_EFFORT="${CLAUDE_EFFORT:-medium}"
CLAUDE_FLAGS="${CLAUDE_FLAGS:-}"

# Validate effort level early — claude rejects unknown values, but failing
# here gives a clearer message than a mid-iteration crash.
case "$CLAUDE_EFFORT" in
  low|medium|high|xhigh|max) ;;
  *) echo "✗ CLAUDE_EFFORT='$CLAUDE_EFFORT' invalid (use: low|medium|high|xhigh|max)"; exit 1 ;;
esac

# ── Pre-flight ──────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null || { echo "✗ $1 not found"; exit 1; }; }
need claude; need jq; need git; need awk
[[ -f "$PROMPT_FILE" ]] || { echo "✗ $PROMPT_FILE missing"; exit 1; }
[[ -f "$SPEC_FILE"   ]] || { echo "✗ $SPEC_FILE missing — pin the spec locally first"; exit 1; }
git rev-parse --git-dir >/dev/null 2>&1 || { echo "✗ not a git repo (run: git init)"; exit 1; }

mkdir -p "$LOG_DIR"
LOCK="$LOG_DIR/ralph.lock"
if [[ -f "$LOCK" ]]; then
  echo "✗ another ralph appears to be running (lock: $LOCK)"
  echo "  if you're sure no other instance is alive: rm $LOCK"
  exit 1
fi
echo "$$" > "$LOCK"
trap 'rm -f "$LOCK"' EXIT INT TERM

# ── State ───────────────────────────────────────────────────────────────
TOTAL_COST=0
RUN_LOG="$LOG_DIR/run-$(date +%Y%m%d-%H%M%S).log"

# Continue iter-NNN numbering from the highest pre-existing log, so re-runs
# of ./ralph.sh don't overwrite earlier iterations. nullglob ensures the
# array is empty (not a literal "iter-*.jsonl" string) when no logs exist.
shopt -s nullglob
EXISTING_ITERS=( "$LOG_DIR"/iter-*.jsonl )
shopt -u nullglob
HIGHEST=0
for f in "${EXISTING_ITERS[@]}"; do
  n="${f##*/iter-}"; n="${n%.jsonl}"
  # 10# forces decimal — leading-zero strings would otherwise be read as octal.
  n=$((10#$n))
  (( n > HIGHEST )) && HIGHEST=$n
done
START_N=$((HIGHEST + 1))
END_N=$((START_N + MAX_ITERATIONS - 1))

{
  echo "── ralph.sh starting ──"
  echo "max_iterations=$MAX_ITERATIONS  max_cost=\$$MAX_COST_USD"
  echo "model=$CLAUDE_MODEL  effort=$CLAUDE_EFFORT"
  echo "prompt=$PROMPT_FILE  spec=$SPEC_FILE"
  echo "log_dir=$LOG_DIR  iters=$START_N..$END_N (continuing from $HIGHEST)"
} | tee -a "$RUN_LOG"

# ── Loop ────────────────────────────────────────────────────────────────
for i in $(seq "$START_N" "$END_N"); do
  TS=$(date +%H:%M:%S)
  RUN_POS=$((i - START_N + 1))
  ITER_FILE=$(printf "%s/iter-%03d.jsonl" "$LOG_DIR" "$i")
  printf "\n── [%s] iter %03d  (%d/%d this run)  cum=\$%s ──\n" \
    "$TS" "$i" "$RUN_POS" "$MAX_ITERATIONS" "$(printf '%.4f' "$TOTAL_COST")" | tee -a "$RUN_LOG"

  HEAD_BEFORE=$(git rev-parse HEAD 2>/dev/null || echo "none")

  # Run Claude headless. Stream JSON straight to file; tail will give a
  # rough live progress view if you `tail -f` it from another terminal.
  set +e
  cat "$PROMPT_FILE" | claude \
    -p \
    --model "$CLAUDE_MODEL" \
    --effort "$CLAUDE_EFFORT" \
    --dangerously-skip-permissions \
    --output-format stream-json \
    --verbose \
    $CLAUDE_FLAGS \
    > "$ITER_FILE"
  CLAUDE_EXIT=$?
  set -e

  if [[ $CLAUDE_EXIT -ne 0 ]]; then
    echo "✗ claude exited $CLAUDE_EXIT — see $ITER_FILE" | tee -a "$RUN_LOG"
    exit 1
  fi

  # Pull the final result event. claude's stream-json emits exactly one.
  RESULT=$(jq -c 'select(.type=="result")' "$ITER_FILE" | tail -1 || true)
  if [[ -z "$RESULT" ]]; then
    echo "✗ no result event found in $ITER_FILE" | tee -a "$RUN_LOG"
    exit 1
  fi

  ITER_COST=$(jq -r '.total_cost_usd // .cost_usd // 0' <<<"$RESULT")
  ITER_DUR_MS=$(jq -r '.duration_ms       // 0'         <<<"$RESULT")
  ITER_TURNS=$(jq -r '.num_turns          // 0'         <<<"$RESULT")
  ITER_TEXT=$(jq -r  '.result             // ""'        <<<"$RESULT")
  IS_ERROR=$(jq -r   '.is_error           // false'     <<<"$RESULT")

  TOTAL_COST=$(awk -v a="$TOTAL_COST" -v b="$ITER_COST" 'BEGIN{printf "%.6f", a+b}')

  HEAD_AFTER=$(git rev-parse HEAD 2>/dev/null || echo "none")
  COMMITTED="no"; [[ "$HEAD_BEFORE" != "$HEAD_AFTER" ]] && COMMITTED="yes"

  printf "  cost=\$%-8s dur=%-7sms turns=%-3s committed=%-3s error=%s\n" \
    "$(printf '%.4f' "$ITER_COST")" "$ITER_DUR_MS" "$ITER_TURNS" "$COMMITTED" "$IS_ERROR" \
    | tee -a "$RUN_LOG"

  if grep -qF "$COMPLETION_TOKEN" <<<"$ITER_TEXT"; then
    echo "✓ completion sentinel found"                  | tee -a "$RUN_LOG"
    printf "── final cum cost: \$%.4f ──\n" "$TOTAL_COST" | tee -a "$RUN_LOG"
    exit 0
  fi

  if awk -v t="$TOTAL_COST" -v c="$MAX_COST_USD" 'BEGIN{exit !(t>=c)}'; then
    echo "✗ cost cap hit (\$$TOTAL_COST >= \$$MAX_COST_USD)" | tee -a "$RUN_LOG"
    exit 2
  fi

  if [[ "$COMMITTED" == "no" ]]; then
    # Bootstrap iteration may legitimately not commit. Surface it; don't bail.
    echo "  ⚠ no commit this iteration" | tee -a "$RUN_LOG"
  fi
done

echo "── max iterations reached without completion ──" | tee -a "$RUN_LOG"
printf "── final cum cost: \$%.4f ──\n" "$TOTAL_COST"   | tee -a "$RUN_LOG"
exit 3
