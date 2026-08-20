# AGENTS.md — sandbox-run

面向编码 agent（DSH / Codex / Claude Code / Cursor）与自动化脚本的项目约定。

## 项目概览

- `sandbox-run`：变更隔离验证执行内核（ChangeSet → Sandbox → RunReport）。
- 纯 Rust（edition 2021），单二进制；`src/` 为模块 + `src/main.rs`。
- 测试：`cargo test`（单元）+ `bash test/gates.sh`（7 条机械验收门禁，git+jj）。

## 提交前：Rust 格式化门禁（必须）

修改 Rust 代码后，**不要用 `cargo fmt`**（会重排整个 workspace）。用 `fmtguard`
（scoped, gated rustfmt，本机 `~/.cargo/bin/fmtguard`）：

```sh
# 1. dry-run 检查范围（只应覆盖你编辑的文件/区域）
fmtguard --scope-from-git --emit patch
# 2. 确认后应用
fmtguard --scope-from-git --apply
# 3. 机械检查
fmtguard --scope-from-git --emit json   # verdict 必须是 "ok"
git diff --check
cargo fmt --check                       # 红 = scope 外有格式债，整文件 cargo fmt 补齐
```

## 行为契约（改动前必读）

- **exit 契约**：`0` pass · `1` fail/timeout/polluted/rejected · `2` error
  （用法/VCS/沙箱 setup/G0 隔离破坏）。G0 失败 = bug 信号，永远 exit 2。
- **G0 隔离完整性**：主树 VCS 状态运行前后必须字节一致（台账路径除外——
  它是设计内唯一写盘路径）；沙箱必须清理（残留即 bug，验收断言）。
- **G1 沙箱污染**：verify 修改物化快照 → `polluted`（默认 deny；`--pollution warn` 降级）。
- **沙箱内容 = 当前工作状态**：按清单拷贝（非 patch 应用、非 hardlink/symlink）；
  缓存目录（target/node_modules/dist/.venv/__pycache__/.pytest_cache）排除 =
  clean-verify（这是特性不是缺陷）。
- **台账**：`.sandbox-run/runs.jsonl` 追加 JSONL 是唯一真相；status/log 是派生视图；
  悬空 run.start（无 report.emit）合成 `interrupted`（镜像 DSH repair）。

## 约定

- 分支：`main`；conventional commits。
- 验证命令 = argv 直接 spawn（不经 shell）；管道请显式 `sh -c '...'`。
- jj 后端（workspace add/forget/abandon）：op log 增长是已记录的可接受成本；
  change id 跨 snapshot 稳定，cleanup 用 add 时记录的 id abandon 孤儿。
- 改 `src/` 前先想清楚它属于哪一层（scope/sync/sandbox/exec/gates/events/report），
  保持单二进制、零 daemon、零外部依赖（git/jj 二进制除外）。
