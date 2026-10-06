#!/bin/bash
#
# scripts/clean-build-artifacts.sh — reclaim a repo's Rust build artifacts, safely.
#
# 为什么要有它（2026-10-07 实测）：
#   * 本机 `target/` 已经长到 26.5 GB（cantool/src-tauri 一项 11 GB）；体积由三个类构成：
#     构建中的工作集（deps/）、incremental、以及被取代的旧产物。三类里只有后两类能删而不
#     "必然重编更多"——但用户要的是"清干净、建立基线"，所以本脚本清的是**整个 target 树**，
#     并把每一类的体积先打印出来（du 口径，按 inode 去重；逻辑字节会把硬链接重复计数，
#     cargo 在 deps/ 里大量使用硬链接，实测虚高可达 1.4×）。
#   * 清理必须**在构建结束后**做。cargo 在 `target/<profile>/.cargo-lock` 上持有 flock；
#     本脚本用 Python 的 fcntl 做**非阻塞探测**（macOS 没有 flock(1)），拿不到锁就拒绝执行
#     （fail-closed），而不是"删了再说"。python3 不可用时也拒绝，除非显式 --force。
#   * 两段式：默认只预览，`--apply` 才删；每次执行追加 JSONL 台账。
#
# 与本仓其它清理工具的分工（不是替代关系）：
#   * `scripts/cleanup.sh`          —— 更丰富的入口：cruft/debug/release/profile/target/
#                                      dist/artifacts/node_modules/logs/all，含隔离区与回滚。
#   * `scripts/build-cache-sweep.sh` —— 只删"可证明已死"的 incremental 目录（外科手术），
#                                      可叠加 cargo-sweep 处理过期产物。
#   * 本脚本                        —— 最小、可移植、带闸门的"清整个 target 并报体积"，
#                                      同样适用于没有上述工具的仓库（fleet 标准）。
#
# 用法：
#   ./scripts/clean-build-artifacts.sh                 # 预览（默认，只报体积与将删项）
#   ./scripts/clean-build-artifacts.sh --apply         # 真正删除
#   ./scripts/clean-build-artifacts.sh --json          # 机器可读输出（基线文档用）
#   ./scripts/clean-build-artifacts.sh --ledger PATH   # 台账路径（默认 ~/.cache/build-clean/ledger.jsonl）
#   ./scripts/clean-build-artifacts.sh --with-frontend # 额外删 frontend/dist 与 dist/（注意：可能含构建 stamp）
#   ./scripts/clean-build-artifacts.sh --force         # 跳过闸门（危险；仅在你确认没有构建时）
#
# 退出码：0 正常（预览或已执行）· 1 运行错误 · 2 用法错误 · 3 拒绝执行（构建进行中/环境不安全）
#
# 明确不碰：~/.cargo、~/.rustup、.git、.jj、node_modules（除 --with-frontend 的 .vite）、
#           用户数据、artifacts/（发布产物走 cleanup.sh artifacts）。

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

APPLY=0
JSON=0
FORCE=0
WITH_FRONTEND=0
LEDGER="${HOME}/.cache/build-clean/ledger.jsonl"

usage() { sed -n '2,40p' "${BASH_SOURCE[0]}"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) APPLY=1; shift ;;
        --dry-run) APPLY=0; shift ;;
        --json) JSON=1; shift ;;
        --force) FORCE=1; shift ;;
        --with-frontend) WITH_FRONTEND=1; shift ;;
        --ledger) LEDGER="${2:?--ledger needs a path}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

# ── helpers ────────────────────────────────────────────────────────────────
du_k() { du -sk "$1" 2>/dev/null | awk '{print $1}'; }
human() { awk -v k="${1:-0}" 'BEGIN { printf "%.1f MB", k / 1024 }'; }

# lock_probe_free <lockfile> — exit 0 when NO build holds the lock.
lock_probe_free() {
    python3 - "$1" <<'PY' 2>/dev/null
import fcntl, sys
f = open(sys.argv[1], "a+")
try:
    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    sys.exit(1)
PY
}

# build_in_progress <targetdir> — prints the held lock path and returns 0 if a build owns it.
build_in_progress() {
    local t="$1" lock
    for lock in "$t"/.cargo-lock "$t"/*/.cargo-lock; do
        [[ -e "$lock" ]] || continue
        if ! lock_probe_free "$lock"; then
            printf '%s\n' "$lock"
            return 0
        fi
    done
    return 1
}

# print_breakdown <targetdir> — per-class du table (the number that matters).
print_breakdown() {
    local t="$1" d name
    for d in "$t"/*/; do
        [[ -d "$d" ]] || continue
        name="$(basename "$d")"
        printf '      %-24s %10s\n' "$name" "$(human "$(du_k "$d")")"
        case "$name" in
            debug|release|test|bench|profiling|dist)
                for sub in incremental deps build examples; do
                    [[ -d "${d%/}/$sub" ]] || continue
                    printf '        %-22s %10s\n' "$name/$sub" "$(human "$(du_k "${d%/}/$sub")")"
                done
                ;;
        esac
    done
    return 0
}

