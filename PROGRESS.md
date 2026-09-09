# PROGRESS.md

## 开工回执 (2026-09-08 Antigravity 全链路打通)

- **基线测试 (N0)**：
  - `cargo test` (src-tauri): 2968 passed (lib: 2847 passed, 5 ignored, 0 failed; binaries: 121 passed, 0 failed).
  - `pnpm typecheck`: 通过 (0 errors).
  - `pnpm test:unit`: 通过 (135 files, 1087 passed, 0 failed).
- **理解目标**：将 cc-switch 中原 Gemini CLI 的会话管理、MCP 联动、配置与环境管理、界面展示及国际化文案全面适配至 Antigravity（覆盖 desktop/cli/ide 三端），实现从会话列表、MCP 穿透到 UI/i18n 的全链路打通。
- **执行顺序**：
  1. 任务 0：初始化基线，记录 N0 与回执（已完成）。
  2. 任务 1：重写 `session_manager/providers/gemini.rs`（三端 conversations/*.db 扫描、只读 SQLite 读取 steps 提取首条用户提问作为标题/摘要与时间戳、load_messages 解析消息、delete_session 彻底删除 db/-wal/-shm 及 brain/<id> 目录，编写 ≥4 个单元测试并通过）。
  3. 任务 2：扩展 `mcp/gemini.rs` 与 `gemini_mcp.rs`（原子双写 settings.json 与 antigravity-ide/mcp_config.json、导入优先合并 antigravity-ide、解构修复 double nesting、确认 gemini_config 目录与代理环境写入，通过 mcp::gemini & gemini_mcp 测试）。
  4. 任务 3：升级前端 UI 与国际化（appConfig.tsx 中 gemini 项 label/name/description 展示 Antigravity、4 套语言包统一为 Antigravity、检查 DirectorySettings 与 AppVisibilitySettings 显示适配，保证 clippy/fmt/typecheck/unit-test 全绿）。
  5. 最终验证：执行会话管理扫描接口实测返回真实 Antigravity 会话数 >0，更新 PROGRESS.md 与 BLOCKED.md（无阻塞写「无」）。
- **最大风险应对**：
  1. 会话 SQLite 多步解析与编码容错：从 steps 表的 step_type (14 用户提问/15 助手回答/17 异常) 正确提取 Wire Format 数据，跳过空内容与工具调用噪音，对无用户提问或损坏 DB 优雅回退不 panic。
  2. MCP 双向同步与双重嵌套消除：读取 settings.json 与 mcp_config.json 时自动扁平化解包已有的 mcpServers 双重嵌套，写入时原子更新对应结构，保证 IDE 与桌面端无冲突。
  3. 删除会话的完整性与安全性：严格限制在 Antigravity 规范会话路径内，同时安全清理 .db、-wal、-shm 以及对应客户端目录下的 brain/<id> 目录。

---

## 进度追踪

- [x] **任务 0：基线测量与开工回执** (N0 = 2968 pass / 0 fail / 5 ignored; typecheck 绿; unit-test 1087 pass)
- [x] **任务 1：会话管理器接入 Antigravity** (重写 session_manager/providers/gemini.rs，三端扫描、只读 SQLite、首提问提取、load_messages、delete_session 及 6 个单元测试全部通过)
- [x] **任务 2：MCP 与配置同步全面打通** (原子同步 settings.json 与 antigravity-ide/mcp_config.json、解嵌套、优先合并、mcp::gemini 与 gemini_mcp 4 个新测试全部通过)
- [x] **任务 3：UI、文案与国际化全面升级为 Antigravity** (appConfig、4 套语言包、DirectorySettings、AppVisibilitySettings、clippy 0 warning、fmt pass、typecheck 绿、unit-test 1087 pass)
- [x] **最终验收与实测** (调用会话扫描返回真实 Antigravity 会话数 44 > 0，cargo test 达到 2979 pass，净增 11 个测试全部通过，0 fail / 5 ignored，BLOCKED.md 为「无」)



