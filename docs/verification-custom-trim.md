# custom/trim-agents 裁剪验证记录

> 分支 `custom/trim-agents`（PR #2：trim: keep only claude-code / opencode / zcode agents）
> 的完整验证过程与结果记录。由前后两次会话的验证工作汇总而成。
> 最后更新：2026-09-13

## 1. 验证环境与方法

- 仓库：`chendaohao/ai-memory`（fork，public），上游 `akitaonrails/ai-memory`。
- 本地：Windows 10 + Git Bash，**未安装 Rust 工具链（rustup 安装失败）**，
  因此所有编译/测试验证完全依赖 GitHub Actions CI。
- 验证循环：本地静态自查 → commit → push → 等待 200~270s → GitHub API
  查询 workflow runs → 下载失败 job 日志 → 定位 → 修复 → 再推。
- GH token 存放于 `~/.ai-memory-gh-token`；代理 `http://172.17.192.1:31181`
  不稳定，直连 api.github.com 才可靠；Git Bash 下 `curl -o /tmp/...` 不可靠，
  输出统一写到 `$HOME` 下。

## 2. push 前的本地静态自查（全部通过）

| 检查项 | 方法 | 结果 |
| --- | --- | --- |
| 工作区干净、改动与计划一致 | `git status` / 逐文件核对 | ✅ |
| 保留的 hook 脚本 shell 语法 | `sh -n hooks/_lib.sh hooks/claude-code/*.sh hooks/opencode/*.sh` | ✅ |
| hook 脚本引用的 7 个 `ai_memory_*` 函数都有定义 | grep + 人工核对 `_lib.sh` | ✅ |
| 文档中被删 agent 名残留（codex/cursor/gemini 等） | `git grep` 两轮 | ✅（README/support-matrix 裁掉 20 行残留） |
| 被删 enum variant 的代码残留 | 全库 `git grep` | ✅（修复 2 处会编译失败的漏网：`resume.rs` 4 处 `Codex→OpenCode`；`transcript.rs` `Pi→Claude`，连带重建 JSONL fixture 偏移 74→82） |

## 3. CI 验证历程

### 3.1 私库额度阶段

基线代码在私库时代完整通过 CI（绿）。裁剪后多轮修复期间，私库
2000 分钟/月额度耗尽，所有 job 秒挂（0 步骤、无日志）——确认为环境问题
而非代码问题。secrets 扫描确认无真实凭据后，将仓库 PATCH 为 public，
CI 恢复真实运行（run 34726354168 @ `ec90d5cc`）。

### 3.2 批量裁剪损伤的逐个修复（全部修复完毕）

- `run.rs:1658` unclosed delimiter、`routing_instructions.rs` 悬空 `#[test]`、
  `uninstall.rs` `strip_mcp_json` 缺 `}` → 补括号/截断。
- 误删的 `SqlCursor`、`HookCommandPlatform/Context` 类型 → 从上游历史恢复。
- 多处 `unused import` / `dead_code`（CI 有 `-D warnings`）、全部 rustfmt
  diff、3 处多余空行、`install_skills.rs` `platform→_platform`。

### 3.3 修复后各 job 状态

通过：companions、gitleaks、cargo-audit、cargo-deny、native packaging
assets、changelog sections、clippy、rustfmt、release-build、source-install、
docker-smoke。

**单元测试：680 passed; 0 failed; 1 ignored。**
**集成测试：98 passed; 1 failed; 1 ignored。**

唯一失败项：`removal::only_hooks_preserves_mcp_in_same_file`。

## 4. 最后一个失败项的根因与修复（2026-09-13）

### 4.1 现象

CI run 34732612828（commit `7c7126ab`）中 `test (ubuntu-latest)` 失败：

```
thread 'removal::only_hooks_preserves_mcp_in_same_file' panicked at
crates/ai-memory-cli/tests/suite/removal.rs:276:5:
uninstall failed: stdout= stderr=...
Error: existing file isn't valid JSON; refusing to overwrite. ...
Caused by: EOF while parsing an object at line 1 column 312
```

