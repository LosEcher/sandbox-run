# sandbox-run

**Isolated verification execution for AI agents.** Run build/test/lint/typecheck
commands inside a git worktree or jj workspace whose content mirrors your current
working state — so the main working tree is **never polluted** and a `pass`
proves a **clean tree** passes.

```
sandbox-run -- cargo test
```

## Why

When an agent (or a human) runs verification directly in the working tree:

1. **Verification pollutes the tree** — build artifacts, generated files and
   cache changes mix into the change set (mis-commits, dsh-undo snapshot
   deadlocks, "what do I even commit?").
2. **Verification is untrustworthy** — running on an already-dirtied tree, a
   `pass` cannot prove a clean tree passes (missing new files / missing deps
   pass locally and explode in CI).
3. **No isolation, no replay** — the result is just a stdout blob; nothing
   records *in what state, over which files, how long, timed out or killed*.
4. **No gates** — non-zero exits / timeouts / out-of-scope writes are not
   mechanically classified.

The root cause is the **execution location**, not the command: verification
should run in a sandbox that mirrors the working state. `sandbox-run` makes that
a one-liner with an event-sourced audit ledger.

## How it works

```
ChangeSet (explicit or git/jj diff) → Sandbox (worktree/workspace + overlay)
→ verify command (timeout + whole-tree kill) → gates → RunReport + runs.jsonl
```

- **L1 scope**: `--scope-from-git`, `--scope-from-jj`, or an explicit
  `--changeset`; cache dirs (`target/`, `node_modules/`, `dist/`, ...) are
  excluded so the sandbox **clean-rebuilds** them — a pass catches
  "forgot to commit the new file" exactly like CI does.
- **L2 sandbox**: `git worktree add --detach` / `jj workspace add` in a temp
  dir, your working state overlaid **by manifest copy** (never patch-apply,
  never hardlink/symlink — writes cannot leak back into the main tree).
  Verify runs with an in-process deadline; on timeout the **whole process tree
  is killed** (own process group → SIGTERM → SIGKILL).
- **L3 gates** (all exit-non-zero):
  - `isolation.integrity` — main tree VCS state byte-identical before/after,
    sandbox cleaned (failure = exit 2, a bug signal);
  - `sandbox.clean` — verify must not modify the materialized snapshot
    (default `deny` → `polluted`; `--pollution warn` downgrades);
  - classification → `pass | fail | timeout | polluted | rejected`.
- **L4 ledger**: append-only `.sandbox-run/runs.jsonl` is the single source of
  truth; `status` and `log` are derived views. A dangling `run.start` without a
  report is repaired as `interrupted` (crash recovery).

## Install

```sh
cargo install sandbox-run-cli    # binary: sandbox-run
```

