#!/usr/bin/env bash
# sandbox-run docker backend acceptance gates (P1).
#
# Every gate is exit-non-zero: prose is not a guard, this script is.
# Run from the repo root:  bash test/docker-gates.sh
#
# Requires a working docker daemon; when docker is unavailable the gates are
# skipped (exit 0 with a note) — CI does not install docker. Fixtures live in
# a temp dir; the only global state touched is the `sandbox-run` container and
# the ~/.cache/sandbox-run workspace, both owned by sandbox-run (an unowned
# pre-existing container with the same name makes the gates skip, never
# delete).

set -u

if [ -d "$HOME/.cargo/bin" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
if [ -d "/opt/homebrew/bin" ]; then
  export PATH="/opt/homebrew/bin:$PATH"
fi

SB=${SB:-"$PWD/target/debug/sandbox-run"}
CN=sandbox-run
FAILS=0

note() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILS=$((FAILS + 1)); }

# docker availability → skip gracefully (CI has no docker)
if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found — docker backend gates skipped"
  exit 0
fi
if ! docker info >/dev/null 2>&1; then
  echo "docker daemon unavailable — docker backend gates skipped"
  exit 0
fi

# refuse to touch an unowned pre-existing container; otherwise start clean
if docker inspect "$CN" >/dev/null 2>&1; then
  OWNED=$(docker inspect --format '{{index .Config.Labels "com.losecher.sandbox-run"}}' "$CN" 2>/dev/null)
  if [ "$OWNED" != "1" ]; then
    echo "unowned container $CN exists — docker backend gates skipped (refusing to touch it)"
    exit 0
  fi
  docker rm -f "$CN" >/dev/null 2>&1 || true
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; docker rm -f "$CN" >/dev/null 2>&1 || true' EXIT

main_hash() { ( cd "$1" && git status --porcelain | LC_ALL=C sort ) | shasum -a 256 | cut -d' ' -f1; }
container_labels() { docker inspect --format '{{json .Config.Labels}}' "$CN" 2>/dev/null; }
container_running() { [ "$(docker inspect --format '{{.State.Running}}' "$CN" 2>/dev/null)" = "true" ]; }

make_git_fixture() {
  local d="$1"
  mkdir -p "$d"
  ( cd "$d" \
    && git init -q \
    && git config user.email test@sandbox-run.local \
    && git config user.name "sandbox-run test" \
    && echo v1 > tracked.txt \
    && echo '.sandbox-run/' > .gitignore \
    && git add -A \
    && git commit -qm init \
    && echo v2 > tracked.txt )
}

# ---------------------------------------------------------------- gate 1
note "D1: docker pass/fail — container creation, isolation, ledger chain, cleanup"
D="$WORK/d1"
make_git_fixture "$D"
printf 'exit 0\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
BEFORE=$(main_hash "$D")
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "pass case: expected exit 0, got $RC"
echo "$OUT" | grep -q '"verdict": "pass"' || fail "pass case: verdict is not pass"
echo "$OUT" | grep -q '"backend": "docker"' || fail "pass case: backend is not docker"
echo "$OUT" | grep -q '"cleaned": true' || fail "pass case: sandbox.cleaned is not true"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "pass case: main tree status changed"
docker inspect "$CN" >/dev/null 2>&1 || fail "pass case: container $CN missing after run"
container_labels | grep -q 'com.losecher.sandbox-run' || fail "pass case: ownership label missing"
container_labels | grep -q 'com.losecher.sandbox-run.schema-version' || fail "pass case: schema label missing"
container_labels | grep -q 'com.losecher.sandbox-run.host-sha256' || fail "pass case: host-sha256 label missing"
WS="$HOME/.cache/sandbox-run/workspace"
[ -z "$(ls -A "$WS" 2>/dev/null)" ] || fail "pass case: workspace not cleared after run"
CHAIN=$( python3 -c '
import json,sys
print(" ".join(json.loads(l)["t"] for l in open(sys.argv[1]) if l.strip()))
' "$D/.sandbox-run/runs.jsonl" )
echo "$CHAIN" | grep -q 'run.start scope.detect sandbox.setup verify.start verify.finish gate.check gate.check report.emit' \
  || fail "pass case: event chain incomplete: $CHAIN"

printf 'exit 1\n' > "$D/verify.sh"
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 1 ] || fail "fail case: expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "fail"' || fail "fail case: verdict is not fail"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "fail case: main tree status changed"
pass "docker pass/fail + creation + ledger chain + workspace cleanup"

