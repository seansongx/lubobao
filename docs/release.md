# 发版流程

录播宝的安装包在 GitHub Actions 里构建，GitHub Release 人工审核发布后，自动上传到 xiaofany.com 分发平台（R2，`dl.xiaofany.cn`），官网和应用内更新都走 xiaofany.com，GitHub Release 作为备用线路：下载地址的备用，以及应用内更新的第二个接口（Release 上的 `latest.json`）。

| 环节 | 位置 | 触发 |
| --- | --- | --- |
| 测试 | `.github/workflows/test.yml` | 每次推送、PR |
| 打包、签名、草稿 Release | `.github/workflows/release.yml` | 推送 `v*` 标签（手动运行只打包，不建 Release） |
| Linux CLI、Docker 镜像 `<版本>` | `.github/workflows/cli.yml`（由 release.yml 调用） | 同上 |
| 上传到 xiaofany.com；把更新说明写进 `latest.json` | `.github/workflows/publish-xiaofany.yml` | Release 发布后；也可手动输入 tag |
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

### 2. 设置 release 环境和三个 Secret

更新签名私钥只放在 GitHub Environment `release` 里：只有推送 `v*` 标签的构建进入这个环境、拿得到私钥；手动运行 release.yml 和 fork 一律不签名。

1. 创建环境：仓库 Settings → Environments → New environment，名字填 `release`。
2. 部署规则：在 `release` 环境的「Deployment branches and tags」选「Selected branches and tags」，只加一条 **tag** 规则 `v*`（不加任何分支）。可以不设审批人。
3. 把两个签名 Secret 设到这个环境（在仓库目录执行，会提示粘贴值，不会留在命令历史里）：

```powershell
Get-Content "$HOME\.lubobao-keys\updater.key" -Raw | gh secret set TAURI_SIGNING_PRIVATE_KEY --env release --repo seansongx/lubobao
```

```powershell
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --env release --repo seansongx/lubobao
```

4. **删除仓库级的同名 Secret**（之前设在仓库级的话），否则任何能改 workflow 的分支都能读到私钥：

```powershell
gh secret delete TAURI_SIGNING_PRIVATE_KEY --repo seansongx/lubobao
```

```powershell
gh secret delete TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo seansongx/lubobao
```

5. 发布到 xiaofany.com 的 Token 设在仓库级（publish-xiaofany.yml 在 Release 发布时运行，不经过 release 环境）：

```powershell
gh secret set RELEASE_TOKEN --repo seansongx/lubobao
```

核对：`gh secret list --env release --repo seansongx/lubobao` 只列出两个签名 Secret；`gh secret list --repo seansongx/lubobao` 里有 `RELEASE_TOKEN`，没有签名 Secret。

| Secret | 位置 | 内容 | 用在 |
| --- | --- | --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | 环境 `release` | `updater.key` 的完整内容 | release.yml 推送 `v*` 标签时签更新包 |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 环境 `release` | 生成密钥时设的密码 | 同上 |
| `RELEASE_TOKEN` | 仓库 | xiaofany.com 后台「API Token」里新建的 Token，权限只勾 `release:lubobao`，建议设过期时间 | publish-xiaofany.yml |

xiaofany.com 后台还要先建好软件：slug `lubobao`、框架 `tauri`，「同步到 GitHub Releases」保持关闭（备用线路由发布时登记的 GitHub 地址提供）。

签名规则：

- 手动运行 release.yml、在 fork 里运行：不进入 release 环境，一律不签名，给出 warning，不生成更新包，其余照常打包；
- 推送 `v*` 标签时 release 环境里缺签名私钥，或公钥还是占位符：构建直接失败，不会发出一个无法应用内更新的正式版；
- publish-xiaofany.yml 缺 `RELEASE_TOKEN` 时给出 notice 后跳过。

## 每次发版

