# Codex Session Sync

Codex Session Sync 是一个个人、自托管的 Codex 会话同步工具，用于在多台电脑之间安全同步 Codex 对话数据。

桌面端使用 Tauri 2、React 和 TypeScript；本地同步核心与服务端使用 Rust；服务端使用 Axum、SQLite 元数据数据库和本地文件对象存储。

项目地址：<https://github.com/2498950046/codex-session-sync>

## 当前实现

项目当前使用 v4 存储和协议。它支持现代 Codex Home（`sqlite/*.db`）和旧版 `state_5.sqlite` 布局，也支持活动会话和归档会话目录中的 rollout 文件。

远端保存的是与 Provider、工作区无关的不可变对象图：

```text
Revision Root
  └─ Thread Descriptor
       ├─ Whole object
       └─ Chunk Manifest ── Chunk objects
```

对象使用 `(kind, sha256)` 寻址。SQLite 文件不会作为二进制文件直接合并；程序会先导出线程级语义数据，再通过备份、Journal、事务、校验和回滚机制安全恢复到本机。

## 主要功能

- 跨设备同步 Codex 会话，支持命名空间、Revision、Push、Pull 和精确切换。
- 使用三方语义合并处理不同线程的变化；同一线程发生修改冲突时要求用户明确选择。
- 本地快照、远端历史、恢复点、回收站、存储统计和垃圾回收。
- Provider 同步、工作区路径映射和基于本地身份的命名空间自动选择。
- Windows 桌面端应用内更新：启动时检查 GitHub Releases，显示更新弹窗，支持手动检查、查看更新说明、签名校验、下载、安装和重启。

## 安全边界

执行快照、导入、恢复、Push、Pull、冲突解决、命名空间切换或清理前，Codex 必须完全退出。后端会重新检测进程，不会自动结束 Codex。

所有写入真实 Codex Home 的操作都会先创建备份和操作 Journal，完成后进行校验；失败时尝试回滚，并支持重启恢复。

原始 API key 不会上传，也不会通过 IPC 返回。远端 Bearer Token 只保存到操作系统凭据库；命名空间自动选择只保存与服务器 URL 绑定的本地 HMAC 指纹。

## 本地开发

先进入项目根目录。下面的命令会安装前端依赖并启动 Tauri 开发模式；`npm install` 只需在依赖发生变化或首次运行时执行。

```powershell
cd F:\codex-session-sync\apps\desktop
npm install
npm run tauri -- dev
```

启动服务端时，`SYNC_SERVER_TOKEN` 是服务端要求的 Bearer Token，`SYNC_SERVER_DATA_DIR` 是服务端元数据和对象的持久化目录。请替换成自己的随机 Token 和数据目录。

```powershell
$env:SYNC_SERVER_TOKEN = "replace-with-a-long-random-token"
$env:SYNC_SERVER_DATA_DIR = "D:\codex-session-sync-data"
cargo run -p sync-server
```

也可以使用 `deploy/server` 下的 Docker Compose 配置。示例中的直接 IP 和 HTTP 配置只适合可信内网测试；长期使用或通过互联网访问时应放在 HTTPS 反向代理后面。

## 发布前验证

`cargo fmt` 检查 Rust 格式；`cargo test` 运行整个 Rust 工作区的测试；`cargo check` 检查 Rust 是否可以编译；Clippy 负责静态分析，并且把所有警告当作错误。

进入 `apps/desktop` 后，`npm run check` 检查 TypeScript 类型；`npm run build` 构建生产前端；`npm test -- --run` 运行前端自动化测试。

下面的命令应逐行执行。PowerShell 不使用反斜杠 `\` 连接命令；如果必须换行，应使用反引号 `` ` ``。

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd apps/desktop
npm run check
npm run build
npm test -- --run
```

如果这些命令全部成功，通常表示代码格式、Rust 编译、静态分析、前端类型、生产构建和自动化测试都已通过。

## 本地打包

`tauri build` 会构建桌面应用和安装包。当前配置的 Windows 目标是 NSIS 安装包。

```powershell
cd F:\codex-session-sync\apps\desktop
npx tauri build
```

## GitHub Releases 和应用内更新

发布工作流位于 `.github/workflows/release.yml`。版本号必须在 `Cargo.toml`、`Cargo.lock`、`apps/desktop/package.json`、`apps/desktop/src-tauri/tauri.conf.json` 和部署示例中保持一致。

项目提供了版本号脚本。脚本参数只接受不带 `v` 的语义化版本号，例如 `0.1.5`。执行前可以先用 `-WhatIf` 预览，确认后再实际修改文件。

```powershell
cd F:\codex-session-sync
.\scripts\set-version.ps1 -Version 0.1.5 -WhatIf
.\scripts\set-version.ps1 -Version 0.1.5
```

脚本完成后，先运行上面的验证命令，再提交代码并推送版本标签。GitHub Actions 只会响应 `v` 开头的标签，例如 `v0.1.5`。

```powershell
git add .
git commit -m "Release 0.1.5"
git tag v0.1.5
git push origin main --tags
```

发布工作流会构建安装包、生成 SHA-256 校验文件、生成 Tauri 更新签名，并把 GitHub Release Notes 写入 `latest.json`。客户端启动时会读取：

```text
https://github.com/2498950046/codex-session-sync/releases/latest/download/latest.json
```

首次发布更新功能前，需要在 GitHub 仓库的 `Settings → Secrets and variables → Actions` 中添加：

- `TAURI_SIGNING_PRIVATE_KEY`：与 `apps/desktop/src-tauri/tauri.conf.json` 中公钥对应的私钥。
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：如果私钥设置了密码才需要添加。

私钥绝不能提交到 Git。公钥可以公开，它用于验证安装包签名；`.sig` 是针对某个安装包生成的公开数字签名，客户端用内置公钥验证它是否来自对应私钥且安装包没有被篡改。

## 本地仓库目录

默认本地同步仓库为 `~/.codex-session-sync`，与真实 Codex Home（`~/.codex`）相互独立：

```text
.codex-session-sync/
├─ objects/{whole,chunks,chunk-manifests,threads,revision-roots}/sha256/
├─ objects/tmp/
├─ snapshots/
├─ metadata/snapshots/
├─ backups/
├─ journal/
├─ trash/snapshots/
├─ trash/gc/
├─ quarantine/
└─ index/source-objects-v4.json
```

## Docker 部署

服务端 Dockerfile 位于 `apps/sync-server/Dockerfile`，Compose 文件位于 `deploy/server`。服务端以非 root 用户运行，并将数据持久化到 `/data`。生产环境请配置 HTTPS、强随机 Bearer Token 和持久化卷。

## 许可证

MIT
