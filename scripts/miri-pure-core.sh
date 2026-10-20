#!/usr/bin/env bash
#
# Miri gate for a repository's *syscall-free pure core*.
#
# Canonical source: <dsfolder>/scripts/miri-pure-core/miri-pure-core.sh
# Installed copy:   <repo>/scripts/miri-pure-core.sh
# Repo-owned config: <repo>/scripts/miri-pure-core.conf
#
# Why a subset and not the whole crate: Miri cannot execute FFI or real
# syscalls, so anything that spawns a process, reads /proc, opens a socket or
# touches the filesystem is out of scope by construction. What is left is the
# part a machine can actually prove — pure data transforms and decision logic
# (parsers, policy matching, budget arithmetic, serialization invariants). The
# shell around it stays covered by integration tests and real-machine acceptance.
#
# The engine is identical in every repo; only the conf differs (filters, test
# floor, target). `install.sh --check` compares the installed engine's hash
# against this file, so the copies cannot silently diverge.
#
# Exit codes (three states, never collapsed):
#   0  pass             — Miri ran the filters, no UB, test floor met
#   1  fail             — UB found, a test failed, or the floor was not met
#   2  inconclusive     — could not judge (no Miri, no sysroot, no config, ...)
#
# Exit 2 is *not* a pass. A caller that treats it as one rebuilds the
# "environment failure disguised as a product defect" bug this workspace already
# paid for once (2026-10-08: `cargo: command not found` recorded as drift).
#
# Usage:
#   scripts/miri-pure-core.sh              # the gate
#   scripts/miri-pure-core.sh --self-test  # positive/negative controls of the judge
#   scripts/miri-pure-core.sh --print      # resolved config (conf path, filters, floor)
#
# Environment overrides (all optional):
#   CARGO              cargo to use (default: PATH, then ~/.cargo/bin, then Homebrew)
#   MIRI_CONF          path to the conf (default: <repo>/scripts/miri-pure-core.conf)
#   MIRI_TOOLCHAIN     rustup toolchain for Miri (default from conf, else nightly)
#   MIRI_SYSROOT       prebuilt Miri sysroot (default: search the known caches)
#   MIRIFLAGS          Miri flags (default from conf, else -Zmiri-tree-borrows)
#   MIRI_MIN_PASSED    test floor (default from conf)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF="${MIRI_CONF:-$SCRIPT_DIR/miri-pure-core.conf}"

say() { printf '%s\n' "$*"; }

inconclusive() {
    local reason="$1"
    shift
    say "miri-pure-core: verdict=inconclusive reason=$reason"
    local line
    for line in "$@"; do say "  $line"; done
    exit 2
}

# --- capability probing ----------------------------------------------------
# Never `command -v cargo` alone: a launchd/CI shell can have a PATH without
# ~/.cargo/bin, and the resulting "not found" looks exactly like a broken repo.
resolve_cargo() {
    local candidate
    for candidate in "${CARGO:-}" "$(command -v cargo 2>/dev/null || true)" \
        "$HOME/.cargo/bin/cargo" /opt/homebrew/bin/cargo /usr/local/bin/cargo; do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

resolve_sysroot() {
    local dir
    if [ -n "${MIRI_SYSROOT:-}" ]; then
        printf '%s' "$MIRI_SYSROOT"
        return 0
    fi
    for dir in "$HOME/Library/Caches/org.rust-lang.miri" "$HOME/.cache/miri"; do
        if [ -d "$dir/lib" ]; then
            printf '%s' "$dir"
            return 0
        fi
    done
    return 1
}

# --- repo-owned config -----------------------------------------------------
# The conf is the only thing that differs per repo. It is sourced, so it can
# express arrays, and it is validated below: an installed engine with no conf
# must not guess what to run.
MIRI_FILTERS=()
MIRI_MIN_PASSED=""
MIRI_TARGET="bins"
MIRI_TOOLCHAIN="nightly"
MIRI_FLAGS="-Zmiri-tree-borrows"

load_conf() {
    [ -f "$CONF" ] || inconclusive "no-config" \
        "expected $CONF" \
        "an installed gate without a conf would have to guess its filters, and a wrong guess runs the wrong tests silently"
    # shellcheck disable=SC1090
    . "$CONF" || inconclusive "config-unreadable" "failed to source $CONF"
    if [ "${#MIRI_FILTERS[@]}" -eq 0 ]; then
        inconclusive "config-empty-filters" \
            "$CONF declares no MIRI_FILTERS" \
            "empty filters make libtest run every test in the target, including ones that need syscalls"
    fi
    case "$MIRI_MIN_PASSED" in
    '' | *[!0-9]*) inconclusive "config-bad-floor" "MIRI_MIN_PASSED must be a positive integer, got '$MIRI_MIN_PASSED'" ;;
    esac
    if [ "$MIRI_MIN_PASSED" -lt 1 ]; then
        inconclusive "config-bad-floor" "MIRI_MIN_PASSED must be >= 1, got $MIRI_MIN_PASSED"
    fi
    case "$MIRI_TARGET" in
    lib) MIRI_TARGET_FLAG="--lib" ;;
    bins) MIRI_TARGET_FLAG="--bins" ;;
    all) MIRI_TARGET_FLAG="" ;;
    *) inconclusive "config-bad-target" "MIRI_TARGET must be lib|bins|all, got '$MIRI_TARGET'" ;;
    esac
}

