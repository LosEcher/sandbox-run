#!/usr/bin/env bash
# sandbox-run mechanical acceptance gates (P0).
#
# Every gate is exit-non-zero: prose is not a guard, this script is.
# Run from the repo root:  bash test/gates.sh
#
# Fixtures are created in a temp dir; nothing outside it is touched.
# jj gates are skipped when jj is unavailable (CI installs it).

set -u

if [ -d "$HOME/.cargo/bin" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
if [ -d "/opt/homebrew/bin" ]; then
  export PATH="/opt/homebrew/bin:$PATH"
fi

SB=${SB:-"$PWD/target/debug/sandbox-run"}
FAILS=0

note() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILS=$((FAILS + 1)); }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

main_hash() { ( cd "$1" && git status --porcelain | LC_ALL=C sort ) | shasum -a 256 | cut -d' ' -f1; }

# git fixture: committed tracked.txt (v1) + .gitignore; agent change → v2.
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

# jj fixture: same shape, jj-backed.
make_jj_fixture() {
  local d="$1"
  mkdir -p "$d"
  ( cd "$d" \
    && jj git init >/dev/null 2>&1 \
    && jj config set --repo user.name "sandbox-run test" >/dev/null 2>&1 \
    && jj config set --repo user.email "test@sandbox-run.local" >/dev/null 2>&1 \
    && echo v1 > tracked.txt \
    && echo '.sandbox-run/' > .gitignore \
    && jj commit -m init >/dev/null 2>&1 \
    && echo v2 > tracked.txt \
    && jj status >/dev/null 2>&1 )  # warm-up snapshot: stable wc commit
}

jj_state() {
  ( cd "$1" && jj status && echo "--WS--" && jj workspace list ) | shasum -a 256 | cut -d' ' -f1
}

jj_visible() { ( cd "$1" && jj log --no-graph -T 'change_id.short() ++ "\n"' | grep -v '^zzzzzzzz' | LC_ALL=C sort ); }

# ---- 沙箱目录清理断言的两个坑（2026-10-09 修）---------------------------------
# sandbox-run 的沙箱目录建在 std::env::temp_dir()（src/sandbox.rs:21）——macOS 下
# 就是 $TMPDIR（/var/folders/.../T），**不是 /tmp**。旧断言写死 `ls -d /tmp/sandbox-run-*`，
# 于是同时错在两个方向：
#   ① 假红（不可归属）：2026-10-08 一个与本次运行毫不相干的 /tmp/sandbox-run-backup
#      让 G1/G4 双双报 "sandbox dir not cleaned"，门禁红了整整一天；
#   ② 假绿（看不见）：正常环境下 $TMPDIR 才是真位置，/tmp 永远为空 ⇒ 真泄漏也照样过。
# 正确判据 = 看对目录 + 只算**本次运行新增**的目录。
sb_tmp_root() { local r="${TMPDIR:-/tmp}"; printf '%s' "${r%/}"; }
sb_tmp_snapshot_to() { ls -d "$(sb_tmp_root)"/sandbox-run-* 2>/dev/null | LC_ALL=C sort > "$1" || true; }
sb_tmp_leaked_since() { ls -d "$(sb_tmp_root)"/sandbox-run-* 2>/dev/null | LC_ALL=C sort | grep -vxF -f "$1" || true; }