> crates.io packages the binary under `sandbox-run-cli` (same pattern as
> `fd` → `fd-find`): the bare name `sandbox-run` on crates.io is an unrelated
> process-sandboxing library, so `cargo install sandbox-run` would install the
> wrong crate. Prefer the
> [GitHub Releases](https://github.com/LosEcher/sandbox-run/releases)
> binaries (`sandbox-run-<os>-<arch>`) for a static artifact.

## Usage

```
sandbox-run [OPTIONS] -- <verify-command...>
```

| Option | Default | Meaning |
|---|---|---|
| `--scope-from-git` / `--scope-from-jj` | auto-detect | force the VCS |
| `--changeset <file.json>` | — | explicit scope `{"base_ref":"HEAD","files":[...]}` |
| `--base <ref>` | `HEAD` | git base for detection + worktree (jj: not in P0) |
| `--timeout <secs>` | `120` | deadline, then whole-tree kill |
| `--env K=V` | — | extra env for the verify command (repeatable) |
| `--pollution <deny\|warn>` | `deny` | G1 policy |
| `--exclude <glob,...>` | cache dirs | extra exclusion globs |
| `--backend <auto\|worktree\|docker>` | `auto` | sandbox backend; `auto` → worktree, `docker` runs the verify inside a container (requires a docker daemon) |
| `--docker-image <name>` | `ubuntu:24.04` | image for `--backend docker`; pick one with your toolchain (e.g. `node:22`) |
| `--docker-mount-auth` | off | read-only mount `~/.codex` and `~/.claude` into the container |
| `--log <path>` / `--no-log` | `.sandbox-run/runs.jsonl` | event ledger |

Exit codes: `0` pass · `1` fail/timeout/polluted/rejected · `2` error
(usage / not a repo / sandbox setup / isolation violation).

### Docker backend (`--backend docker`)

Runs the verify command inside a container; the host orchestrates and
retrieves the result (no in-container gateway daemon):

- **Content-addressed runtime**: the `sandbox-run` binary is staged to
  `~/.cache/sandbox-run/runtime/<sha256>/` (existing bytes verified against the
  live binary, rewritten atomically if tampered) and mounted into the
  container **read-only** at `/usr/local/bin/sandbox-run`.
- **Named labeled container** (`sandbox-run`): labels
  `com.losecher.sandbox-run=1` (ownership),
  `com.losecher.sandbox-run.schema-version` and
  `com.losecher.sandbox-run.host-sha256` (fingerprint). A fingerprint mismatch
  (binary / image / schema / mount-auth change) recreates the container via
  `docker rm --force`; an **unowned** container with the same name is refused
  (exit 2).
- **G0**: the host's working-state copy is mounted read-only at `/host` —
  container writes to host paths are rejected (`Read-only file system`). The
  writable container workspace is `/workspace`.
- **Execution**: the working state is copied into `/workspace`, the verify
  command runs via `docker exec` (exit code propagates), on timeout the whole
  container is killed (`docker kill` = whole-tree kill), and the workspace
  changes are retrieved with `docker cp` for G1 pollution detection.
- The container is kept between runs while the fingerprint matches, so docker
  backend runs are **serialized** (a second concurrent run fails loudly).

```sh
# verify against a Node toolchain inside a container
sandbox-run --backend docker --docker-image node:22 -- npm test

# mount codex/claude auth read-only into the container
sandbox-run --backend docker --docker-mount-auth -- cargo test --no-run
```

### Examples

```sh
# verify your uncommitted changes pass tests, in isolation
sandbox-run --scope-from-git -- cargo test --no-run

# jj repo, 10-minute build budget, extra env
sandbox-run --scope-from-jj --timeout 600 --env RUST_BACKTRACE=1 -- cargo build

# explicit scope (caller decides), run a custom check
sandbox-run --changeset scope.json -- sh -c 'npm ci && npm test'

# audit the ledger
sandbox-run status
sandbox-run log <runId>
```

### JSON report (stdout)

```jsonc
{
  "runId": "run-1787233298817-0",
  "verdict": "pass",
  "vcs": "git",
  "baseRef": "HEAD",
  "scope": { "files": ["src/lib.rs"], "source": "git-diff" },
  "sandbox": { "backend": "worktree", "overlaidFiles": 3, "cleaned": true },
  "verify": { "cmd": ["cargo", "test"], "exitCode": 0, "durationMs": 3842, "killed": false },
  "gates": [
    { "gate": "isolation.integrity", "pass": true, "detail": "main tree state unchanged" },
    { "gate": "sandbox.clean", "pass": true, "detail": "0 files modified by verify" }
  ],
  "logPath": ".sandbox-run/runs.jsonl"
}
```

## Notes

- The only file written in the main tree is the event ledger
  (`--log`); add `.sandbox-run/` to your `.gitignore`.
- Verify commands are spawned as **argv, not through a shell** — for pipelines
  pass an explicit shell: `sandbox-run -- sh -c 'npm ci && npm test'`.
- Cache-dir exclusion means the sandbox rebuilds them: the first run of a
  large project can take a while (that is the clean-verify cost, and it is the
  point). `--exclude` cannot re-include them in P0.
- jj is a first-class backend (workspace add / forget / abandon, orphan change
  cleanup; op-log growth is a documented accepted cost).
- The docker backend materializes the full working state (all non-ignored
  files) into the container, since there is no VCS checkout inside the
  container to reconstruct the base from — gitignored files are never copied.

## Development

```sh
cargo test              # unit tests
bash test/gates.sh      # 7 mechanical acceptance gates (git + jj)
bash test/docker-gates.sh  # docker backend gates (skipped when docker is absent)
```

## License

MIT — see [LICENSE](LICENSE).
