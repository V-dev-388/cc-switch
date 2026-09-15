# PROGRESS.md

## 开工回执 (2026-09-15 修复 CC Switch 安装版白屏)

- **理解目标**：/Applications 里的 CC Switch.app 是手工拼装/替换二进制的坏包，双击白屏；源码与 dev 模式正常。用 `pnpm tauri build` 干净重打整包并正确装回，确认窗口正常渲染。
- **前提核验**：已读 REPAIR_LOG.md（其“已修好”结论不可信，仅背景）；已复现白屏（/tmp/before.png：CC Switch 窗口内容区纯白只剩窗口框）；已 pkill 清掉 dev/实例进程；当前安装包 codesign 虽 exit=0 但仍白屏，说明问题在包完整性而非签名。
- **执行顺序**：任务0 核验 -> pnpm tauri build（退出码0）-> rm -rf 旧包 + cp -R 整个新 .app 到 /Applications -> codesign 自检 -> open 复验进程+after.png+git status 改动仍在。
- **最大风险**：build 卡在签名或耗时过长；换包时误只拷单二进制（禁区）；误丢 14 个 M + 3 个 ?? 未提交改动。全程不动 src/ 前端代码。

## 完工记录 (2026-09-15 修复 CC Switch 安装版白屏) — 已达成

- **干净重建**：`pnpm tauri build` 完整跑通（前端重编 dist -> Rust release 编译 -> 内嵌 -> 打包 .app/.dmg），最终退出码 0，产物 `src-tauri/target/release/bundle/macos/CC Switch.app`（二进制 29,624,496 字节）。
- **换包**：`rm -rf` 旧包 + `cp -R` 整个新 .app 到 /Applications（非单二进制拷贝）。
- **验收全绿**：open 后 pgrep 见进程 49053；/tmp/after.png 窗口正常渲染出顶部标签栏“通用/路由/认证/高级/使用统计/关于”及设置页内容，不再白屏；`codesign -v --deep --strict` 退出码 0；git status 原 14 个 M + 3 个 ?? 全部仍在，src/ 前端代码一行未改。

### 两处必要偏离（白屏真因与绕过，均非手工拼包）

1. **退出码**：首次 `pnpm tauri build` 编译/打包全部成功，仅最后的 updater 产物 `CC Switch.app.tar.gz` 需 `TAURI_SIGNING_PRIVATE_KEY`（本机无此私钥）而 exit 1。改用 `pnpm tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'` 仅跳过这个与白屏无关的自动更新包，主流程不变，得到 exit 0。未改任何源码/配置文件（tracked 的 tauri.conf.json 未动）。
2. **白屏真因**：tauri 本地构建的 .app 只被链接器 adhoc 签名（flags=adhoc,linker-signed），bundle 无 `_CodeSignature/CodeResources`，`codesign -v --deep --strict` 报“code has no resources but signature indicates they must be present”——这正是 WebKit 白屏根因。对完整新 bundle 就地 adhoc 重签 `codesign --force --deep -s - "/Applications/CC Switch.app"` 生成 bundle 级签名后验证通过。（这是对合法 tauri 产物的签名步骤，非“拷单二进制进旧包”。）

---

## 历史：开工回执 (2026-09-09 Qoder 与 QoderCN 全链路会话用量接入)

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