# `#[cfg(miri)]` / `cfg!(miri)` branches are how a gate turns into "green but
# untested": code that only exists under Miri never runs in production, and code
# that vanishes under Miri hides the very paths the gate claims to prove.
# Zero tolerance — none exist today, so any appearance is a deliberate act.
scan_ub_holes() {
    local hits
    hits="$(cd "$REPO_ROOT" && grep -rn -E 'cfg\(miri\)|cfg!\(miri\)|cfg_attr\(miri' src tests 2>/dev/null || true)"
    if [ -n "$hits" ]; then
        printf '%s\n' "$hits"
        return 1
    fi
    return 0
}

# --- judgement (pure: log + exit code in, verdict out) ---------------------
# Kept separate from execution so `--self-test` can feed it synthetic logs. The
# ordering matters: UB text wins, then an explicit libtest failure, then a
# non-zero exit (a non-zero exit must never be readable as a pass just because
# the log happens to contain a happy "test result" line), and only then the
# counts — including "no result line at all", which is exit 2, not 0.
# judge <log> <rc> [floor] → "<code> <verdict> <reason>"
judge() {
    local log="$1" rc="$2" floor="${3:-${MIRI_MIN_PASSED:-1}}" passed failed
    if grep -q 'Undefined Behavior' "$log" 2>/dev/null; then
        printf '1 ub undefined-behavior'
        return 0
    fi
    if grep -q 'test result: FAILED' "$log" 2>/dev/null; then
        printf '1 fail test-failed'
        return 0
    fi
    if [ "$rc" -ne 0 ]; then
        printf '2 inconclusive exit-rc%s' "$rc"
        return 0
    fi
    passed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) passed.*/\1/p' "$log" | tail -1)"
    failed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) failed.*/\1/p' "$log" | tail -1)"
    if [ -z "$passed" ]; then
        # No libtest summary line = nothing was measured. This is the "empty
        # gate = false confidence" case; it must not be reachable as a pass.
        printf '2 inconclusive no-test-result-line'
        return 0
    fi
    if [ "${failed:-0}" -ne 0 ]; then
        printf '1 fail failed-count-%s' "${failed:-0}"
        return 0
    fi
    if [ "$passed" -lt "$floor" ]; then
        printf '1 fail below-floor-%s-of-%s' "$passed" "$floor"
        return 0
    fi
    printf '0 pass ok'
}

# --- modes -----------------------------------------------------------------
print_config() {
    local cargo sysroot
    cargo="$(resolve_cargo || true)"
    sysroot="$(resolve_sysroot || true)"
    say "repo=$REPO_ROOT"
    say "conf=$CONF"
    say "cargo=${cargo:-<not found>}"
    say "toolchain=+$MIRI_TOOLCHAIN"
    say "target=$MIRI_TARGET"
    say "miri_flags=$MIRI_FLAGS"
    say "sysroot=${sysroot:-<unset: cargo-miri will run its own setup>}"
    say "filters=${MIRI_FILTERS[*]}"
    say "min_passed=$MIRI_MIN_PASSED"
}