# ---------------------------------------------------------------- gate 2
note "D2: G0 — /host is read-only: container write to a host path is rejected"
D="$WORK/d2"
make_git_fixture "$D"
printf 'sh -c "echo pollute > /host/tracked.txt; exit 0"\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
BEFORE=$(main_hash "$D")
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "expected exit 0 (write failed, verify exited 0), got $RC"
echo "$OUT" | grep -qi "read-only file system" || fail "container write to /host was not rejected (no EROFS in output)"
echo "$OUT" | grep -q '"verdict": "pass"' || fail "verdict is not pass (write failed → no pollution)"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "main tree status changed"
grep -q '^v2$' "$D/tracked.txt" || fail "main tree tracked.txt was modified"
pass "G0 read-only /host mount (EROFS on container write)"

# ---------------------------------------------------------------- gate 3
note "D3: G1 — verify modifying the workspace is detected as pollution"
D="$WORK/d3"
make_git_fixture "$D"
printf 'sh -c "echo pollute >> /workspace/tracked.txt"\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
BEFORE=$(main_hash "$D")
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 1 ] || fail "expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "polluted"' || fail "verdict is not polluted"
echo "$OUT" | grep -q '"gate": "sandbox.clean"' || fail "gates lack sandbox.clean"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "main tree status changed"
grep -q '^v2$' "$D/tracked.txt" || fail "main tree tracked.txt was modified"
pass "pollution capture inside the container (deny)"

# ---------------------------------------------------------------- gate 4
note "D4: timeout — whole-container kill, container left stopped, no residual process"
D="$WORK/d4"
make_git_fixture "$D"
printf 'sh -c "sleep 91 & wait"\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
START=$(date +%s)
OUT=$( cd "$D" && "$SB" --backend docker --timeout 2 -- ./verify.sh 2>/dev/null ); RC=$?
END=$(date +%s)
[ "$RC" = 1 ] || fail "expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "timeout"' || fail "verdict is not timeout"
echo "$OUT" | grep -q '"killed": true' || fail "verify.killed is not true"
[ $((END - START)) -lt 15 ] || fail "took too long: $((END - START))s"
container_running && fail "container still running after timeout kill (whole-tree kill failed)" || true
# the next run must bring the container back (docker start via fingerprint reuse);
# after any run the container is reset (killed) by design, so reuse is proven by
# the run succeeding, not by the final running state
printf 'exit 0\n' > "$D/verify.sh"
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "post-timeout run failed (exit $RC): container not reusable"
echo "$OUT" | grep -q '"verdict": "pass"' || fail "post-timeout run verdict is not pass"
pass "timeout whole-container kill + container reuse (docker start on fingerprint match)"

# ---------------------------------------------------------------- gate 5
note "D5: fingerprint upgrade — stale owned container is recreated (rm --force)"
D="$WORK/d5"
make_git_fixture "$D"
printf 'exit 0\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
docker rm -f "$CN" >/dev/null 2>&1 || true
docker run -d --name "$CN" \
  --label com.losecher.sandbox-run=1 \
  --label com.losecher.sandbox-run.schema-version=0 \
  --label com.losecher.sandbox-run.host-sha256=deadbeef \
  --label com.losecher.sandbox-run.mount-auth=0 \
  ubuntu:24.04 sh -c 'sleep infinity' >/dev/null 2>&1 \
  || fail "cannot create stale fixture container"
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "expected exit 0 after recreate, got $RC"
echo "$OUT" | grep -q '"verdict": "pass"' || fail "verdict is not pass"
LABELS=$(container_labels)
echo "$LABELS" | grep -q 'com.losecher.sandbox-run.host-sha256":"deadbeef' && fail "stale host-sha256 label survived (recreate did not happen)" || true
echo "$LABELS" | grep -q 'com.losecher.sandbox-run.schema-version":"1"' || fail "schema-version not upgraded to 1"
pass "fingerprint mismatch → rm --force + recreate"

# ---------------------------------------------------------------- gate 6
note "D6: ownership — an unowned container with the same name is refused"
D="$WORK/d6"
make_git_fixture "$D"
printf 'exit 0\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
docker rm -f "$CN" >/dev/null 2>&1 || true
docker run -d --name "$CN" --label not-ours=1 ubuntu:24.04 sh -c 'sleep infinity' >/dev/null 2>&1 \
  || fail "cannot create unowned fixture container"
OUT=$( cd "$D" && "$SB" --backend docker -- ./verify.sh 2>&1 ); RC=$?
[ "$RC" = 2 ] || fail "expected exit 2 (refused), got $RC"
echo "$OUT" | grep -qi "not owned" || fail "no clear refusal message"
pass "unowned container refused (exit 2)"

# ---------------------------------------------------------------- summary
echo
if [ "$FAILS" -gt 0 ]; then
  printf '\033[31m%d docker gate(s) FAILED\033[0m\n' "$FAILS"
  exit 1
else
  printf '\033[32mall docker gates passed\033[0m\n'
  exit 0
fi
