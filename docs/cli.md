# 录播宝命令行（lubobao-cli）

`lubobao-cli` 是录播宝的无界面版本，适合放在没有图形环境的 Linux 服务器、NAS 或容器里长期运行。它和桌面端用同一套核心（平台解析、调度、录制、录后转 MP4），设置项的键名、默认值和校验规则也完全相同，只是没有窗口、托盘和系统通知：开播、下播、录制开始和结束、需处理等事件都写进日志。

- Linux（x64、arm64）：静态链接的单个可执行文件，本身不依赖 glibc 版本、GTK 或 WebKit。自动下载的 FFmpeg（来自 BtbN 的构建）需要 glibc 2.28 及以上（Debian 10、Ubuntu 18.10、RHEL/Rocky 8 起）；Alpine（musl）、CentOS 7 等系统请装好系统的 ffmpeg，用 `--ffmpeg` 指定。
- Docker：`ghcr.io/seansongx/lubobao`，amd64、arm64 多架构，内置已校验的 FFmpeg。
- Windows、macOS 也能运行，主要用于开发和测试；桌面用户请用安装包。

## 安装

### 下载可执行文件

从 [GitHub Release](https://github.com/seansongx/lubobao/releases) 下载对应架构的压缩包（校验值见 `SHA256SUMS.txt`）：

```sh
version=0.1.0
arch=x64            # ARM 服务器（树莓派 4/5、鲲鹏、Graviton 等）用 arm64
tar -xzf lubobao-cli_${version}_linux_${arch}.tar.gz
sudo install -m 755 lubobao-cli_${version}_linux_${arch}/lubobao-cli /usr/local/bin/
lubobao-cli --version
```

压缩包里还有许可证、第三方声明、本文档和 systemd 服务示例。

### FFmpeg

默认在第一次 `run` 时自动下载 FFmpeg 到数据目录（`components/ffmpeg/`），来源是 npm 包 `@seankit/ffmpeg`，优先从 npmmirror 下载，失败时改用 npmjs，按 SHA-256 校验后安装。也可以提前装好：

```sh
lubobao-cli ffmpeg install
lubobao-cli ffmpeg status
```

录 YouTube 还需要 yt-dlp 和 Deno：第一次检测到 YouTube 开播时，在后台从 GitHub 下载到数据目录（`components/youtube/`），按 SHA-256 校验后安装，期间这个直播间显示「正在准备 YouTube 组件」，下载完自动开始录制。它们按 YouTube 的代理设置下载。Docker 镜像里录 YouTube 尚未验证。

自动下载失败时，`run` 会在 1、5、15 分钟后重试，之后每 30 分钟一次，每次都写一行日志；也可以随时手动 `lubobao-cli ffmpeg install`。

自动下载的 FFmpeg 需要 glibc 2.28 及以上。Alpine、CentOS 7 这类系统上请用包管理器装 ffmpeg（例如 `apk add ffmpeg`），再用 `--ffmpeg <路径>` 或环境变量 `LUBOBAO_FFMPEG` 指定，此时不再下载（设为空值等于没设置）：

```sh
LUBOBAO_FFMPEG=/usr/bin/ffmpeg lubobao-cli run
```

## 快速上手

```sh
# 终端 1：前台常驻，监控并自动录制，日志输出到终端（stderr）
lubobao-cli run

# 终端 2：添加直播间、查看状态
lubobao-cli add https://live.douyin.com/123456
lubobao-cli add https://live.kuaishou.com/u/someone --quality HD
lubobao-cli list
lubobao-cli status
```

`run` 运行时，其他子命令通过本地控制通道交给它执行，立刻生效。`run` 没有运行时，修改类命令（`add`、`pause`、`resume`、`remove`、`config set`）直接写数据库，下次 `run` 启动时生效；`stop`、`start`、`shutdown` 必须有 `run` 在运行，否则提示并以退出码 3 结束。

按 Ctrl+C（或发送 SIGTERM、SIGHUP）后，`run` 停止所有录制、等 FFmpeg 写完文件尾（最长约 30 秒）再退出，录好的文件可以直接播放。收尾期间再按一次 Ctrl+C 会立即退出。Windows 上直接关闭控制台窗口时，系统只给约 5 秒，收尾可能来不及，请先按 Ctrl+C 或运行 `lubobao-cli shutdown`。

`run` 没有运行时，只读命令（`list`、`status`、`config list`、`config get`、`ffmpeg status`）以只读方式打开数据库，不会改动任何文件；`add` 先联网解析链接，再写入数据库。

## 数据目录与录像目录

| 系统 | 默认数据目录 |
| --- | --- |
| Linux | `$XDG_DATA_HOME/lubobao`，未设置时 `~/.local/share/lubobao` |
| macOS | `~/Library/Application Support/lubobao-cli` |
| Windows | `%LOCALAPPDATA%\lubobao-cli` |

用 `--data-dir <目录>` 或 `LUBOBAO_DATA_DIR` 覆盖。数据目录里有任务数据库 `lubobao.db`、FFmpeg 组件、实例锁 `run.lock` 和控制通道。`run` 启动时把数据目录权限收紧为 0700（里面有 Cookie 设置和控制通道）。命令行和桌面端不共用数据目录，各自的直播间列表互不影响。

控制通道：

- Linux、macOS：数据目录里权限为 0600 的 `control.sock`。数据目录路径太长（超过约 100 字节），或者数据目录放在不支持 Unix socket 的网络盘上（NAS 挂载的 CIFS/SMB、NFS 等）时，自动改用 `$XDG_RUNTIME_DIR/lubobao-<哈希>.sock`（没有 `XDG_RUNTIME_DIR` 时用临时目录）。也可以用 `--control <路径>` 或 `LUBOBAO_CONTROL` 指定，`run` 和其他子命令要用同一个值。
- Windows：按数据目录命名的命名管道，只允许当前用户访问；子命令连接时会确认 `run` 属于同一个用户。

数据目录在 NAS 上时，建议把数据目录放在本地磁盘、只把录像目录（`live_save_path`）放到 NAS 上；SQLite 数据库在网络文件系统上也容易出现锁的问题。

同一个数据目录同时只能运行一个 `run`，第二个会报错退出。要同时跑几套互不相关的配置，给每套指定不同的数据目录。

录像默认保存在数据目录下的 `recordings`，由设置 `live_save_path` 决定：

```sh
lubobao-cli config set live_save_path /srv/recordings
```

`config set live_save_path` 的相对路径按执行命令时的当前目录转成绝对路径；空值会被拒绝。`--save-dir` / `LUBOBAO_SAVE_DIR` 改的是这个设置的缺省值（Docker 镜像里是 `/recordings`）；用 `config set` 改过之后以设置为准。

## 子命令

全局选项可以放在子命令前后：`--data-dir`、`--ffmpeg`、`--save-dir`、`--control`。

| 子命令 | 作用 |
| --- | --- |
| `run` | 前台常驻：监控直播间并自动录制 |
| `add <链接> [--quality OD\|UHD\|HD\|SD\|LD] [--notify-only] [--note 备注] [--force]` | 添加直播间。画质缺省用设置 `record_quality`；`--notify-only` 开播时只写日志不录制；与已有任务重复时需要 `--force` |
| `list [--json]` | 列出直播间：id、状态、平台、主播、画质、链接，录制中的显示正在写入的文件，需处理的显示原因 |
| `pause <id…\|all>` | 暂停监控，正在录制的会停止 |
| `resume <id…\|all>` | 恢复监控，同时清除录制失败和手动停止的状态 |
| `remove <id…>` | 删除直播间；正在录制的要先 `stop` |
| `stop <id…\|all>` | 停止录制。本次直播不再自动录制，下播后、`start` 或重新 `resume` 后恢复（需要 `run`） |
| `start <id…\|all>` | 重新开始录制：清除手动停止和录制失败的状态，立即检测一次，在播就开始录制。已暂停监控的要先 `resume`，仅通知的不录制（需要 `run`） |
| `status [--json]` | `run` 是否在运行、数据目录、录像目录、FFmpeg、录制数、下次检测时间、各状态的直播间数 |
| `config list [--json]` | 列出全部设置（含默认值） |
| `config get <键>` | 读取一项设置 |
| `config set <键> <值>` | 修改一项设置。字符串直接写；数字、布尔值和对象按 JSON 写 |
| `ffmpeg status [--json]` | FFmpeg 状态 |
| `ffmpeg install` | 下载并安装 FFmpeg；已安装时重新检查 |
| `shutdown` | 让正在运行的 `run` 收尾并退出（与 SIGTERM 相同，Windows 上没有 SIGTERM 时用它） |

退出码：0 成功；1 执行失败，包括重复添加（加不加 `--json` 都一样），错误信息写到 stderr；2 参数或设置值无效；3 需要 `run` 但它没有运行。

### 常用设置

与桌面端「设置」页一一对应，完整列表用 `config list` 查看。

```sh
lubobao-cli config set loop_time_seconds 120          # 检测间隔（秒）
lubobao-cli config set record_quality HD              # 默认画质
lubobao-cli config set video_segment_time 3600        # 分段时长（秒）
lubobao-cli config set convert_to_mp4 true            # 录完转 MP4
lubobao-cli config set delete_original false          # 转完保留 TS
lubobao-cli config set recording_space_threshold 5    # 磁盘剩余低于 5 GB 时停止录制
lubobao-cli config set proxy_address 127.0.0.1:7890   # 代理
lubobao-cli config set platform_cookies '{"douyin":"ttwid=…"}'
```

`auto_check_update`（自动检查更新）只对桌面端有效：命令行不做自动更新，这个键设了也不起作用，升级请重新下载或拉取新镜像。

### 日志

`run` 的日志每行一条：`时间 级别 内容`，写到 stderr。例如：

```text
2026-11-22 20:00:03 INFO  #1 [某主播] 检测到开播
2026-11-22 20:00:04 INFO  #1 [某主播] 开始录制（/srv/recordings/抖音/某主播/某主播_2026-11-22_20-00-04_%03d.ts）
2026-11-22 22:31:40 INFO  #1 [某主播] 录制结束（…）
2026-11-22 22:31:41 INFO  #1 [某主播] 已转为 MP4（…）
2026-11-22 22:40:12 WARN  #2 [另一位] 需处理：需要登录 Cookie
```

在 systemd 下用 `journalctl -u lubobao -f` 查看，在 Docker 里用 `docker logs -f lubobao`。

## systemd

仓库和压缩包里有示例 [`deploy/systemd/lubobao.service`](../deploy/systemd/lubobao.service)：用专用用户 `lubobao` 运行，数据放在 `/var/lib/lubobao`，停止时发 SIGTERM 并留 45 秒收尾，异常退出后自动重启。

```sh
sudo useradd --system --home-dir /var/lib/lubobao --shell /usr/sbin/nologin lubobao
sudo install -m 755 lubobao-cli /usr/local/bin/lubobao-cli
sudo cp lubobao.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now lubobao
journalctl -u lubobao -f
```

管理直播间时一定要用服务的用户和数据目录（`sudo -u lubobao`），命令会交给正在运行的服务执行。用 root 或自己的账号操作服务的数据目录，可能留下服务用户无法读写的文件：

```sh
alias lbb='sudo -u lubobao LUBOBAO_DATA_DIR=/var/lib/lubobao lubobao-cli'
lbb add https://live.douyin.com/123456
lbb list
```

示例里设了 `KillMode=mixed`：只给 `lubobao-cli` 发 SIGTERM，由它通知 FFmpeg 正常结束；45 秒后还没退出才结束剩下的进程。录像想放到别的磁盘时，先 `sudo chown lubobao: /srv/recordings`，再 `lbb config set live_save_path /srv/recordings`。

## Docker

镜像 `ghcr.io/seansongx/lubobao`：推送版本标签时发布 `<版本>`，Release 人工发布后再把同一个镜像标为 `latest`。

- 内置 FFmpeg（`/opt/ffmpeg/ffmpeg`），构建时从 `@seankit/ffmpeg` 下载并按清单的 SHA-256 校验；
- 以非 root 用户（`nonroot`，uid 65532）运行；
- 数据卷 `/data`，录像卷 `/recordings`；
- 基于 `gcr.io/distroless/cc-debian12`（glibc 2.36，没有 shell），带 ca-certificates 和 tzdata，用 `TZ` 设置时区（默认 `Asia/Shanghai`，影响文件名里的时间）；
- 内含的 FFmpeg 为 GPL-3.0-or-later，许可证和源码位置在 `/opt/ffmpeg/LICENSE`、`/opt/ffmpeg/README.md`；
- `STOPSIGNAL SIGTERM`。

```sh
docker run -d --name lubobao --restart unless-stopped --stop-timeout 45 \
  -e TZ=Asia/Shanghai \
  -v lubobao-data:/data -v lubobao-recordings:/recordings \
  ghcr.io/seansongx/lubobao:latest

docker exec lubobao lubobao-cli add https://live.douyin.com/123456
docker exec lubobao lubobao-cli list
docker logs -f lubobao
```

**停止容器时请留够收尾时间**：录制收尾最长约 30 秒，而 `docker stop` 默认只等 10 秒，超时后强制结束会让最后一段文件缺少文件尾。用 `docker stop -t 45 lubobao`，或者像上面那样创建容器时加 `--stop-timeout 45`。

录像或数据想放在宿主机目录（绑定挂载）时，`/data` 和 `/recordings` 对应的目录都要让 uid 65532 可写：

```sh
sudo mkdir -p /srv/lubobao/data /srv/recordings
sudo chown 65532:65532 /srv/lubobao/data /srv/recordings
docker run … -v /srv/lubobao/data:/data -v /srv/recordings:/recordings …
```

镜像内置 FFmpeg（`LUBOBAO_FFMPEG=/opt/ffmpeg/ffmpeg`）。想改为自动下载到 `/data`，加 `-e LUBOBAO_FFMPEG=`。

### Docker Compose

示例 [`deploy/docker/compose.yaml`](../deploy/docker/compose.yaml) 已设 `stop_grace_period: 45s`：

```yaml
services:
  lubobao:
    image: ghcr.io/seansongx/lubobao:latest
    container_name: lubobao
    restart: unless-stopped
    stop_grace_period: 45s
    environment:
      TZ: Asia/Shanghai
    volumes:
      - lubobao-data:/data
      - lubobao-recordings:/recordings

volumes:
  lubobao-data:
  lubobao-recordings:
```

```sh
docker compose up -d
docker compose exec lubobao lubobao-cli add https://live.douyin.com/123456
docker compose stop      # 按 stop_grace_period 等待收尾
```

### 国内拉取较慢时

GHCR 在国内访问较慢或不稳定时，可以：

- 使用提供 `ghcr.io` 加速的镜像服务，把镜像名里的 `ghcr.io` 换成加速地址，例如 `docker pull <加速域名>/seansongx/lubobao:latest`，再 `docker tag` 回原名；
- 在有代理的机器上 `docker pull` 后 `docker save | gzip > lubobao.tar.gz`，复制到服务器上 `docker load`；
- 直接用 Release 里的 `lubobao-cli_*_linux_*.tar.gz`，配合 systemd 运行（FFmpeg 从 npmmirror 下载，国内较快）。

## 常见问题

- **`stop`、`start` 提示 run 没有在运行**：`stop`、`start` 和 `shutdown` 只对正在运行的 `run` 有意义。检查 `status`，或者确认用的是同一个数据目录（systemd、Docker 下数据目录与你的用户默认目录不同）。
- **数据目录被另一个进程占用**：同一个数据目录已有 `run`。用 `status` 查看它的 pid。
- **抖音在海外服务器上检测失败**：部分海外 IP 会被风控，可以配置代理（`proxy_address`）或 Cookie（`platform_cookies`），或把服务器放在国内。
- **Ctrl+C 后 FFmpeg 也收到了中断信号**：在终端里按 Ctrl+C 时信号会发给整个前台进程组，FFmpeg 会自行写完文件尾后退出，文件同样可以播放。systemd 和 Docker 只给 `lubobao-cli` 发信号，由它统一收尾。