self_test() {
    local tmp log code got failures=0 checks=0 expect
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/miri-self-test.XXXXXX")" || inconclusive "no-temp-dir"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" EXIT

    say 'miri-pure-core: self-test — judge() on synthetic logs'

    expect_case() {
        local name="$1" logfile="$2" rc="$3" want="$4"
        checks=$((checks + 1))
        got="$(judge "$logfile" "$rc")"
        if [ "$got" = "$want" ]; then
            say "  ok   $name -> $got"
        else
            say "  FAIL $name -> got '$got', want '$want'"
            failures=$((failures + 1))
        fi
    }

    printf 'error: Undefined Behavior: memory access failed\n' >"$tmp/ub.log"
    expect_case 'undefined behaviour is a failure' "$tmp/ub.log" 1 '1 ub undefined-behavior'

    printf 'test result: FAILED. 3 passed; 1 failed; 0 ignored\n' >"$tmp/failed.log"
    expect_case 'libtest failure is a failure' "$tmp/failed.log" 1 '1 fail test-failed'

    printf 'test result: ok. %s passed; 0 failed; 0 ignored\n' "$MIRI_MIN_PASSED" >"$tmp/ok.log"
    expect_case 'floor met is a pass' "$tmp/ok.log" 0 '0 pass ok'

    # The control this engine exists for: an empty log with a zero exit code.
    # Treating that as a pass is how a gate stops measuring anything.
    : >"$tmp/empty.log"
    expect_case 'empty log is not a pass' "$tmp/empty.log" 0 '2 inconclusive no-test-result-line'

    # A non-zero exit must never be laundered into a pass by a stray summary line.
    expect_case 'rc!=0 with a happy line is inconclusive' "$tmp/ok.log" 1 '2 inconclusive exit-rc1'

    printf 'error: no such command: `miri`\n' >"$tmp/nomiri.log"
    expect_case 'missing miri is inconclusive' "$tmp/nomiri.log" 101 '2 inconclusive exit-rc101'

    # Below floor, derived from the actual floor so the case survives a repo
    # with a different test count.
    local below=$((MIRI_MIN_PASSED - 1))
    if [ "$below" -lt 0 ]; then below=0; fi
    printf 'test result: ok. %s passed; 0 failed; 0 ignored\n' "$below" >"$tmp/floor.log"
    expect_case 'below floor is a failure' "$tmp/floor.log" 0 \
        "1 fail below-floor-$below-of-$MIRI_MIN_PASSED"

    say 'miri-pure-core: self-test — real Miri on throwaway crates'
    local cargo sysroot
    cargo="$(resolve_cargo)" || inconclusive "cargo-not-found" \
        "probed: \$CARGO, PATH, ~/.cargo/bin, /opt/homebrew/bin, /usr/local/bin"
    sysroot="$(resolve_sysroot || true)"
    [ -n "$sysroot" ] && export MIRI_SYSROOT="$sysroot"
    if ! "$cargo" "+$MIRI_TOOLCHAIN" miri --version >"$tmp/version.log" 2>&1; then
        inconclusive "miri-not-installed" \
            "remediation: rustup +$MIRI_TOOLCHAIN component add miri && $cargo +$MIRI_TOOLCHAIN miri setup"
    fi
    say "  miri: $(tail -1 "$tmp/version.log")"

    # Positive control: a clean crate must come back green, otherwise a broken
    # harness would make the UB control below pass for the wrong reason.
    checks=$((checks + 1))
    "$cargo" new --lib --vcs none --name miri_clean "$tmp/clean" >/dev/null 2>&1
    cat >>"$tmp/clean/src/lib.rs" <<'RS'
#[cfg(test)]
mod t {
    #[test]
    fn clean() {
        let mut v = vec![1u8, 2, 3];
        v.push(4);
        assert_eq!(v.iter().sum::<u8>(), 10);
    }
}
RS
    ( cd "$tmp/clean" && MIRIFLAGS="$MIRI_FLAGS" "$cargo" "+$MIRI_TOOLCHAIN" miri test --lib ) \
        >"$tmp/clean.log" 2>&1
    code=$?
    got="$(judge "$tmp/clean.log" "$code" 1)"
    if [ "${got%% *}" = '0' ]; then
        say "  ok   clean crate -> $got"
    else
        say "  FAIL clean crate -> $got (a Miri that cannot go green cannot be trusted to go red)"
        tail -3 "$tmp/clean.log"
        failures=$((failures + 1))
    fi

    # Negative control: real UB must be reported. `get_unchecked` past the end is
    # deliberate — Miri catches it deterministically under tree borrows.
    checks=$((checks + 1))
    "$cargo" new --lib --vcs none --name miri_ub "$tmp/ub" >/dev/null 2>&1
    cat >>"$tmp/ub/src/lib.rs" <<'RS'
#[cfg(test)]
mod t {
    #[test]
    fn ub() {
        let bytes = [1u8, 2, 3];
        let bad = unsafe { *bytes.as_ptr().add(3) };
        assert_eq!(bad, 0);
    }
}
RS
    ( cd "$tmp/ub" && MIRIFLAGS="$MIRI_FLAGS" "$cargo" "+$MIRI_TOOLCHAIN" miri test --lib ) \
        >"$tmp/ub-run.log" 2>&1
    code=$?
    got="$(judge "$tmp/ub-run.log" "$code" 1)"
    case "$got" in
    1\ ub\ undefined-behavior*) say "  ok   injected UB -> $got" ;;
    *)
        say "  FAIL injected UB -> $got (expected '1 ub undefined-behavior')"
        failures=$((failures + 1))
        ;;
    esac

    checks=$((checks + 1))
    if scan_ub_holes >"$tmp/holes.log" 2>&1; then
        say '  ok   no cfg(miri) escape holes'
    else
        say '  FAIL cfg(miri) escape holes present'
        sed -n '1,5p' "$tmp/holes.log"
        failures=$((failures + 1))
    fi

    if [ "$failures" -ne 0 ]; then
        say "miri-pure-core: verdict=fail reason=self-test checks=$checks failures=$failures"
        exit 1
    fi
    say "miri-pure-core: verdict=pass reason=self-test checks=$checks failures=0"
    exit 0
}