（该测试此前还因"debug 输出导致 uninstall 被跑两次"的写法问题反复折腾，
最终改为单次 `output()` 捕获，即 commit `7c7126ab`。）

### 4.2 根因

测试夹具 `removal.rs:268` 手写的 ZCode 配置 JSON 缺一个右花括号：
`hooks` 对象未关闭就写了 `,"mcp":`，整个文档到结尾少一层 `}`，
共 312 字符——与 CI 报错 "column 312" 精确吻合。uninstall 的
`mutate_json` 读到非法 JSON 按设计拒绝覆盖（`apply_shared.rs:185`），
测试断言失败。

夹具本意（三条断言）：uninstall `--apply --only hooks --yes` 后
① 我们的 ZCode `SessionStart` hook 被删；② 第三方 `PostToolUse` hook 保留；
③ `mcp.servers.ai-memory` 必须保留（`--only hooks` 不碰 MCP）。

### 4.3 修复

在 `,"mcp"` 前补一个 `}`（312 → 313 字符）。本地用 Python 对
`removal.rs` 全部 7 个 raw-string JSON 夹具做了合法性校验，全部通过。

行为链路复核（uninstall.rs）：`strip_zcode_hooks` → 只动
`hooks.events` → `hook_entry_is_ours` 按签名
（command 含 ai-memory + args 含 `hook/--event/--agent/--server-url`）
判定夹具条目为我们的 → 删除后 SessionStart 键清空移除，PostToolUse 与
`mcp.servers` 原样保留。与三条断言逐条对应。

### 4.4 修复

在 `,"mcp"` 前补一个 `}`（312 → 313 字符）。本地用 Python 对
`removal.rs` 全部 7 个 raw-string JSON 夹具做了合法性校验，全部通过。

行为链路复核（uninstall.rs）：`strip_zcode_hooks` → 只动
`hooks.events` → `hook_entry_is_ours` 按签名
（command 含 ai-memory + args 含 `hook/--event/--agent/--server-url`）
判定夹具条目为我们的 → 删除后 SessionStart 键清空移除，PostToolUse 与
`mcp.servers` 原样保留。与三条断言逐条对应。

修复 commit：`0f33794d fix: close the hooks brace in the zcode same-file fixture`。

## 5. 第二个失败项：被 fail-fast 掩盖的 workstream 断言错误（2026-09-13）

### 5.1 现象

推送 `0f33794d` 后（run 34733300164），removal 套件转为 99 passed / 0
failed，但此前从未运行过的 `ai-memory-workstream` lib 套件首次执行即失败：

```
test harness::tests::adoption_is_only_allowed_for_session_launches_without_a_selector ... FAILED
panicked at crates/ai-memory-workstream/src/harness.rs:373:9:
assertion failed: !allows_native_session_adoption(ManagedHarness::OpenCode,
        &[OsString::from("doctor")])
```

### 5.2 根因

`cargo test` 默认 fail-fast：run 1（`7c7126ab`）里 cli 集成套件失败后，
排在最后的 workstream 套件从未执行，这个确定性 bug 一直被掩盖。

具体矛盾：裁剪提交 `8b9431cb` 删除 Codex/Pi/CommandCode 等 harness 时，
把测试里 `Codex "login"`（utility 子命令直通）断言改写为
`OpenCode "doctor"`，但 OpenCode 的 utility 列表从上游起就不含 `doctor`
（上游的 `doctor` 直通断言属于 Kimi/Grok，均已被裁掉）。实现与断言互相
矛盾，`opencode doctor` 被判为 Session 启动 → 允许 adoption → 断言必败。

### 5.3 决策与修复

