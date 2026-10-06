#!/bin/bash
#
# scripts/apply-build-profile.sh — 幂等地把 fleet 构建策略写进本仓的 Cargo.toml。
#
# 策略（2026-10-07 实测得出，逐条有数字）：
#   [profile.dev] incremental = false
#       incremental 目录是本机最大的一类不可回收产物（cantool 4.6 GB / 全机 5.16 GB），
#       而且 cargo 从不清理它；关掉后单 crate 重编约 0.29 s → 0.69 s（小仓实测），
#       换来的是不再无限增长 + sccache 能缓存这些单元。
#   [profile.dev.package."*"] debug = 0
#       依赖的调试信息既是磁盘大头也是 codegen 大头：verify-gate 实测全新 target
#       372 MB → 225 MB（−40%），冷编墙钟 −12%；自身 crate 仍保留可调试性。
#   [profile.dev.package."*"] strip = "none"
#       保险丝。`debug = 0` 会让 cargo 传 `-C strip=debuginfo`（本机实测 98 处），
#       而 Apple 的 strip 在特定组合下会破坏 proc-macro dylib（dlopen:
#       mis-aligned LINKEDIT string pool）。cantool 为此显式写了 strip = "none"；
#       它不增加体积（debug 已经不生成），只是关掉那次 strip。
#
# 幂等：已存在的键不重复写；已存在 `[profile.dev.package."*"]` 且带自定义键的仓
# （例如 cantool 的 opt-level=1 / strip="none" 调优）只报告、不覆盖。
#
# 用法：
#   ./scripts/apply-build-profile.sh            # 预览（默认）
#   ./scripts/apply-build-profile.sh --apply    # 写入
#   ./scripts/apply-build-profile.sh --json     # 机器可读
#
# 退出码：0 已满足或已应用 · 1 错误 · 2 用法错误 · 3 需要人工决策（检测到自定义 deps profile）

set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

APPLY=0; JSON=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) APPLY=1; shift ;;
        --json) JSON=1; shift ;;
        -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

MANIFEST=""
for cand in "${ROOT}/Cargo.toml" "${ROOT}/src-tauri/Cargo.toml"; do
    [[ -f "$cand" ]] && { MANIFEST="$cand"; break; }
done
[[ -n "$MANIFEST" ]] || { echo "no Cargo.toml at repo root or src-tauri/" >&2; exit 1; }

python3 - "$MANIFEST" "$APPLY" "$JSON" "$ROOT" <<'PY'
import json, re, sys

manifest, apply_, json_, root = sys.argv[1], sys.argv[2] == "1", sys.argv[3] == "1", sys.argv[4]
text = open(manifest).read()
orig = text
changes, notes = [], []

def section_span(name):
    """Return (start, end) line indices of a [section] block, or None."""
    lines = text.splitlines(keepends=True)
    start = None
    for i, ln in enumerate(lines):
        if re.match(r"^\[%s\]\s*$" % re.escape(name), ln):
            start = i
            break
    if start is None:
        return None
    end = len(lines)
    for j in range(start + 1, len(lines)):
        if re.match(r"^\[", lines[j]):
            end = j
            break
    return (start, end)

def has_key(span, key):
    s, e = span
    return any(re.match(r"^\s*%s\s*=" % re.escape(key), l) for l in lines[s + 1:e])

lines = text.splitlines(keepends=True)

# --- [profile.dev] incremental = false -------------------------------------
span = section_span("profile.dev")
if span is None:
    lines.append("\n[profile.dev]\nincremental = false\n")
    changes.append("append [profile.dev] incremental = false")
else:
    if has_key(span, "incremental"):
        s, e = span
        val = next(l for l in lines[s + 1:e] if re.match(r"^\s*incremental\s*=", l))
        cur = val.split("=", 1)[1].strip()
        if cur == "false":
            notes.append("profile.dev.incremental already false")
        else:
            lines[s + 1] = re.sub(r"^(\s*incremental\s*=\s*).*$", r"\1false", lines[s + 1], count=1)
            changes.append("set profile.dev.incremental false (was %s)" % cur)
    else:
        s = span[0]
        lines.insert(s + 1, "incremental = false\n")
        changes.append("insert profile.dev.incremental = false")

text = "".join(lines)
lines = text.splitlines(keepends=True)

# --- [profile.dev.package."*"] debug = 0 / strip = "none" ------------------
span = section_span('profile.dev.package."*"')
if span is None:
    lines.append('\n[profile.dev.package."*"]\n'
                 '# deps: no debuginfo (biggest dev-build disk lever; measured 372 -> 225 MB here).\n'
                 'debug = 0\n'
                 '# debug = 0 makes cargo pass `-C strip=debuginfo`; Apple strip can break proc-macro\n'
                 '# dylibs (dlopen: mis-aligned LINKEDIT string pool). Free insurance, no size cost.\n'
                 'strip = "none"\n')
    changes.append('append [profile.dev.package."*"] debug = 0 + strip = "none"')
else:
    s, e = span
    body = "".join(lines[s + 1:e])
    extra = [l for l in lines[s + 1:e]
             if re.match(r"^\s*[a-z-]+\s*=", l) and not re.match(r'^\s*(debug|strip)\s*=', l)]
    if extra:
        notes.append('existing [profile.dev.package."*"] has custom keys (%d) — left untouched'
                     % len(extra))
        print(json.dumps({"manifest": manifest, "action": "needs-review",
                          "custom_keys": [l.strip() for l in extra], "changes": [],
                          "notes": notes}) if json_ else
              "  needs-review: [profile.dev.package.\"*\"] has custom keys:\n    " +
              "\n    ".join(l.strip() for l in extra), file=sys.stdout)
        sys.exit(3)
    if not re.search(r"^\s*debug\s*=", body, re.M):
        lines.insert(s + 1, "debug = 0\n"); changes.append('insert package."*".debug = 0')
    if not re.search(r"^\s*strip\s*=", body, re.M):
        idx = s + 1 + (1 if 'debug' in "".join(lines[s + 1:s + 3]) else 0)
        lines.insert(idx, 'strip = "none"\n'); changes.append('insert package."*".strip = "none"')

text = "".join(lines)

import os
toolchain = os.path.exists(os.path.join(os.path.dirname(manifest), "rust-toolchain.toml"))
if not toolchain:
    notes.append("no rust-toolchain.toml (toolchain drift source; CI may pin a different version)")

if apply_ and changes:
    open(manifest, "w").write(text)

if json_:
    print(json.dumps({"manifest": manifest, "changes": changes, "notes": notes,
                      "applied": bool(apply_ and changes)}))
else:
    print("apply-build-profile — %s" % manifest)
    for c in changes:
        print("  %s %s" % ("applied:" if apply_ else "would:", c))
    for n in notes:
        print("  note: %s" % n)
    if not changes:
        print("  already compliant")
PY
