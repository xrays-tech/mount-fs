# mount-fs 技术方案（v0.2 — 已通过 @oracle 门禁 PASS-WITH-CHANGES 并落实 R1–R8）

> 状态：APPROVED，已落档 `dev-docs/technical-design.md`。本版已通过 @oracle 门禁（PASS）并落实 R1–R8（必改）及 O1–O8（择要采纳）。

## 1. 背景与目标

### 1.1 目标
开发一个 Rust 编写的 Linux CLI 工具 `mount-fs`，提供统一命令界面，将多种网盘/对象存储挂载到本地文件系统：

- 阿里云盘（Aliyun Drive）
- 兼容 S3 协议的对象存储（MinIO、Ceph RGW、阿里云 OSS、Backblaze B2/S3Proxy 等）

核心定位：**编排层 / 环境适配层**，不自己实现 FUSE。工具负责：

1. 环境探测（发行版、FUSE、依赖、网络）。
2. 主动安装/补齐后端挂载工具（优先包管理器，其次官方发布产物，最后源码编译）。
3. 凭据与配置管理。
4. 统一的挂载/卸载/状态/诊断命令界面。

### 1.2 非目标（Non-Goals）
- 不实现 FUSE 文件系统本身。
- 不做文件同步/备份（那是 aliyunpan/rclone 的职责）。
- 首版仅 Linux（不覆盖 macOS/Windows）。
- 不自建 WebDAV 服务（仅作为后端工具的可选项）。
- 首版不做自定义引擎命令模板（避免任意命令注入；`Engine` trait 即扩展点）。
- 首版不做二维码/OAuth 登录（改为手动粘贴令牌，见 §2.4.2）。

### 1.3 关键调研结论（截至 2026-11，作为方案事实基础）
- `aliyundrive-fuse`（messense）：**已归档（archived=true）**，最后提交 2023-02，最后 release v0.1.14（2022-07）；**只读挂载**；使用旧版 `api.aliyundrive.com` 网页 token。发布产物仅有 musl tarball / rpm / deb；CLI 为 `aliyundrive-fuse --refresh-token <TOKEN> <PATH>`（或 `--workdir` 下纯文本 `refresh_token` 文件），**无配置文件**。
- `s3fs-fuse`：**活跃维护**，最新 v1.97（2025-12）。Fedora 44/45 官方仓库已提供 `s3fs-fuse`（`dnf install s3fs-fuse`），RHEL 系经 EPEL。v1.96 起**强制要求 FUSE 3**、C++14，并将 `-o endpoint` 改名为 `-o region`。凭据文件格式 `ACCESS_KEY:SECRET`，`~/.passwd-s3fs` 需 0600 权限；非 AWS 端点需 `-o url= -o use_path_request_style -o region= -o sigv4`。**上游 GitHub Release 仅提供源码 tarball，无预编译二进制**。
- 阿里云盘替代/补充引擎（用于 fallback，避免被归档工具卡死）：
  - `qrypt`：**活跃**，直接 FUSE3 挂载，需 OpenAPI `refresh_token` + `drive_id`，支持读写。
  - `aliyundrive-webdav`：GitHub 标记 `archived=true`（最后 release v2.3.3, 2023-10），但 2025 年有 Rust 后端重写提交、CI/依赖更新至 2026-02（**注意：该重写未随 release 发布，当前可安装版本仍是旧版**）；采用 **OpenAPI** 体系，提供 WebDAV 服务（需 rclone 等桥接为 FUSE）。**低维护，且 OpenAPI refresh_token 与旧网页 token 不通用。**
  - `OpenList`：活跃，WebDAV；列为后续可选引擎，不纳入 v1。
  - `aliyunpan`：活跃，但**不挂载**（仅同步），不用于挂载引擎。

> 结论：`aliyundrive-fuse` 作为用户明确指定的**首选引擎**实现，但必须通过“引擎抽象 + fallback 链”规避其归档风险，并在文档/诊断中如实标注只读、旧 token、无配置文件等限制。**注意不同引擎的令牌体系互不通用**（见 §2.4.2 与 §2.9）。

## 2. 总体架构