1. **改版本号、写更新说明**：`node scripts/set-version.mjs 0.1.1`（同时改 package.json、package-lock.json、tauri.conf.json、Cargo.toml、Cargo.lock），`node scripts/check-version.mjs` 核对。在 `docs/release-notes/<版本>.md`（例如 `docs/release-notes/0.1.1.md`）写好本版本的更新说明，和版本号一起提交并合入 main。这个文件只写「更新内容」标题下面的正文，不带标题和分隔注释；只用纯文本和简单的 Markdown（标题、列表、粗体、链接），应用的更新对话框只认这些。0.1.0 的说明见 [docs/release-notes/0.1.0.md](release-notes/0.1.0.md)。
2. **打标签**：`git tag v0.1.1`，`git push origin v0.1.1`。release.yml 先检查标签与版本号一致，再在 Windows、macOS（arm64、x64）、Linux 上打包，用 tauri.conf.json 里的公钥校验每个 `.sig`（私钥和公钥不是一对时失败），用 `scripts/updater-latest-json.mjs` 生成 GitHub 兜底更新清单 `latest.json`（四个平台的签名和 Release 下载地址，缺任何一个平台的更新包或 `.sig` 都失败；此时还没有更新说明），然后建草稿 Release，附上全部文件、`SHA256SUMS.txt` 和 `latest.json`。`latest.json` 不在 `SHA256SUMS.txt` 里：发布后它会被改写。Docker 镜像以 `<版本>` 推到 GHCR。
3. **审核草稿**：在 GitHub 的 Releases 页面打开草稿。
   - 把正文最上面「更新内容」里的占位句（以「- （发布前把这里换成本版本的变化」开头的那一行）整行换成 `docs/release-notes/<版本>.md` 的全部内容；保留上面的「## 更新内容」标题和下面的 `<!-- 以上为应用内更新说明 -->`。这一行以上的部分会显示在应用的更新对话框里，也是 xiaofany.com 上的版本说明；以下的安装说明不会。占位句没换掉时，上传到 xiaofany.com 会失败。
   - 用命令行编辑也可以，先把草稿正文存下来，替换占位行后写回：`gh release view v0.1.1 --json body --jq .body > body.md`，编辑后 `gh release edit v0.1.1 --notes-file body.md`。
   - 核对文件：`*_windows_x64-setup.exe`(.sig)、`*_macos_{arm64,x64}.dmg`、`*_macos_{arm64,x64}.app.tar.gz`(.sig)、`*_linux_amd64.AppImage`(.sig)、`*_linux_amd64.deb`、`lubobao-cli_*_linux_{x64,arm64}.tar.gz`、`SHA256SUMS.txt`、`latest.json`。
   - 测试版勾选「Set as a pre-release」：发到 xiaofany.com 的 beta 渠道，Docker 不打 `latest`。
4. **发布**：点「Publish release」。之后自动进行：
   - publish-xiaofany.yml：下载 Release 的全部文件并校验 `SHA256SUMS.txt`，生成发布清单（每个文件的 platform、arch、format、kind、签名和 GitHub 备用地址），上传到 xiaofany.com 并发布；然后核对 `https://xiaofany.com/api/v1/products/lubobao/latest` 的版本号，以及 Tauri 升级接口对旧版本（Windows、macOS 两种芯片、Linux）都返回这个版本和签名。
   - publish-xiaofany.yml 的另一个 job（不需要 `RELEASE_TOKEN`）：把正文里分隔注释以上的更新说明写进 Release 上的 `latest.json`（`notes`，`pub_date` 改为当时），重新上传。占位句没换掉时失败，`latest.json` 保持没有更新说明，仍然可用。
   - docker-latest.yml：把 `<版本>` 镜像加上 `latest`。
5. **确认**：Actions 里两个 workflow 都成功；官网下载按钮拿到新版本；装着旧版本的录播宝在 设置 → 关于 →「检查更新」能看到新版本。

重跑：publish-xiaofany.yml 可以在 Actions 页面手动运行（tag 填 `v0.1.1`），两个 job 都会重跑。版本已经发布时安全跳过（`--skip-existing`）；上次中途失败留下的草稿会被覆盖重传（`--force`）。手动运行只用于已正式发布的 Release，草稿会被拒绝（草稿在 GitHub 上的下载地址是 404，备用线路会失效）。

## 出问题时

| 情况 | 处理 |
| --- | --- |
| 打包失败、草稿有问题，还没发布 | 删除草稿 Release 和标签（`git push origin :refs/tags/v0.1.1`、`git tag -d v0.1.1`），修好后重新打标签。GHCR 上已推送的 `<版本>` 镜像在 GitHub 的 Packages 页面删除 |
| 已发布到 GitHub，上传 xiaofany.com 失败 | 看 publish-xiaofany.yml 的日志，修好原因（Token 过期、占位句没换等）后手动运行该 workflow |
| 已发布的版本有严重问题 | 在 xiaofany.com 后台**撤回**该版本：下载和应用内更新接口立即回退到上一个已发布版本，还没升级的用户不会再收到它。已经装上的用户不会被降级，尽快发布更高版本号的修复版。撤回的版本号不能再用 |
| 同时撤回 GitHub 上的版本 | 把 GitHub Release 改回草稿或删除：`releases/latest` 只指向最新的、已发布的非预发布版本，改回草稿后兜底接口 `releases/latest/download/latest.json` 自动回到上一版的 `latest.json`（上一版没有这个文件时返回 404，应用当作接口失败）。只撤回 xiaofany.com、GitHub 仍保留时，xiaofany.com 正常期间不受影响，它故障时兜底接口仍会给出这个版本，所以有严重问题的版本两边都要撤回。不要只删 `latest.json` 而保留 Release：兜底接口会 404，而不是回到上一版；Docker `latest` 用 `docker buildx imagetools create --tag ghcr.io/seansongx/lubobao:latest ghcr.io/seansongx/lubobao:<上一版本>` 指回上一版 |
| 私钥泄露 | 立即在 GitHub 删除 release 环境里的签名 Secret，生成新密钥、换掉 tauri.conf.json 里的公钥，发布新版本。旧版本的应用只认旧公钥，收不到新密钥签的更新，需要提示用户手动下载一次新版本 |
| `RELEASE_TOKEN` 泄露 | 在 xiaofany.com 后台吊销并重建，更新 Secret |

