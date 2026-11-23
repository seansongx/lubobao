# 发版流程

录播宝的安装包在 GitHub Actions 里构建，GitHub Release 人工审核发布后，自动上传到 xiaofany.com 分发平台（R2，`dl.xiaofany.cn`），官网和应用内更新都走 xiaofany.com，GitHub Release 作为备用线路。

| 环节 | 位置 | 触发 |
| --- | --- | --- |
| 测试 | `.github/workflows/test.yml` | 每次推送、PR |
| 打包、签名、草稿 Release | `.github/workflows/release.yml` | 推送 `v*` 标签（手动运行只打包，不建 Release） |
| Linux CLI、Docker 镜像 `<版本>` | `.github/workflows/cli.yml`（由 release.yml 调用） | 同上 |
| 上传到 xiaofany.com | `.github/workflows/publish-xiaofany.yml` | Release 发布后；也可手动输入 tag |
| Docker `latest` | `.github/workflows/docker-latest.yml` | Release 发布后（预发布不打） |

## 一次性准备

只需要做一次。私钥和 Token 都不要提交到仓库，也不要贴到 issue、PR 或日志里。

### 1. 生成应用内更新的签名密钥

在**仓库目录以外**生成（例如用户目录下的专用文件夹），设置一个密码：

```powershell
npx tauri signer generate -w "$HOME\.lubobao-keys\updater.key"
```

得到两个文件：

- `updater.key`：私钥（已用密码加密）。只放进 GitHub Secrets 和你自己的备份。
- `updater.key.pub`：公钥。把文件内容（一整行 base64）填进 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`，替换占位符 `LUBOBAO_UPDATER_PUBKEY_NOT_SET`，然后提交。

**私钥要另外备份**（例如密码管理器里存私钥文件内容和密码，再加一份离线备份）。私钥或密码丢了，就再也不能给已经安装的用户推送更新：他们只能手动下载新版本，新版本还得换一对新密钥。

### 2. 设置三个 Secret

在仓库目录执行（会提示粘贴值，不会留在命令历史里）：

```powershell
Get-Content "$HOME\.lubobao-keys\updater.key" -Raw | gh secret set TAURI_SIGNING_PRIVATE_KEY --repo seansongx/lubobao
```

```powershell
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo seansongx/lubobao
```

```powershell
gh secret set RELEASE_TOKEN --repo seansongx/lubobao
```

| Secret | 内容 | 用在 |
| --- | --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | `updater.key` 的完整内容 | release.yml 打包时签更新包 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成密钥时设的密码 | 同上 |
| `RELEASE_TOKEN` | xiaofany.com 后台「API Token」里新建的 Token，权限只勾 `release:lubobao`，建议设过期时间 | publish-xiaofany.yml |

xiaofany.com 后台还要先建好软件：slug `lubobao`、框架 `tauri`，「同步到 GitHub Releases」保持关闭（备用线路由发布时登记的 GitHub 地址提供）。

没有这些 Secret 时：

- 手动运行 release.yml（或在 fork 里）会给出 warning，不生成更新包，其余照常打包；
- 推送 `v*` 标签时缺签名私钥或公钥还是占位符，构建直接失败，不会发出一个无法应用内更新的正式版；
- publish-xiaofany.yml 缺 `RELEASE_TOKEN` 时给出 notice 后跳过。

## 每次发版

1. **改版本号**：`node scripts/set-version.mjs 0.1.1`（同时改 package.json、package-lock.json、tauri.conf.json、Cargo.toml、Cargo.lock），`node scripts/check-version.mjs` 核对，提交并合入 main。
2. **打标签**：`git tag v0.1.1`，`git push origin v0.1.1`。release.yml 先检查标签与版本号一致，再在 Windows、macOS（arm64、x64）、Linux 上打包，用 tauri.conf.json 里的公钥校验每个 `.sig`（私钥和公钥不是一对时失败），然后建草稿 Release，附上全部文件和 `SHA256SUMS.txt`。Docker 镜像以 `<版本>` 推到 GHCR。
3. **审核草稿**：在 GitHub 的 Releases 页面打开草稿。
   - 把正文最上面「更新内容」里的占位句换成本版本的变化（纯文本或简单的 Markdown：标题、列表、粗体、链接）。`<!-- 以上为应用内更新说明 -->` 以上的部分会显示在应用的更新对话框里，以下的安装说明不会。占位句没换掉时，上传到 xiaofany.com 会失败。
   - 核对文件：`*_windows_x64-setup.exe`(.sig)、`*_macos_{arm64,x64}.dmg`、`*_macos_{arm64,x64}.app.tar.gz`(.sig)、`*_linux_amd64.AppImage`(.sig)、`*_linux_amd64.deb`、`lubobao-cli_*_linux_{x64,arm64}.tar.gz`、`SHA256SUMS.txt`。
   - 测试版勾选「Set as a pre-release」：发到 xiaofany.com 的 beta 渠道，Docker 不打 `latest`。
4. **发布**：点「Publish release」。之后自动进行：
   - publish-xiaofany.yml：下载 Release 的全部文件并校验 `SHA256SUMS.txt`，生成发布清单（每个文件的 platform、arch、format、kind、签名和 GitHub 备用地址），上传到 xiaofany.com 并发布；然后核对 `https://xiaofany.com/api/v1/products/lubobao/latest` 的版本号，以及 Tauri 升级接口对旧版本（Windows、macOS 两种芯片、Linux）都返回这个版本和签名。
   - docker-latest.yml：把 `<版本>` 镜像加上 `latest`。
