# mount-fs 开发计划（v0.2 — 已通过 @oracle 门禁 PASS-WITH-CHANGES 并落实 R1–R8）

> 状态：APPROVED，已落档 `dev-docs/development-plan.md`。本版已通过 @oracle 门禁（PASS）并落实 R1–R8 及择要采纳 O1–O8。

配套文档：`technical-design.md`（技术方案）。本文件描述分阶段实施、里程碑、交付物、验收标准与工程实践。

## 0. 交付原则
- **垂直切片优先**：先打通一条端到端可用链路（S3 + s3fs-fuse，含安装/挂载/卸载），再横向扩展。
- 每阶段可独立验收、可回滚；核心逻辑（MountPlan/配置/发行版识别/对账状态机）先做单测覆盖。
- 密钥安全（不进 argv、0600 文件、内嵌 sha256）与最小提权从第一天起作为硬约束。
- **接口冻结时点**：`Provider`/`Engine`/`MountPlan` trait 在 **M2 结束时**冻结（由垂直切片验证后冻结），而非 M2 之前。

## 1. 阶段划分与里程碑

### 阶段 0：工程脚手架（M0）
交付物：`cargo` 项目骨架（bin）、模块目录、错误类型、日志（tracing）、clap 命令框架（`version`/`doctor` 占位）；CI（fmt、clippy `-D warnings`、test）、`cargo-deny`/`cargo-audit`、README/贡献指南。
验收：`cargo test`/`clippy` 绿；`mount-fs version` 可运行。
预计：1–2 天。

### 阶段 1：系统适配层（M1）
交付物：`system` 模块——发行版识别（fixture 可测）、包管理器探测、权限/提权封装、`/dev/fuse`+`fusermount3`+`/etc/fuse.conf` 探测、进程执行器、进程监督与 PID 注册（含 starttime 防复用）、并发锁（state 文件锁 + 每 profile 挂载锁）。
说明：**下载 + sha256 + 解压机制推迟到 M3**（仅阿里云盘 Release 产物需要），缩短到首个可用演示的路径。
验收：单元测试覆盖发行版识别、权限判定、锁行为。
预计：3–4 天。

### 阶段 2：S3 端到端垂直切片（M2，核心）
交付物：
- `S3Provider` + `s3fs-fuse` `Engine`（`detect`/`install_plan`/`plan_mount`）。
- 安装：Fedora(dnf)/Debian(apt)/Arch(pacman) 包安装；源码编译仅显式 `--method source`。
- **凭据输入**（本阶段必需，避免 M4 才可测）：env（`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`）+ 交互式 stdin；临时 `passwd_file` 0600。
- 挂载/卸载（`--force` = `fusermount3 -uz`）/`list`/`status`（基于 mountinfo 对账）/`doctor`（S3 项）。
- 挂载生命周期语义：幂等挂载、临时凭据清理、部分失败回滚、启动 GC。
- 集成测试：假引擎脚本全流程（断言 argv 不含密钥、权限 0700/0600）+ MinIO 真实挂载冒烟（best-effort）。
验收：Fedora 上 `mount-fs install s3fs-fuse && mount-fs mount bucket --endpoint ...` 成功挂载并读写；卸载干净；`list/status` 正确；密钥不出现在 argv；对账能识别 stale/hung。
预计：6–8 天。

### 阶段 3：阿里云盘 Provider 与引擎 fallback（M3）
交付物：
- `AliyunProvider`，首选 `aliyundrive-fuse` 引擎（Release tarball 安装 + **内嵌固定 sha256 表**、rpm/deb 兜底；移除 `cargo install`）。
- fallback 引擎：`qrypt`（优先，直接 FUSE3、可写）；`aliyundrive-webdav`+rclone（可选 feature，低维护）。
- **引擎感知令牌采集**：`token_kind`（legacy_web / openapi）；v1 手动粘贴令牌（stdin → 0600 文件）。
- 下载/解压/校验子系统（从 M1 移入本阶段）。
- 引擎能力标注（只读/写）与自动 fallback 策略。
- **M3 凭据**：以 0600 文件或 env 运行；不依赖 M4 的 keyring/持久 UX。
验收：至少一种引擎真实挂载（qrypt 可写，或 aliyundrive-fuse 只读）；`providers` 正确报告各引擎状态。
预计：5–7 天。

