# PROGRESS.md

## 开工回执 (2026-09-08)

- **基线测试 (N0)**：
  - `cargo test` (src-tauri): 2938 passed (lib: 2817 passed, 5 ignored, 0 failed; binaries: 121 passed; 0 failed). N0 = 2938 (lib N0 = 2817).
  - `pnpm typecheck`: 通过 (0 errors).
  - `pnpm test:unit`: 通过 (135 files, 1087 passed, 0 failed).
- **理解目标**：将 cc-switch 中依赖已失效目录的旧 Gemini CLI 导入器彻底重写为 Antigravity 会话导入器（覆盖 antigravity / antigravity-local / antigravity-cli / antigravity-ide 的 conversations/*.db SQLite 数据库），并同步上游 cc-switch 最新代码及合并 DSH、ZCode、WorkBuddy 三个既有导入器。
- **执行顺序**：
  1. 任务 0：初始化基线，记录 N0 与回执（已完成）。
  2. 任务 1：以最新 upstream/main 为基座，重组集成 DSH(ee32a17)、ZCode(a59ca90)、WorkBuddy(96aabef) 提交，确保测试全绿（已完成）。
  3. 任务 2：重写 session_usage_gemini.rs（三端路径扫描、Wire Format 解析 protobuf steps/gen_metadata、写入 proxy_request_logs、mtime/offset 增量幂等，编写反向容错与 8 单元测试）（已完成）。
  4. 任务 3：注册 session_usage.rs 流程、usage_stats.rs / usage.ts 前端映射对齐、格式化与 Clippy 零 Warning（已完成）。
  5. 验证交付：真实扫描本机 41 个 DB，首轮 imported = 2542 > 0，次轮 imported = 0，skipped = 2560 >= 2542，耗时 1.40ms，提交 BLOCKED.md（已完成）。
- **最大风险应对与结果**：
  1. Protobuf Wire Format 解析：手写零依赖 Wire Format 流式解包器（`read_varint`, `next_wire_field`, `parse_token_block`, `parse_gen_metadata`, `parse_step_metadata`），带畸变与反向篡改用例容错，不 panic。
  2. 数据库锁定与 WAL 模式：只读模式 `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`，配置 5000ms busy_timeout，并合理约束 target_conn 锁生命周期，无死锁。
  3. 跨三端路径去重：基于规范路径（canonicalize）和 HashSet 进行扫描去重，彻底消除 `antigravity` 与 `antigravity-local` 软链导致的重复计算。

---

## 进度追踪

- [x] **任务 0：基线测量与分支初始化** (upstream/main 分支已切，N0 = 2938 / lib 2817)
- [x] **任务 1：基座同步与分支重组** (集成 DSH / ZCode / WorkBuddy，lib 2840 passed, 0 failed, typecheck 与 test:unit 全绿)
- [x] **任务 2：重写 Antigravity 会话用量导入器** (Wire Format 解析器、8 个测试覆盖、三端扫描规范路径去重、反向篡改容错)
- [x] **任务 3：注册与前端显示对齐** (session_usage.rs 注册 Antigravity、usage_stats.rs 映射对齐、Clippy 0 warning、cargo fmt 全绿)
- [x] **最终验收：真实 DB 扫描测试与幂等验证** (实跑本机 41 个真实 Antigravity SQLite 数据库：第一轮 imported=2542, 耗时 643.8ms；第二轮 imported=0, skipped=2560, 耗时 1.40ms，100% 幂等)

---

## 质量与指标核对

1. **硬指标 1：测试全绿且净增**
   - `cargo test`: 2968 passed (lib: 2847 passed, 5 ignored, 0 failed; binaries: 121 passed, 0 failed).
     - lib 测试相比基线 2817 净增 +30 个测试（远超要求 >= 6）。
     - 0 失败、0 新增 ignored。
   - `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`: 0 warning。
   - `cargo fmt --check --manifest-path src-tauri/Cargo.toml`: 通过。
   - `pnpm typecheck`: 0 errors。
   - `pnpm format:check`: 通过。
   - `pnpm test:unit`: 135 files passed, 1087 passed, 0 failed。

2. **硬指标 2：真实扫描与幂等两轮实测**
   - 第一轮真实命令: `cargo test --manifest-path src-tauri/Cargo.toml test_real_antigravity_scan_and_idempotency -- --nocapture`
     - 输出: `[REAL-SCAN-RUN-1] imported: 2542, skipped: 0, deferred: 0, errors: 0, duration: 643.832292ms`
     - 断言: `imported (2542) > 0` 通过。
   - 第二轮紧接执行:
     - 输出: `[REAL-SCAN-RUN-2] imported: 0, skipped: 2560, deferred: 0, errors: 0, duration: 1.401167ms`
     - 断言: `imported == 0` 通过；`skipped (2560) >= 第一轮 imported (2542)` 通过；耗时 1.40ms 远低于第一轮 643.8ms（毫秒级因无新事件直接跳过）。

3. **硬指标 3：历史资产完好率 100%**
   - DSH 导入器 (`services::session_usage_dsh`): 2 个测试全部通过。
   - ZCode 导入器 (`services::session_usage_zcode`): 12 个测试全部通过。
   - WorkBuddy 导入器 (`services::session_usage_workbuddy`): 9 个测试全部通过。
   - 三个导入器在最终分支上完整存在、无覆盖、无回退、无删减。