## 应用内更新怎么工作

- 接口（tauri.conf.json 的 `plugins.updater.endpoints`，按顺序尝试）：
  1. `https://xiaofany.com/api/update/lubobao/tauri/{{target}}/{{arch}}/{{current_version}}`，有更新时 200，没有时 204。请求带 `X-Client-Id`（本机随机生成，存在应用数据目录的 `client-id`），服务端用来灰度。
  2. 兜底：`https://github.com/seansongx/lubobao/releases/latest/download/latest.json`，Tauri 静态清单（`version`、`notes`、`pub_date`、`platforms` 下 `windows-x86_64`、`darwin-aarch64`、`darwin-x86_64`、`linux-x86_64` 各自的 `signature` 和 `url`）。不带 `X-Client-Id`，也没有灰度和强制更新标记；只给最新的已发布正式版，预发布不会经它推送。
- 第一个接口连不上、超时、返回错误状态或无效数据时才查第二个；xiaofany.com 返回 204（包括灰度没轮到）时到此为止，不会被 GitHub 绕过。每个接口单独计时（见下面的时限），两个都卡住时最长约一分钟才报网络错误。插件的请求头对全部接口生效，所以 `update.rs` 给每个接口单独建 updater，只有 xiaofany.com（调试构建里另加本机地址）带 `X-Client-Id`。
- 每个平台用的文件：Windows 是 NSIS 安装包本身（`-setup.exe` + `.sig`），macOS 是 `.app.tar.gz` + `.sig`，Linux 是 AppImage + `.sig`。deb 安装的用户在应用里只看到「请到官网下载新版本」。
- 签名：下载完成后用 tauri.conf.json 里的公钥校验，还要求签名记录的版本号与接口宣布的一致（`requireSignedVersion`，对两个接口都适用），不符时拒绝安装。
- 安装前与退出一样收尾：有录制时先确认，再停止调度和全部录制、等文件收尾，然后安装并重启（Windows 由安装器装好后打开新版本）。
- 开启「自动检查更新」（设置键 `auto_check_update`，默认开）时，启动约 10 秒后检查一次，之后每 24 小时再检查一次。这个键只对桌面端有效，命令行（`lubobao-cli`）不做自动更新，设置了也不起作用。
- 检查更新时带一个随机设备标识（`X-Client-Id`），只发给升级接口，不发给下载服务器。下载地址必须是 https。检查请求 30 秒内没完成、或连接后 30 秒收不到数据，就报网络错误。
- 安装失败时（只读卷、被安全软件拦下等）录制已经停止：对话框不能关闭，只能「重启录播宝」，重启后恢复监控。
- 更新说明里只有 xiaofany.com、lubobao.xiaofany.com 和本仓库的链接可以点，其他链接显示为纯文本。
- 调试构建可以用环境变量 `LUBOBAO_UPDATER_ENDPOINT` 指向本地测试服务器（逗号分隔可写多个，按顺序尝试，整个替换配置里的接口），发布构建忽略它。兜底逻辑由 `cargo test -p lubobao --test updater_fallback` 用模拟运行时和本机假服务器检查：第一个接口 500、卡住或连不上时用第二个，204 时不再查第二个，GitHub 那一端收不到 `X-Client-Id`。
- fork 自己发版时，要把两个接口换成自己的（第二个里的 `seansongx/lubobao` 换成 fork 的仓库）并换上自己的公钥。

本地 `npm run tauri build` 默认会生成更新包，需要签名私钥；没有时加 `--config '{"bundle":{"createUpdaterArtifacts":false}}'` 跳过更新包。

## 只能等第一次正式发布时验证的部分

- release.yml 在真实 Secret 下的签名、改名和 `.sig` 校验（macOS `.app.tar.gz`、Linux AppImage 的路径在本机没有跑过）；
- publish-xiaofany.yml 的上传、发布和发布后核对；
- GitHub 兜底：Release 上的 `latest.json` 内容，以及发布后 `https://github.com/seansongx/lubobao/releases/latest/download/latest.json` 能下载到（重定向到 GitHub 的文件服务器）。实测时可以在调试构建里设 `LUBOBAO_UPDATER_ENDPOINT=https://127.0.0.1:1/x,https://github.com/seansongx/lubobao/releases/latest/download/latest.json`，并把应用版本临时改低，在 设置 → 关于 检查更新；
- macOS、Linux AppImage 的应用内升级（Windows 已在本机用测试密钥和本地假服务器验证，见 `docs/feature-map.md`）。