# ── discover Rust target dirs (bash 3.2-safe: no mapfile) ──────────────────
targets_file="$(mktemp "${TMPDIR:-/tmp}/clean-build-artifacts.XXXXXX")"
trap 'rm -f "${targets_file}"' EXIT
find "${ROOT}" -maxdepth 4 -type d -name target \
    -not -path '*/node_modules/*' -not -path '*/.git/*' -not -path '*/.jj/*' \
    -not -path '*/target/*' 2>/dev/null | sort > "${targets_file}"

[[ "${JSON}" -eq 1 ]] || {
    echo "clean-build-artifacts — repo: ${ROOT}"
    echo "  mode: $([[ "${APPLY}" -eq 1 ]] && echo apply || echo 'dry-run (default)')"
    echo
}

total_k=0
found=0
refused=0
json_items=""

while IFS= read -r t; do
    [[ -n "$t" ]] || continue
    found=$((found + 1))
    real="$(cd "$t" && pwd -P)"
    case "${real}" in
        "${ROOT}"/*) : ;;
        *) echo "refusing: ${t} resolves outside the repo (${real})" >&2; exit 3 ;;
    esac

    k="$(du_k "${real}")"
    total_k=$((total_k + k))

    if [[ "${FORCE}" -eq 0 ]]; then
        if holder="$(build_in_progress "${real}")"; then
            echo "refusing: a build holds ${holder}" >&2
            echo "          (wait for it to finish, or pass --force if you know it is stale)" >&2
            refused=1
            continue
        fi
    fi

    rel="${real#${ROOT}/}"
    [[ "${JSON}" -eq 1 ]] || {
        printf '  %-32s %10s\n' "${rel}" "$(human "${k}")"
        print_breakdown "${real}"
    }
    json_items="${json_items}${json_items:+,}{\"target\":\"${rel}\",\"bytes_du\":$((k * 1024))}"

    if [[ "${APPLY}" -eq 1 ]]; then
        rm -rf "${real}"
        [[ "${JSON}" -eq 1 ]] || echo "      → removed"
    fi
done < "${targets_file}"

if [[ "${WITH_FRONTEND}" -eq 1 ]]; then
    for d in "${ROOT}/frontend/dist" "${ROOT}/dist" "${ROOT}/node_modules/.vite" "${ROOT}/frontend/node_modules/.vite"; do
        [[ -d "$d" ]] || continue
        k="$(du_k "$d")"; total_k=$((total_k + k))
        [[ "${JSON}" -eq 1 ]] || printf '  %-32s %10s%s\n' "${d#${ROOT}/}" "$(human "${k}")" "$([[ "${APPLY}" -eq 1 ]] && echo '  → removed' || echo '')"
        json_items="${json_items}${json_items:+,}{\"target\":\"${d#${ROOT}/}\",\"bytes_du\":$((k * 1024))}"
        if [[ "${APPLY}" -eq 1 ]]; then
            rm -rf "$d"
        fi
    done
fi

if [[ "${refused}" -eq 1 && "${APPLY}" -eq 1 ]]; then
    echo "aborted: at least one target is being built; nothing was applied for it" >&2
fi

if [[ "${JSON}" -eq 1 ]]; then
    printf '{"repo":"%s","machine":"%s","mode":"%s","targets_found":%d,"bytes_du":%d,"items":[%s]}\n' \
        "${ROOT}" "$(hostname -s)" "$([[ "${APPLY}" -eq 1 ]] && echo apply || echo dry-run)" \
        "${found}" "$((total_k * 1024))" "${json_items}"
else
    echo
    printf '  %-32s %10s\n' "TOTAL ($([[ "${APPLY}" -eq 1 ]] && echo removed || echo reclaimable))" "$(human "${total_k}")"
    if [[ "${APPLY}" -eq 0 && "${found}" -gt 0 ]]; then
        echo
        echo "  preview only — re-run with --apply to remove"
        echo "  (不碰 .git/.jj/node_modules/artifacts/~/.cargo；细档清理见 scripts/build-cache-sweep.sh)"
    fi
fi

# ── ledger ─────────────────────────────────────────────────────────────────
if [[ "${APPLY}" -eq 1 ]]; then
    mkdir -p "$(dirname "${LEDGER}")"
    printf '{"ts":"%s","op":"clean-build-artifacts","repo":"%s","machine":"%s","targets":%d,"bytes_du":%d}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${ROOT}" "$(hostname -s)" "${found}" "$((total_k * 1024))" >> "${LEDGER}"
fi

if [[ "${refused}" -eq 1 ]]; then
    exit 3
fi
exit 0