### 2.1 分层
```
┌──────────────────────────────────────────────────────────┐
│ CLI 层 (clap)  mount/umount/list/status/doctor/install/    │
│                config/providers/logs/version               │
├──────────────────────────────────────────────────────────┤
│ 核心编排层 Core                                            │
│  - MountOrchestrator  - ProfileStore  - StateRegistry      │
│  - Diagnostics        - CredentialManager  - Locking       │
├──────────────────────────────────────────────────────────┤
│ Provider 层 (trait Provider)                               │
│  - AliyunProvider   - S3Provider   - (可扩展占位)          │
│     └ Engine 抽象 (trait Engine)：每个后端二进制一个实现    │
├──────────────────────────────────────────────────────────┤
│ 系统适配层 System Adapters                                 │
│  - DistroDetector  - PackageManager  - Privilege           │
│  - FuseProbe       - ProcessSupervisor - DownloadVerifier  │
│  - InstallManifest - ChecksumRegistry  - SystemdIntegrator │
└──────────────────────────────────────────────────────────┘
```

### 2.2 关键抽象（接口草案，Rust 伪代码）

```rust
// Provider：一类网盘能力的对外契约（接口在 M2 结束时冻结）
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;                  // "aliyun" | "s3"
    fn info(&self) -> ProviderInfo;                // 描述、支持的引擎、所需凭据
    fn engines(&self) -> Vec<Box<dyn Engine>>;     // 按优先级排列（首选在前）
    fn validate_cfg(&self, cfg: &Profile) -> Result<Validation>;
    fn plan_mount(&self, cfg: &Profile, eng: &dyn Engine, rt: &RuntimeDirs)
        -> Result<MountPlan>;
}

// Engine：一个具体后端可执行程序（s3fs-fuse / aliyundrive-fuse / qrypt ...）
pub trait Engine: Send + Sync {
    fn id(&self) -> &'static str;                  // "s3fs-fuse"
    fn detect(&self) -> DetectResult;              // 是否已装、路径、版本
    fn min_version(&self) -> Option<VersionReq>;
    fn required_deps(&self) -> Vec<Dependency>;    // fuse3, libcurl ...
    fn install_plan(&self, env: &HostEnv) -> InstallPlan; // 分发行版策略
    fn capabilities(&self) -> Capabilities;        // 读写/allow_other/前台/系统d
    fn token_kind(&self) -> TokenKind;             // LegacyWeb | OpenApi| AwsKey
}

// MountPlan：一次挂载的完整“纯数据”描述，便于单测与 dry-run
pub struct MountPlan {
    pub program: PathBuf,
    pub argv: Vec<OsString>,           // 绝不含明文密钥
    pub env: Vec<(OsString,OsString)>, // 仅非敏感运行参数
    pub preflight: Vec<Preflight>,     // 写 0600 凭据文件、检查 /etc/fuse.conf
    pub mountpoint: PathBuf,
    pub unmount: UnmountStrategy,      // FuseMount3 | Umount | Lazy
}
```

设计要点：
- **MountPlan 为纯数据**：命令构造与执行解耦，可在无 FUSE 环境下做单元测试，并支持 `--dry-run`。
- **引擎可插拔**：新增网盘 = 新增 Provider；新增后端工具 = 新增 Engine，不改核心。
- **密钥不经命令行参数**：s3fs 用 `-o passwd_file=`（0600 文件）；阿里云盘用 `--workdir` 下的 0600 `refresh_token` 文件（**不传 `--refresh-token`，不用未经证实的 `REFRESH_TOKEN` env**）。
- **不引入异步运行时**：HTTP 下载用同步 `ureq`，避免为顺序 CLI 引入 tokio。

### 2.3 配置与凭据

配置根：`$XDG_CONFIG_HOME/mount-fs/config.toml`（默认 `~/.config/mount-fs/`）。
运行态：`$XDG_RUNTIME_DIR/mount-fs/`（**目录 0700、文件 0600**，tmpfs）。若 `XDG_RUNTIME_DIR` 缺失（ssh 无会话/cron），回退到 `/tmp/mount-fs-<uid>/`（0700）并输出 WARN；仍不可用则**明确报错退出**。
缓存：`$XDG_CACHE_HOME/mount-fs/`。
状态：`$XDG_STATE_HOME/mount-fs/mounts.json`（advisory，权威来源见 §2.6）；日志 `.../logs/<profile>.log`（0600）。