# --- main ------------------------------------------------------------------
case "${1:-}" in
--print)
    load_conf
    print_config
    exit 0
    ;;
--self-test)
    load_conf
    self_test
    ;;
'')
    load_conf
    ;;
*)
    inconclusive "unknown-argument" "got '$1'; expected no argument, --print or --self-test"
    ;;
esac

if ! cargo_bin="$(resolve_cargo)"; then
    inconclusive "cargo-not-found" \
        "probed: \$CARGO, PATH, ~/.cargo/bin, /opt/homebrew/bin, /usr/local/bin"
fi

if sysroot="$(resolve_sysroot)"; then
    export MIRI_SYSROOT="$sysroot"
else
    say 'miri-pure-core: note: no prebuilt Miri sysroot found; cargo-miri will build one (needs rust-src)'
fi
export MIRIFLAGS="$MIRI_FLAGS"

if ! "$cargo_bin" "+$MIRI_TOOLCHAIN" miri --version >/dev/null 2>&1; then
    inconclusive "miri-not-installed" \
        "remediation: rustup +$MIRI_TOOLCHAIN component add miri && $cargo_bin +$MIRI_TOOLCHAIN miri setup"
fi

holes_file="$(mktemp "${TMPDIR:-/tmp}/miri-holes.XXXXXX")"
if ! scan_ub_holes >"$holes_file" 2>&1; then
    say 'miri-pure-core: verdict=fail reason=cfg-miri-escape-hole'
    say '  a #[cfg(miri)] branch means the gate proves a different program than the one that ships'
    sed -n '1,10p' "$holes_file"
    rm -f "$holes_file"
    exit 1
fi
rm -f "$holes_file"

log="$(mktemp "${TMPDIR:-/tmp}/miri-pure-core.XXXXXX")"
trap 'rm -f "$log"' EXIT

say "miri-pure-core: filters: ${MIRI_FILTERS[*]} (target=$MIRI_TARGET, min_passed=$MIRI_MIN_PASSED, flags=$MIRIFLAGS)"
target_args=()
[ -n "$MIRI_TARGET_FLAG" ] && target_args=("$MIRI_TARGET_FLAG")
( cd "$REPO_ROOT" && "$cargo_bin" "+$MIRI_TOOLCHAIN" miri test "${target_args[@]}" -- "${MIRI_FILTERS[@]}" ) >"$log" 2>&1
rc=$?

read -r code verdict reason <<<"$(judge "$log" "$rc")"

case "$code" in
0)
    passed="$(sed -n 's/^test result: .* \([0-9][0-9]*\) passed.*/\1/p' "$log" | tail -1)"
    say "miri-pure-core: verdict=pass passed=$passed floor=$MIRI_MIN_PASSED"
    ;;
1)
    say "miri-pure-core: verdict=fail reason=$verdict/$reason"
    grep -E 'Undefined Behavior|^test result|panicked at|^error' "$log" | head -20
    ;;
*)
    say "miri-pure-core: verdict=inconclusive reason=$verdict/$reason"
    say '  this is not a pass: the gate could not judge the pure core'
    tail -20 "$log"
    ;;
esac
exit "$code"
