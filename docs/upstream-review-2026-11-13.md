# 上游提交审查（2026-11-13）

用户已确认合入三项修复和 OMP 图标，并沿用 README 中的 fork 边界：保留 Android 协议、AI/Git 设置、外部编辑器与禁用上游更新器的策略，不恢复 GitHub/Info 面板。

## 范围与结论

- 本地审查版本：`7f0f67aa`。
- 上游：<https://github.com/l0ng-ai/tty7>，默认分支 `main`。
- 上次审查基线：`8ad8841e4835e4e6c4e6f4d5a0a16541eaa9107f`。
- 本轮上游截止：`384ef87554e0f9a98f72282c6306c9221a7a48b5`（2026-11-13）。
- 基线后共 6 个提交：已应用 2 个修复、适配应用 1 个修复，以及 1 个图标更新；跳过 2 个。

四项改动已应用到工作区，README 和 CHANGELOG 的集成记录推进至本轮截止版本，累计选取 33 个上游提交。由于此前使用 cherry-pick，Git 图上的 ahead/behind 不等于缺失修复数。

| 上游提交 | 内容 | 处理及依据 |
| --- | --- | --- |
| [65097ba7](https://github.com/l0ng-ai/tty7/commit/65097ba7) | ARM macOS CI 迁移到 macOS 15 | 跳过。本地 `501c3d68` 已迁移仍保留的 ARM 发布任务；上游涉及的 CI/nightly 工作流已被 fork 删除。 |
| [5d80817d](https://github.com/l0ng-ai/tty7/commit/5d80817d) | profile.d 扩展 PROMPT_COMMAND 时 Bash 首条命令标记 | 已应用实现和回归测试，手工整合 CHANGELOG；修复本地相同的顺序问题。 |
| [e7e5bd10](https://github.com/l0ng-ai/tty7/commit/e7e5bd10) | Oh My Pi 使用 omp.sh 图标及品牌色 | 已应用。四个文件干净应用，更新桌面和移动端图标及配色；不改变协议或行为。 |
| [7ee8f7fa](https://github.com/l0ng-ai/tty7/commit/7ee8f7fa) | macOS SCDynamicStore 创建失败时返回无系统代理 | 已应用。同步更新 core/gateway 的 system-configuration 依赖 0.6→0.8，并改用 `build()?`；手工整合 CHANGELOG。 |
| [1874d89f](https://github.com/l0ng-ai/tty7/commit/1874d89f) | GitHub/Info 面板正文宽度 | 跳过。本地只保留 SCM/Files，相关界面与滚动容器已删除，没有修复目标。 |
| [384ef875](https://github.com/l0ng-ai/tty7/commit/384ef87554e0f9a98f72282c6306c9221a7a48b5) | 从 resume 命令立即记住会话 ID，避免再次重启后退回 Shell | 已适配应用。识别 fork 默认模型/推理参数的命令顺序，并保留本地 daemon 移动端状态发布逻辑。 |

## Codex 修复的适配

上游只处理 `codex resume <id> ...`。本地 `src/ui/agent_launch.rs` 的 `configured_codex_line` 会将默认参数插在子命令前；现有测试已明确验证如下输出：

```text
codex -c model_reasoning_effort=high resume session --model explicit
```

上游 `resumed_session_id` 在 `codex` 后读到 `-c` 即返回 `None`。本次复用 AI 设置模块中的选项分类，按已知参数的取值数量跳过 Codex 选项；支持分离值、等号值和短选项粘连值。遇到未知取值数量保守返回无 ID，避免把选项值或提示内容中的 `resume` 误认成子命令。保留 agent 自报 ID 优先、不把 `--last` 或新会话当作具体会话的行为。

本地 `crates/tty7-core/src/daemon/pane.rs` 的 `apply_agent` 额外维护 `agent_clock.generation` 和 `publish_mobile_agent`。上游补丁直接应用检查失败；本次人工整合保留这些逻辑。仅补全会话 ID 时不增加状态修订、不发布移动端状态事件，新增回归测试验证这一点；新增 Agent 时仍发布正常初始状态。

## 上游来源

- Bash：`5d80817d72640efd69cd42a6efb5951c994b331e`。
- macOS 代理：`7ee8f7fadc5eea5f26306807d71dabd1031f4404`。
- OMP 图标：`e7e5bd103a23cecde2e2ea970ba740e3a9007ff0`。
- 会话恢复：`384ef87554e0f9a98f72282c6306c9221a7a48b5`。

## 验证

- `cargo +1.95.0 test --workspace`：4947 项通过，0 失败，7 项原有忽略。包含新增 Bash 首命令、恢复会话 ID、Codex 参数误识别及移动端状态发布回归；同时编译并测试 macOS core/gateway。
- Bash 回归实际使用从 GNU 官方源码临时编译的 Bash 5.3，测试进程 PATH 前置 `/tmp/tty7-bash-audit-20261113`，未因系统 Bash 3.2 而跳过；未安装系统软件。
- `npm run build`（mobile）：TypeScript 检查与生产构建通过，存在打包体积提示。
- `cargo +1.95.0 clippy --workspace --all-targets`：通过，存在既有 lint 警告；补装了本机 1.95.0 toolchain 缺少的 Clippy 组件。
- `./gradlew :app:testDebugUnitTest`（mobile）：通过，任务为 UP-TO-DATE；通过命令环境变量指定已有 SDK `/Users/monsoon/Library/Android/sdk`，未写入 local.properties。
- `cargo +1.95.0 fmt --all -- --check`：仓库已有格式差异；本次新增代码已按 rustfmt 格式化。仍出现差异的已修改文件仅 pane.rs，已对照 HEAD 确认其原本就有格式差异，未扩大修改范围。
- `git diff --check` 通过；`UPDATES_ENABLED` 仍为 `false`，未修改 vendored 终端、令牌日志过滤或 relay 地址/指纹。
- 查询历史使用临时 ref `refs/audit/upstream-main`；fetch 自动补充了上游 tags，未修改当前分支或配置 upstream remote。

本轮未改变领域概念，也未形成难以逆转的架构决策，因此不新增 GLOSSARY 或 ADR。