```toml
[general]
mount_root = "~/mnt"
allow_other = false
engine_auto_fallback = true
log_level = "info"

[profiles.pan]
provider = "aliyun"
engine = "aliyundrive-fuse"      # 或 "qrypt"（写）/"aliyundrive-webdav"
mountpoint = "~/mnt/pan"
token_kind = "legacy_web"        # 与所选引擎绑定，见 §2.4.2
credentials_ref = "file:/home/u/.config/mount-fs/credentials/pan.token"  # 显式路径、0600
allow_other = false

[profiles.bucket]
provider = "s3"
engine = "s3fs-fuse"
mountpoint = "~/mnt/bucket"
bucket = "mybucket"
endpoint = "https://minio.example.com"
region = "us-east-1"
path_style = true
credentials_ref = "file:/home/u/.config/mount-fs/credentials/bucket.s3fs" # 0600
options = { uid = 1000, gid = 1000, umask = "077", ro = false,
            cache = "~/.cache/mount-fs/bucket" }
```

凭据策略（分层，按可用性降级）：
1. 系统 keyring（secret-service）—— **v1 非默认**，作为可选 cargo feature（`keyring`），M4 之后提供。
2. 0600 文件（显式路径）—— v1 默认持久化存储。
3. 环境变量（`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`；阿里云盘令牌用专用只读环境变量供脚本场景）。
4. 交互式输入（`dialoguer`，不回显，经 **stdin**）。

**密钥输入/输出硬约束**：
- **禁止**通过 `mount-fs` 自身 argv 传密钥（如 `config set token=xxx` 会泄漏到 shell history 与 `ps`）。密钥只能经 **stdin/交互/编辑器** 输入。
- **env 变量仅为 mount-fs 的输入渠道**：如 `AWS_ACCESS_KEY_ID` 等由 mount-fs 读取后，**始终**转写入 `passwd_file` 再交给 s3fs；**不**直接 passthrough 环境变量（s3fs 自身使用的是 `AWSACCESSKEYID`/`AWSSECRETACCESSKEY`，名称/行为不同，勿混用）。
- `config get` **永不**输出密钥值（仅输出 `<redacted>` 或来源类型）。
- 日志对密钥脱敏；写文件后强制 `0600`（目录 `0700`）并回读校验权限。

### 2.4 Provider 细节

#### 2.4.1 S3 Provider
- 首选引擎 `s3fs-fuse`。安装优先级（`--method auto` 的**显式规则**）：
  1. **包管理器**：Fedora `dnf install s3fs-fuse`；Debian/Ubuntu `apt install s3fs-fuse`；Arch `pacman -S s3fs-fuse`；RHEL/EPEL 同。命中即用。
  2. **源码编译**（仅 `--method source` 或用户显式同意后）：安装构建依赖（`fuse3-devel libcurl-devel libxml2-devel openssl-devel automake gcc-c++ mime.types`）后 `./autogen.sh && ./configure --with-openssl && make && sudo make install`。
  - **纠正**：上游 GitHub Release **无预编译二进制**（仅源码 tarball），故不存在 “release 二进制” 通道。
  - `--method auto` **绝不自动源码编译、绝不静默 sudo**；需要提权时先 `--dry-run` 展示并征得同意。
- 预检：`/dev/fuse` 存在；`fusermount3` 可用；若 `allow_other` 则检查 `/etc/fuse.conf` 是否含 `user_allow_other`（**仅在 `doctor --fix` 且授权时**修改，见 §3.3）。
- 挂载命令（示例，密钥在 passwd_file）：
  `s3fs <bucket> <mnt> -f -o passwd_file=<0600> -o url=<endpoint> -o use_path_request_style -o region=<r> -o sigv4 -o uid=.. -o gid=.. -o umask=..`
- 卸载：`fusermount3 -u <mnt>`（非特权）；`--force` → `fusermount3 -uz`（lazy）；root 可用 `umount`。
- Rclone / Mountpoint-S3 列为后续可选引擎，v1 仅 s3fs-fuse。