5. **确认**：Actions 里两个 workflow 都成功；官网下载按钮拿到新版本；装着旧版本的录播宝在 设置 → 关于 →「检查更新」能看到新版本。

重跑：publish-xiaofany.yml 可以在 Actions 页面手动运行（tag 填 `v0.1.1`）。版本已经发布时安全跳过（`--skip-existing`）；上次中途失败留下的草稿会被覆盖重传（`--force`）。手动运行只用于已正式发布的 Release，草稿会被拒绝（草稿在 GitHub 上的下载地址是 404，备用线路会失效）。

## 出问题时

| 情况 | 处理 |
| --- | --- |
| 打包失败、草稿有问题，还没发布 | 删除草稿 Release 和标签（`git push origin :refs/tags/v0.1.1`、`git tag -d v0.1.1`），修好后重新打标签。GHCR 上已推送的 `<版本>` 镜像在 GitHub 的 Packages 页面删除 |
| 已发布到 GitHub，上传 xiaofany.com 失败 | 看 publish-xiaofany.yml 的日志，修好原因（Token 过期、占位句没换等）后手动运行该 workflow |
| 已发布的版本有严重问题 | 在 xiaofany.com 后台**撤回**该版本：下载和应用内更新接口立即回退到上一个已发布版本，还没升级的用户不会再收到它。已经装上的用户不会被降级，尽快发布更高版本号的修复版。撤回的版本号不能再用 |
| 同时撤回 GitHub 上的版本 | 把 GitHub Release 改回草稿或删除；Docker `latest` 用 `docker buildx imagetools create --tag ghcr.io/seansongx/lubobao:latest ghcr.io/seansongx/lubobao:<上一版本>` 指回上一版 |
| 私钥泄露 | 立即在 GitHub 删除 Secret，生成新密钥、换掉 tauri.conf.json 里的公钥，发布新版本。旧版本的应用只认旧公钥，收不到新密钥签的更新，需要提示用户手动下载一次新版本 |
| `RELEASE_TOKEN` 泄露 | 在 xiaofany.com 后台吊销并重建，更新 Secret |

## 应用内更新怎么工作

- 接口：`https://xiaofany.com/api/update/lubobao/tauri/{{target}}/{{arch}}/{{current_version}}`（tauri.conf.json 的 `plugins.updater.endpoints`），有更新时 200，没有时 204。请求带 `X-Client-Id`（本机随机生成，存在应用数据目录的 `client-id`），服务端用来灰度。
- 每个平台用的文件：Windows 是 NSIS 安装包本身（`-setup.exe` + `.sig`），macOS 是 `.app.tar.gz` + `.sig`，Linux 是 AppImage + `.sig`。deb 安装的用户在应用里只看到「请到官网下载新版本」。
- 签名：下载完成后用 tauri.conf.json 里的公钥校验，还要求签名记录的版本号与接口宣布的一致（`requireSignedVersion`），不符时拒绝安装。
- 安装前与退出一样收尾：有录制时先确认，再停止调度和全部录制、等文件收尾，然后安装并重启（Windows 由安装器装好后打开新版本）。
- 调试构建可以用环境变量 `LUBOBAO_UPDATER_ENDPOINT` 指向本地测试服务器，发布构建忽略它。

本地 `npm run tauri build` 默认会生成更新包，需要签名私钥；没有时加 `--config '{"bundle":{"createUpdaterArtifacts":false}}'` 跳过更新包。

## 只能等第一次正式发布时验证的部分

- release.yml 在真实 Secret 下的签名、改名和 `.sig` 校验（macOS `.app.tar.gz`、Linux AppImage 的路径在本机没有跑过）；
- publish-xiaofany.yml 的上传、发布和发布后核对；
- macOS、Linux AppImage 的应用内升级（Windows 已在本机用测试密钥和本地假服务器验证，见 `docs/feature-map.md`）。
