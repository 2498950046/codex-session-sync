# 发布 `0.1.0` 及后续版本

一次正式发布会产出两个不同类型的制品：

- **GitHub Release**：Windows 的 NSIS 安装器、SHA-256 校验文件、Linux 部署用的 `compose.yaml` 和 `.env.example`。
- **GitHub Container Registry (GHCR)**：Linux `amd64` 服务端镜像。GitHub Release 中的 `server-image.txt` 给出对应镜像标签和不可变 digest。

容器镜像不作为 Release 附件上传。将镜像放在 GHCR 能让 Linux 主机用 Docker 按标签或 digest 可靠拉取，也避免把镜像导出为难以管理的 tar 文件。

## 首次准备

1. 在 GitHub 仓库的 **Actions** 页面启用工作流（如果 GitHub 因为新工作流而要求确认），并确认组织/仓库策略没有禁止工作流申请 `contents: write` 和 `packages: write` 权限。工作流已经显式声明这两个最小权限，`GITHUB_TOKEN` 会在发布时创建 Release 并推送 GHCR，不需要把 PAT 放进仓库 Secrets。
2. 在 GitHub 仓库的 **Actions** 页面启用工作流（如果 GitHub 因为新工作流而要求确认）。
3. 首次发布完成后，到仓库的 **Packages** 页面把 `codex-session-sync-server` 设置为 Public；若保持 Private，则每台服务器须以含 `read:packages` 的 GitHub PAT 登录 `ghcr.io` 后才能拉取。

## 发布一个版本

发布 tag 必须与下列三个版本号完全相同：根目录 `Cargo.toml`、`apps/desktop/package.json`、`apps/desktop/src-tauri/tauri.conf.json`。例如发布 `0.1.0`：

可先运行统一版本工具；不传参数时会提示输入版本号，`-WhatIf` 只预览不写入文件：

```powershell
cd F:\codex-session-sync
.\scripts\set-version.ps1
# 或 .\scripts\set-version.ps1 -Version 0.1.1
# 或 .\scripts\set-version.ps1 -Version 0.1.1 -WhatIf
```

该工具还会同步更新发布附件中的 GHCR 镜像示例版本。

```powershell
cd F:\codex-session-sync
git status
git push origin main
git tag -a v0.1.0 -m "Codex Session Sync 0.1.0"
git push origin v0.1.0
```

`v0.1.0` 推送后，`.github/workflows/release.yml` 会依次：验证版本、在 Windows 构建 NSIS、在 Linux 构建并推送 `ghcr.io/2498950046/codex-session-sync-server:0.1.0`、最后创建 GitHub Release 并附加制品。工作流失败时不会创建 GitHub Release；修正后请删除失败的远端 tag，再推送同名 tag，或更推荐发布一个新版本号。

## 在 Linux 服务器部署

从对应 GitHub Release 下载 `compose.yaml` 与 `.env.example`，并执行：

```bash
sudo install -d -m 700 /opt/codex-session-sync
sudo chown "$USER":"$USER" /opt/codex-session-sync
cd /opt/codex-session-sync
# 将下载的两个文件放到这里；再将 .env.example 复制为 .env。
cp .env.example .env
chmod 600 .env
```

编辑 `.env`：把 `OWNER` 改为 `2498950046`，并用 `openssl rand -hex 32` 生成的值替换 `SYNC_SERVER_TOKEN`。若采用 Private GHCR 镜像，先运行 `docker login ghcr.io -u 2498950046`，并输入仅含 `read:packages` 的 PAT。

```bash
docker compose pull
docker compose up -d --remove-orphans
docker compose ps
curl --fail http://127.0.0.1:8787/health
```

数据会保存在 Docker named volume `codex-session-sync-data`。普通停止可用 `docker compose down`；不要使用 `docker compose down -v`，后者会删除服务端数据。公网/长期使用前，务必通过 Caddy、Nginx 或 Traefik 配置 HTTPS，并仅让反向代理对公网开放。