#### 2.4.2 Aliyun Provider
- 引擎优先级：`aliyundrive-fuse`（用户指定首选）→ `qrypt`（活跃、可写、直接 FUSE3）→ `aliyundrive-webdav`+rclone（WebDAV 桥接，低维护）。
- **令牌体系按引擎区分（关键）**：
  - `aliyundrive-fuse`：旧版**网页 token**（legacy_web）。
  - `qrypt`：**OpenAPI refresh_token + drive_id**。
  - `aliyundrive-webdav`：**OpenAPI refresh_token (+ client_id/secret)**。
  - 三者令牌**不通用**；`config login aliyun --engine <id>` 必须按引擎采集对应令牌类型。
- **v1 登录方式：手动粘贴令牌**（经 stdin，写入 0600 文件）。二维码/OAuth 登录**移出 v1**，理由：目标旧 API 正在退场、QR 流程是进度最大风险项。
- `aliyundrive-fuse` 引擎安装优先级：1) GitHub Release musl tarball（按 arch 选择，**校验内嵌固定 sha256**，见 §2.5）；2) 上游 rpm/deb 兜底；~~3) `cargo install`~~（移除：归档 2022 crate 对新 rustc 易碎裂，列为 experimental）。
- 运行：引擎前台启动，`--workdir $XDG_RUNTIME_DIR/mount-fs/pan`，令牌以 0600 `refresh_token` 文件写入该 workdir；argv 不含令牌。
- **能力标注**：`aliyundrive-fuse` = 只读 + 旧网页 token；`providers`/`doctor` 明确标注；`engine_auto_fallback=true` 时，若需求为写或被判定不可用则建议切换 `qrypt`。

### 2.5 安装引擎（Installer）与信任链

- 发行版识别：解析 `/etc/os-release` 的 `ID`/`ID_LIKE`/`VERSION_ID`；架构识别 `uname -m`。
- 包管理器映射：`dnf`/`yum`、`apt`、`pacman`、`zypper`、`apk`。缺失时降级到源码/受控产物。
- **sha256 信任链**：Release 产物（当前仅 aliyundrive-fuse 等第三方）不得从“同一发布渠道即时拉取校验和”（同源篡改=校验失效）。改为**随 mount-fs 发布内嵌的固定校验表**（`engine → version → arch → sha256`），仅随 mount-fs 新版本更新；校验失败拒绝安装并删除下载物。
- 下载与解压：`ureq`（rustls）下载、`sha2` 校验、`tar`+`flate2`/`xz2`/`zip` 解压、安装到 `~/.local/bin` 或 `/usr/local/bin`。
- **安装清单（InstallManifest）**：安装时记录 `{engine, version, path, sha256, method, timestamp}` 到 `$XDG_STATE_HOME/mount-fs/installed.json`，`uninstall` 据此**只删除本工具安装的产物**，绝不误删包管理器管理的包。
- 幂等与验证：安装后运行 `<tool> --version` 校验满足 `min_version`；已满足则跳过。
- 提权：优先用户态安装；确需 `/usr`/`/etc` 时检测 `sudo -n`，否则 `pkexec`/提示；任何提权前 `--dry-run` 展示。

### 2.6 挂载生命周期与状态一致性

- **权威来源**：`/proc/self/mountinfo`（解析 FUSE 挂载点、源、类型、选项）。`mounts.json` 仅 advisory。
- **状态对账（reconciliation）**：每次 `list/status/mount/umount` 前对账：
  - state 记“已挂载”但 mountinfo 无 → 标记 stale，提示/清理；
  - mountinfo 有但 state 无 → 标为 external（非本工具管理），仅展示；
  - 引擎 PID 已死但挂载点仍在 → 判定 hung，提示 `umount --force`。
