# PROGRESS.md

## 任务 0 开工回执 (2026-09-16 根除缓存创建显示 N/A 缺陷)
- **基线核验**：vitest run tests/types/usage.test.ts 1 pass；cargo test --lib services::session_usage_codebuddy 15 pass；数字与源码硬编码完全吻合。
- **理解目标**：根除全平台全部视图与有数据场景下缓存创建永久显示 N/A 的缺陷；打通 CodeBuddy/DSH 采集器的缓存写入解析流。
- **执行顺序**：任务 0 基线核对 -> 任务 1 修复前端 UsageHero 与 usage.ts 及反向验证 -> 任务 2 修复 CodeBuddy 与 DSH 采集器及反向验证 -> 最终全量验收。
- **最大风险**：OpenAI 协议与带缓存写入真实场景的判定重叠；采集器日志结构中 `cachedWriteTokens` 等字段的提取兼容性与缺失处理。

## 进度追踪
- [x] 任务 0：基线测量与开工回执 (vitest 1 pass, cargo test codebuddy 15 pass)
- [x] 任务 1：修复前端 UsageHero 与 usage.ts 的缓存创建展示逻辑
  - `src/types/usage.ts`：`getCacheWriteAvailability` 增加 `isAll` 支持，全部视图下绝不返回 `"na"`。
  - `src/components/usage/UsageHero.tsx`：全部视图与 `cacheWrite > 0` 场景下无条件展示格式化数值，彻底消除误导性 N/A。
  - 单测反向验证已完成（红：断言全部视图返回 na 报错 FAIL；绿：2 passed 全绿）。`tsc --noEmit` 0 错误；前端 1088 单测全绿。
- [x] 任务 2：修复 CodeBuddy 与 DSH 采集器的缓存写入数据解析
  - `session_usage_codebuddy.rs`：在 `notifyStepEnd` 中提取 `cachedWriteTokens`，在 trace `toolOutput` 的 `prompt_tokens_details` 中提取 `cache_write_tokens`，填入 `record.cache_creation_tokens`，修复已有断言 `cache_creation_tokens must be 0`。
  - `session_usage_dsh.rs`：支持解析 `cacheCreationTokens` / `cacheWriteTokens` 并存入 `proxy_request_logs` 的 `cache_creation_tokens`，补充单测。
  - 单测反向验证已完成（红：构造 `cachedWriteTokens: 500` 未提取时 `assert_eq!(row.9, 500)` 报 `left: 0, right: 500` FAIL；绿：提取后 15 passed 全绿）。
  - `cargo test --lib services::session_usage_dsh` 6 个单测全绿。
- [x] 任务 3：修复空环境变量误报冲突与白屏问题
  - 根除白屏：使用官方标准 `tauri build -b app` 打包完整生产 Bundle 并安装签名，杜绝单二进制编译导致的 `dev` 模式连接 localhost:3000。
  - 根除冲突误报：`env_checker.rs` 增加对空值与空白变量的过滤（Unix 进程环境、Shell 配置文件及 Windows 注册表）；`env_manager.rs` 支持 Unix 下删除进程环境变量；前端 `api/env.ts`、`App.tsx`、`EnvWarningBanner.tsx` 全链路增加非空防御并补充单测 `EnvWarningBanner.test.tsx`。