# ---------------------------------------------------------------- gate 1
note "G1: git pass/fail — isolated run, main tree untouched, ledger + cleanup"
D="$WORK/g1"
make_git_fixture "$D"
printf 'exit 0\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
BEFORE=$(main_hash "$D")
sb_tmp_snapshot_to "$WORK/g1-tmp-before"
OUT=$( cd "$D" && "$SB" --scope-from-git -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "pass case: expected exit 0, got $RC"
echo "$OUT" | grep -q '"verdict": "pass"' || fail "pass case: verdict is not pass"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "pass case: main tree status changed"
[ -s "$D/.sandbox-run/runs.jsonl" ] || fail "pass case: ledger not written"
python3 -c 'import json,sys
for i,l in enumerate(open(sys.argv[1])):
    if l.strip(): json.loads(l)
' "$D/.sandbox-run/runs.jsonl" || fail "pass case: ledger not parseable"
WT=$( cd "$D" && git worktree list | wc -l | tr -d ' ' )
[ "$WT" = 1 ] || fail "pass case: sandbox worktree left registered (list = $WT)"
[ -z "$(sb_tmp_leaked_since "$WORK/g1-tmp-before")" ] || fail "pass case: sandbox dir not cleaned ($(sb_tmp_leaked_since "$WORK/g1-tmp-before"))"

printf 'exit 1\n' > "$D/verify.sh"
OUT=$( cd "$D" && "$SB" --scope-from-git -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 1 ] || fail "fail case: expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "fail"' || fail "fail case: verdict is not fail"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "fail case: main tree status changed"
pass "git pass/fail + isolation + ledger + cleanup"

# ---------------------------------------------------------------- gate 2
note "G2: timeout — whole-tree kill, no residual process"
D="$WORK/g2"
make_git_fixture "$D"
# grandchild chain: verify.sh → sh -c → sleep 91 (distinctive, avoids false
# positives from unrelated system processes)
printf 'sh -c "sleep 91 & wait"\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
START=$(date +%s)
OUT=$( cd "$D" && "$SB" --scope-from-git --timeout 2 -- ./verify.sh 2>/dev/null ); RC=$?
END=$(date +%s)
[ "$RC" = 1 ] || fail "expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "timeout"' || fail "verdict is not timeout"
echo "$OUT" | grep -q '"killed": true' || fail "verify.killed is not true"
[ $((END - START)) -lt 10 ] || fail "took too long: $((END - START))s"
pgrep -f '^sleep 91$' >/dev/null 2>&1 && fail "residual sleep 91 process" || true
pass "timeout whole-tree kill (<10s, no residual)"

# ---------------------------------------------------------------- gate 3
note "G3: pollution — verify modifies a tracked file → polluted, main tree clean"
D="$WORK/g3"
make_git_fixture "$D"
printf 'echo pollute >> tracked.txt\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
BEFORE=$(main_hash "$D")
OUT=$( cd "$D" && "$SB" --scope-from-git -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 1 ] || fail "expected exit 1, got $RC"
echo "$OUT" | grep -q '"verdict": "polluted"' || fail "verdict is not polluted"
echo "$OUT" | grep -q '"gate": "sandbox.clean"' || fail "gates lack sandbox.clean"
[ "$(main_hash "$D")" = "$BEFORE" ] || fail "main tree status changed"
grep -q '^v2$' "$D/tracked.txt" || fail "main tree tracked.txt was modified"
pass "pollution capture (deny)"

# ---------------------------------------------------------------- gate 4
note "G4: jj pass/fail — workspace isolation, no residual, no new visible change"
if command -v jj >/dev/null 2>&1; then
  D="$WORK/g4"
  make_jj_fixture "$D"
  # arg-driven script: content stays byte-identical across runs (jj snapshots
  # untracked content, so overwriting it between runs would churn the wc commit)
  printf 'exit "$1"\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
  BEFORE=$(jj_state "$D")
  LOG_BEFORE=$(jj_visible "$D")
  sb_tmp_snapshot_to "$WORK/g4-tmp-before"
  OUT=$( cd "$D" && "$SB" --scope-from-jj -- ./verify.sh 0 2>/dev/null ); RC=$?
  [ "$RC" = 0 ] || fail "jj pass case: expected exit 0, got $RC"
  echo "$OUT" | grep -q '"verdict": "pass"' || fail "jj pass case: verdict is not pass"
  echo "$OUT" | grep -q '"backend": "workspace"' || fail "jj pass case: backend is not workspace"
  [ "$(jj_state "$D")" = "$BEFORE" ] || fail "jj pass case: jj state changed"
  [ "$(jj_visible "$D")" = "$LOG_BEFORE" ] || fail "jj pass case: new visible change in jj log"
  WS=$( cd "$D" && jj workspace list )
  echo "$WS" | grep -q 'sandbox-run' && fail "jj pass case: residual workspace" || true
  [ -z "$(sb_tmp_leaked_since "$WORK/g4-tmp-before")" ] || fail "jj pass case: sandbox dir not cleaned ($(sb_tmp_leaked_since "$WORK/g4-tmp-before"))"

  OUT=$( cd "$D" && "$SB" --scope-from-jj -- ./verify.sh 1 2>/dev/null ); RC=$?
  [ "$RC" = 1 ] || fail "jj fail case: expected exit 1, got $RC"
  echo "$OUT" | grep -q '"verdict": "fail"' || fail "jj fail case: verdict is not fail"
  [ "$(jj_state "$D")" = "$BEFORE" ] || fail "jj fail case: jj state changed"
  [ "$(jj_visible "$D")" = "$LOG_BEFORE" ] || fail "jj fail case: new visible change in jj log"
  pass "jj pass/fail + workspace isolation + no residual"
else
  pass "jj unavailable — skipped"
fi

# ---------------------------------------------------------------- gate 5
note "G5: fail-closed — non-repo → exit 2 + clear message"
D="$WORK/g5"
mkdir -p "$D"
OUT=$( cd "$D" && "$SB" -- ./verify.sh 2>&1 ); RC=$?
[ "$RC" = 2 ] || fail "expected exit 2, got $RC"
echo "$OUT" | grep -qi "not a git or jj repository" || fail "no clear fail-closed message"
pass "fail-closed non-repo (exit 2)"

# ---------------------------------------------------------------- gate 6
note "G6: ledger — full event chain + verdict/stats rebuildable via log"
D="$WORK/g6"
make_git_fixture "$D"
printf 'exit 0\n' > "$D/verify.sh"; chmod +x "$D/verify.sh"
OUT=$( cd "$D" && "$SB" --scope-from-git -- ./verify.sh 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "expected exit 0, got $RC"
RUNID=$( echo "$OUT" | python3 -c 'import sys,json; print(json.load(sys.stdin)["runId"])' )
[ -n "$RUNID" ] || fail "no runId in report"
CHAIN=$( python3 -c '
import json,sys
print(" ".join(json.loads(l)["t"] for l in open(sys.argv[1]) if l.strip()))
' "$D/.sandbox-run/runs.jsonl" )
echo "$CHAIN" | grep -q 'run.start scope.detect sandbox.setup verify.start verify.finish gate.check gate.check report.emit' \
  || fail "chain incomplete: $CHAIN"
LOGOUT=$( cd "$D" && "$SB" log "$RUNID" 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "log exit $RC"
echo "$LOGOUT" | grep -q 'verdict pass' || fail "log does not rebuild verdict"
echo "$LOGOUT" | grep -q 'exit 0' || fail "log does not rebuild exit code"
( cd "$D" && "$SB" log no-such-run >/dev/null 2>&1 ); RC=$?
[ "$RC" = 2 ] || fail "log with unknown runId should exit 2, got $RC"
pass "ledger chain + rebuild + unknown-id fail-closed"

# ---------------------------------------------------------------- gate 7
note "G7: dangling repair — bare run.start → status/log synthesize interrupted"
D="$WORK/g7"
make_git_fixture "$D"
python3 - "$D" <<'PYEOF'
import json, os, sys
d = sys.argv[1]
os.makedirs(os.path.join(d, ".sandbox-run"), exist_ok=True)
with open(os.path.join(d, ".sandbox-run", "runs.jsonl"), "a") as f:
    f.write(json.dumps({
        "t": "run.start", "run_id": "run-dangling-g7", "version": "0.1.0",
        "ts": 1, "cwd": d, "vcs": "git", "base_ref": "HEAD", "cmd": ["x"],
        "env_keys": [], "scope_files": [], "scope_source": "git-diff",
        "config_hash": "abc"
    }) + "\n")
PYEOF
OUT=$( cd "$D" && "$SB" status 2>/dev/null )
echo "$OUT" | grep -q 'run-dangling-g7' || fail "status misses the dangling run"
echo "$OUT" | grep -q 'interrupted' || fail "status does not synthesize interrupted"
LOGOUT=$( cd "$D" && "$SB" log run-dangling-g7 2>/dev/null ); RC=$?
[ "$RC" = 0 ] || fail "log (dangling) exit $RC"
echo "$LOGOUT" | grep -q 'interrupted' || fail "log does not synthesize interrupted"
pass "dangling run.start → interrupted (DSH repair semantics)"

# ---------------------------------------------------------------- summary
echo
if [ "$FAILS" -gt 0 ]; then
  printf '\033[31m%d gate(s) FAILED\033[0m\n' "$FAILS"
  exit 1
else
  printf '\033[32mall gates passed\033[0m\n'
  exit 0
fi
