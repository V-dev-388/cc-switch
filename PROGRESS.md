# PROGRESS.md

## 开工回执 (2026-09-09 Qoder 与 QoderCN 全链路会话用量接入)

- **基线实测**：cargo test --lib 跑通 2850 (5 ignored, 0 failed)；pnpm typecheck 零报错；pnpm test:unit 跑通 1087。
- **理解目标**：实现海外版 Qoder 与国内版 QoderCN 双导入器，全链路打通会话 Token/金额解析、去重幂等、后端统计与前端筛选展示。
- **执行顺序**：任务 0 基线核对 -> 任务 1 Qoder 导入器 -> 任务 2 QoderCN 导入器 -> 任务 3 全链路注册/前端/i18n 及双反向验证 -> 最终验收。
- **最大风险**：Qoder/QoderCN 异构 JSONL 结构容错（缺失 request_id、超大文件与损坏行）及全局去重幂等防重复计费。

---

## 进度追踪

- [x] **任务 0：基线测量与开工回执** (cargo test --lib 2850 pass / 0 fail / 5 ignored; pnpm typecheck 0 报错; pnpm test:unit 1087 pass)
- [x] **任务 1：实现 Qoder 导入器（海外版）** (session_usage_qoder.rs 支持 QODER_PROJECTS_DIR 覆盖、扫描、credits 费用/token 估算、qoder:<req_id> 去重、10 个单元测试全绿)
- [x] **任务 2：实现 QoderCN 导入器（国内版）** (session_usage_qodercn.rs 支持 QODERCN_PROJECTS_DIR 覆盖、扫描会话及 transcript/*.jsonl、轮次解析、模型定价映射、qodercn:<sess_id>:<uuid> 去重、10 个单元测试全绿)
- [x] **任务 3：注册全链路与前端适配** (services/mod.rs、session_usage.rs、usage_stats.rs、sql_helpers.rs、types/usage.ts、UI/Hero/4 语种 i18n 完整适配，反向验证①②均完成「红 -> 还原 -> 绿」实测)
- [x] **生产环境完整构建打包**
  - macOS 应用程序包：`src-tauri/target/release/bundle/macos/CC Switch.app`
  - macOS DMG 安装镜像：`src-tauri/target/release/bundle/dmg/CC Switch_3.20.2_aarch64.dmg` (15 MB)
  - 实测真实数据导入：
    - `[QODER-SYNC]`: 导入 3,566 条，跳过 25 条，扫描 70 个项目文件
    - `[QODERCN-SYNC]`: 导入 6,330 条，跳过 0 条，扫描 85 个会话文件
- [x] **最终验收与硬指标交付** (cargo test qoder 20 passed; cargo test --lib 2870 pass / 0 fail / 5 ignored; cargo clippy 零警告; pnpm typecheck 零报错; pnpm test:unit 1087 pass; git diff 严格符合白名单; BLOCKED.md 无阻塞)