- **幂等挂载**：已挂载且健康时，`mount` = 成功 no-op（返回现有引擎信息），**不重复 spawn**。
- **临时凭据生命周期**：passwd_file / refresh_token 临时文件在 umount、任何挂载失败路径、进程退出时 `unlink`；启动时 GC 扫描 `$XDG_RUNTIME_DIR/mount-fs/` 的陈旧条目。
- **部分失败回滚**：任何失败路径不得遗留孤儿 state / 孤儿临时凭据 / 半装产物（下载未校验→删；解压未移动→删）。
- **并发**：state 文件用 `fs4`（或 fs2）加锁；另加 **每 profile 挂载锁**（runtime 目录 lockfile），防止并发 `mount` 双重 spawn。
- **进程监督**：后台挂载时引擎 stdout/stderr 重定向到 `$XDG_STATE_HOME/mount-fs/logs/<profile>.log`；state 记录 `pid + starttime`（`/proc/<pid>/stat` 的 starttime）防 PID 复用；`logs --follow` 读取该文件；`status` 依据 PID+mountinfo 判定健康。

### 2.7 诊断（doctor）
检查项：`/dev/fuse`、内核 `fuse` 模块、`fuse3`/`fusermount3`、`/etc/fuse.conf`（`user_allow_other`）、发行版与包管理器、后端引擎安装与版本、网络可达性（endpoint / aliyun api）、systemd 可用性、SELinux 状态、当前用户权限。输出分级（OK/WARN/FAIL）+ 修复建议；支持 `--json`。`--fix` 仅在明确授权下修改系统文件（如追加 `user_allow_other`）。
- **SELinux（Fedora）说明**：用户 FUSE 挂载落在 `fusefs_t`，对 unconfined 用户通常可用；真正的坑是**受限服务**读取该挂载。doctor 给出提示与排查建议，不做阻塞性修改。

### 2.8 CLI 命令面（草案）
```
mount-fs providers [--json]            # 列出 provider/引擎及安装状态
mount-fs doctor [--fix] [--json]       # 环境诊断（授权后可修复）
mount-fs install <engine> [--method auto|pkg|release|source] [--dry-run]
mount-fs uninstall <engine>            # 仅移除本工具安装的产物
mount-fs config init|list|get|set|login|logout
    # set/login 的密钥值只能来自 stdin/交互；get 永不输出密钥
mount-fs mount <profile|provider> [flags]
    # --mountpoint --engine --ro/--rw --allow-other --foreground --dry-run
mount-fs umount <profile|path> [--force] [--all]
mount-fs list [--json]
mount-fs status <profile>
mount-fs logs <profile> [--follow]
mount-fs version
```
全局：`--verbose/-v`、`--quiet`、`--no-color`、`--config <path>`。
**优先级规则**：`mount` 显式 flag > profile 配置 > 全局默认；显式 flag 覆盖时在日志中记录来源。

### 2.9 持久化挂载（systemd，阶段 5）
**用户 `.service`（非 `.mount`）**：`.mount` 需要 `mount.<type>` helper 且无法注入 `--workdir/令牌`，还易把令牌写进 `Options=`。正确做法：
```ini
# ~/.config/systemd/user/mount-fs-pan.service
[Unit]
Description=mount-fs: pan
After=network-online.target
[Service]
Type=simple
ExecStart=%h/.local/bin/mount-fs mount pan --foreground
ExecStopPost=/usr/bin/fusermount3 -u %h/mnt/pan
Restart=on-failure
[Install]
WantedBy=default.target
```
- 凭证须指向**持久 0600 路径**（`$XDG_CONFIG_HOME/mount-fs/credentials/...`），不可用会被重启清空的 `$XDG_RUNTIME_DIR`。
- **实现注意**：`ExecStopPost` 的 `fusermount3 -u` 需容忍“目标已卸载”（如启动即失败），避免损坏 unit 状态或产生噪音；失败时忽略退出码并记录。
- **fstab 仅 root 路径**：`mybucket /mnt/s3 fuse.s3fs _netdev,allow_other,passwd_file=/etc/passwd-s3fs,url=...,use_path_request_style,region=... 0 0`；`_netdev` 只用于 fstab，不用于用户 service。

## 3. 依赖与工程结构

### 3.1 Rust 依赖（拟）
clap(derive)、serde/serde_json/toml、thiserror/anyhow、tracing/tracing-subscriber、etcetera 或 directories、which、**ureq(rustls)**（同步 HTTP，不用 tokio/reqwest）、sha2、flate2/tar/xz2/zip、rustix(或 nix)（uid/gid/权限/信号）、users、semver、dialoguer、indicatif、**fs4**、tempfile。
可选特性：`keyring`、`systemd`、`qrypt`、`webdav-bridge`。`qrcode` 依赖**移除**（v1 无 QR 登录）。