### 阶段 4：配置与凭据 UX（M4）
交付物：`config` 子命令（`get` 永不输出密钥、`set`/`login` 仅经 stdin/交互）、profile schema、0600 文件持久存储（`keyring` 作为可选 feature）、脱敏 `list`。
验收：profile 增删改查；凭据文件权限 0600；`config set` 不在 argv 出现密钥的测试用例；脱敏日志用例。
预计：4–5 天。

### 阶段 5：持久化与打磨（M5）
交付物：systemd **用户 `.service`** 生成（`ExecStart` 前台引擎 + `ExecStopPost=fusermount3 -u`）、fstab 片段（root 路径，持久 0600 凭据）、`logs --follow`、`--json` 输出、`doctor --fix`、可选引擎（rclone/mountpoint）、打包（cargo-dist：rpm/deb/musl）。
验收：重启后按需/开机挂载生效；安装包可装；fstab/systemd 凭证路径持久可用。
预计：5–7 天。

### 阶段 6：加固与发布（M6）
交付物：安全审查、异常/边界用例补全、跨发行版测试矩阵（**限定 Fedora + Debian + 一个 EPEL/Arch，2–3 个发行版**）、文档（使用手册+故障排查）、首个 release。
验收（可测清单）：CI 全绿；cargo-deny/audit 无高危；**断言假引擎录制 argv 无密钥**的测试通过；权限断言（目录 0700/文件 0600）通过；威胁模型复核通过。
预计：3–5 天。

## 2. 依赖关系（关键路径）
```
M0 → M1 → M2(S3 切片, 含凭据输入) → M3(Aliyun, 含下载/校验) → M4(config/creds UX) → M5(systemd/打包) → M6(发布)
                                    ↘ M4 的 profile schema 设计可与 M2/M3 并行（存储/UX 在 M4）
```
- M2 是首个“有价值交付”，必须优先完成。
- 接口冻结于 **M2 结束**。
- M4 的持久凭据依赖 M1（权限/文件）；M3 仅需 env/0600 文件即可运行。

## 3. 团队/并行工作拆分（如多人）
- Lane A：system/install（M1）。
- Lane B：S3 provider+engine（M2）。
- Lane C：Aliyun provider+engines（M3，依赖 A/B 接口在 M2 冻结后）。
- Lane D：config/creds（M4，schema 可提前并行）。
- Lane E：CI/打包/文档（贯穿）。
接口冻结点：`MountPlan`、`Engine`、`Provider` trait 于 **M2 结束时**冻结（见技术方案 §2.2）。

## 4. 验收与门禁
- 代码门禁：CI 绿（fmt/clippy/test）、cargo-deny/audit 无高危、核心纯函数单测覆盖。
- 功能门禁：M2/M3 真实挂载冒烟通过；`doctor` 无误报阻断。
- 安全门禁：**断言密钥不进 argv、不入 world-readable 文件**；下载强制内嵌 sha256；权限断言（目录 0700/文件 0600）。
- 发布门禁：跨发行版矩阵（2–3 发行版）+ 打包产物安装验证。

## 5. 测试策略（与方案 §4 对齐）
单元 / 假引擎集成（含 argv 无密钥断言、权限断言）/ MinIO 容器 e2e（best-effort，API 级为确定性门禁）/ 手工矩阵。每阶段新增对应测试用例。

## 6. 估算与非目标
- 总工期（单人全职）估算：**约 4–5 周**（前提：v1 去除 QR 登录；M6 矩阵限定 2–3 发行版）。
- 非目标：FUSE 实现、同步备份、非 Linux 平台、自定义引擎模板、v1 二维码/OAuth 登录。

## 7. 已定设计决策（原 §6 待确认项，见技术方案 §6）
1. fallback 顺序 qrypt → aliyundrive-webdav；2. keyring 非 v1 默认；3. systemd 用户 service（阶段 5）；4. 不自定义引擎模板；5. v1 手动粘贴令牌。