改测试而非改实现：裁剪分支不应静默变更上游验证过的启动语义（若真实
opencode 确有 `doctor` 子命令，应作为上游缺口反馈，而非在本 fork 里改
`launch_mode` 列表）。将断言改为 OpenCode utility 列表中真实存在的
`models`，仍完整覆盖"utility 子命令不允许 adoption"这一行为。

修复 commit：`dc3bca40 test: use a real opencode utility subcommand in the adoption gate test`。

执行顺序核实：store/wiki/consolidate 的 `tests/suite/` 均为 lib 测试的
模块（非独立二进制），run 2 中全部 15 个测试二进制都已执行——workstream
lib 是最后一个，修好它即可全绿，无需再担心 fail-fast 掩盖。

## 6. 最终 CI 结果（2026-09-13）

- Run：[34734026137](https://github.com/chendaohao/ai-memory/actions/runs/34734026137)
  @ `dc3bca40`，**conclusion = success**。
- 12 个 job 全绿：rustfmt、clippy、test (ubuntu-latest)、release-build、
  source-install、companions、native-packaging、changelog、docker-smoke、
  cargo-deny、cargo-audit、gitleaks。
- 测试合计：**15 个套件，2897 passed / 0 failed**（cli lib 680、cli 集成
  99、store 426、mcp 415、hooks 304、llm 225、consolidate 216、core 194、
  wiki 184、workstream 22、evals smoke 18、web 114）。
- windows 工作流按设计 skipped（PR 无 `windows` 标签；夜间/手动/打标才跑，
  发布前必须 dispatch 一次并等绿，见 AGENTS.md）。

结论：`custom/trim-agents` 裁剪分支的 CI 验证全部通过，PR #2 可合并。

## 7. 裁剪残留审计（2026-09-13）

对被删 agent（codex/cursor/gemini/kimi/grok/pi/antigravity/kiro/devin 等）
的残留引用做了全库分类清点：

| 类别 | 位置 | 判定 |
| --- | --- | --- |
| 数据词汇层：`AgentKind` 枚举（ids.rs，含全部已删 agent variant） | ai-memory-core | **保留**。serde kebab-case 反序列化 + `sessions.agent_kind` CHECK 约束 + `from_wire`：删 variant 会破坏历史会话数据读取与旧版 hook 上报。fork 定位是记忆服务器，应能继续摄入任何 agent 抓到的数据 |
| 冻结的 SQL 迁移：V09/V11/V20/V25 等枚举 `agent_kind` | ai-memory-store/migrations | **保留**。迁移一经应用不可改写 |
| store 单测夹具用 `AgentKind::Codex` 当样本数据 | store/src/lib.rs 测试 | **保留**（数据层兼容性测试） |
| 已删 agent 的 hook 运行时特判（Devin 会话 ID、AntigravityCli 事件过滤、KiroCli 分支） | ai-memory-cli/src/commands/hook.rs | **可留可清**。仅当线上真有该 agent 的 hook payload 才会执行，属惰性代码，且有单测覆盖；清理属可选的后续 hygiene，不应混进本次裁剪 PR |
| 文档注释提到 Cursor/Gemini/Codex 用户 | cli.rs:974 等 | 装饰性，暂留 |
| importer 明确不发送 codex 身份的断言与说明 | companions/ai-memory-importer | 本来就是测试，保留 |
| e2e 注释解释为何直连 Gemini REST | tests/e2e/handoff_smoke.sh | 装饰性 |

总体结论：**集成层**（ManagedHarness/RunHarnessChoice、hook 脚本、
setup-agent、文档矩阵）已裁干净；残留集中在**数据词汇层**（必须保留）
与少量惰性特判/注释（无需急于清理）。"opencode doctor" 疑点经官方 CLI
文档核实：opencode **没有** `doctor` 子命令，测试断言用 `models` 是对的；
顺带发现官方列表有 `auth` 而代码 utility 列表缺失（`opencode auth` 会被
误判为会话启动、可能误触发 adoption 提示），已补入 `harness.rs`。