### 3.2 仓库结构（拟）
```
Cargo.toml
src/
  main.rs / cli.rs
  core/{orchestrator.rs, state.rs, reconcile.rs, diagnostics.rs, creds.rs, locking.rs, supervisor.rs}
  providers/{mod.rs, aliyun/, s3/}
  engines/{mod.rs, s3fs.rs, aliyundrive_fuse.rs, qrypt.rs, aliyundrive_webdav.rs}
  system/{distro.rs, pkg.rs, privilege.rs, fuse.rs, process.rs, download.rs, manifest.rs, checksums.rs, systemd.rs}
  config/{mod.rs, schema.rs}
  error.rs
tests/ (integration, fake engines)
dev-docs/ (本方案 + 开发计划)
```

### 3.3 安全与合规
- 密钥不进 argv；`config get`/日志脱敏；文件 0600、目录 0700 强校验。
- 下载必须校验**内嵌固定 sha256**，失败拒绝安装。
- 提权最小化，所有系统修改可 `--dry-run`。
- **不修改系统文件（`/etc/fuse.conf`）除非显式 `doctor --fix` 且获授权**（与 §2.4.1 对齐）。
- `uninstall` 依据 InstallManifest，避免误删。

## 4. 测试与验证策略

- 单元：配置序列化、MountPlan 构造（纯函数）、passwd_file 生成与权限、发行版识别（fixture 文件）、版本比较、mountinfo 解析、state 对账状态机。
- 集成（无真实 FUSE）：`PATH` 注入**假后端脚本**记录 argv/env 并返回；验证 `mount/umount/list/doctor` 全流程，并**断言录制 argv 中不含密钥**、凭据文件 0600/目录 0700。
- 真实 e2e（CI 尽力而为）：Docker 起 MinIO，真实 `s3fs-fuse` 挂载读写冒烟；托管 runner 上 FUSE 不稳定，**标记 best-effort**，以 MinIO API 级测试为确定性门禁，必要时用自托管 VM。阿里云盘走 mock 或人工矩阵。
- CI：GitHub Actions（fmt/clippy/test + cargo-deny/audit + 容器化 MinIO 集成）；`cargo-dist` 产出 rpm/deb/musl。
- 手工测试矩阵：Fedora + Debian + 一个 EPEL/Arch × 引擎 × RO/RW × 权限。

## 5. 风险与缓解
| 风险 | 影响 | 缓解 |
|---|---|---|
| aliyundrive-fuse 已归档/只读/旧 token | 阿里云盘挂载可能失效 | 引擎抽象 + fallback（qrypt/webdav）；文档/doctor 明确标注 |
| 引擎间令牌体系不通用 | 切换引擎需重新授权 | `token_kind` 与 engine 绑定；login 按引擎采集 |
| 自动安装需 root | 体验受阻 | 优先用户态安装；按需 sudo/pkexec；`--method auto` 不静默提权/编译 |
| allow_other 需改 /etc/fuse.conf | 多用户访问失败 | 检测并提示；`doctor --fix` 授权后追加 |
| 校验和同源拉取失效 | 供应链风险 | 内嵌固定 sha256 表，随 release 更新 |
| 发行版碎片化 | 安装策略复杂 | 分发行版 InstallPlan + 显式源码兜底 |
| 令牌/API 变更 | 后端不可用 | doctor 连通性检查 + 替代引擎建议 |

## 6. 设计决策（原开放问题，已采纳 @oracle 建议）
1. **阿里云盘 fallback 顺序：`qrypt` 优先于 `aliyundrive-webdav`**（单二进制、无额外 WebDAV/rclone 跳；OpenList 列为后续可选引擎）。
2. **keyring：v1 不默认**；先 0600 文件 + env，`keyring` 作为 M4 之后的可选 feature（headless/DBus 不可靠）。
3. **systemd：阶段 5**，以**用户 `.service`** 实现（非 `.mount`）。
4. **自定义引擎模板：v1 不做**（避免任意命令注入；`Engine` trait 为扩展点）。
5. **登录：v1 手动粘贴令牌**，QR/OAuth 推迟。
