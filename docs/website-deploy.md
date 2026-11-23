# 官网部署（lubobao.xiaofany.com）

官网是 `website/` 下的静态页面，由 xiaofany.com 所在的国内备案服务器上已有的 nginx 提供服务，子域名 `lubobao.xiaofany.com`。安装包不放在这台服务器上：下载按钮和安装脚本都走 xiaofany.com 分发平台（`/dl/lubobao/…`、`/api/v1/products/lubobao/…`）。

push 到 main 且 `website/` 有改动时，GitHub Actions 的 [`deploy-site.yml`](../.github/workflows/deploy-site.yml) 用 rsync 把 `website/` 同步到服务器的 `/var/www/lubobao`，再用 curl 检查首页和两个安装脚本。也可以在 Actions 页面手动运行「Deploy website」。

仓库是公开的，所以部署用的是一个专门的受限用户 `lubobao-site`：它的密钥在 `authorized_keys` 里被限定为 `command="rrsync /var/www/lubobao",restrict`，只能在网站目录里同步文件，不能登录 shell、不能转发端口，也读写不到别的目录。Secret 没配好之前，workflow 只输出一条提示，不会报错。

下面是第一次上线时要做的事，按顺序来。命令里的 `你的服务器` 指 SSH 连接用的地址（IP 或域名）。

## 1. DNS

在 xiaofany.com 的域名解析里加一条记录：

| 主机记录 | 类型 | 记录值 |
| --- | --- | --- |
| `lubobao` | A | 服务器的公网 IPv4 |

服务器有 IPv6 时再加一条 AAAA。生效后检查：

```sh
dig +short lubobao.xiaofany.com
```

子域名和主域名在同一个备案主体下，不需要单独备案。页脚已经写了备案号「鲁ICP备17049248号-1」并链接到 https://beian.miit.gov.cn/ 。

## 2. 准备网站目录和部署用户

把仓库里的 [`deploy/site-setup.sh`](../deploy/site-setup.sh) 传到服务器。先在**你自己的电脑上**生成一把只用于部署的密钥（不要设密码，不要和别的用途共用）：

```sh
ssh-keygen -t ed25519 -N '' -C lubobao-site-deploy -f lubobao-site-deploy
```

得到 `lubobao-site-deploy`（私钥）和 `lubobao-site-deploy.pub`（公钥）。把公钥传到服务器，以 root 执行（第二个参数是 GitHub Actions 连接时用的地址，第三个是 SSH 端口，默认 22）：

```sh
sudo sh site-setup.sh lubobao-site-deploy.pub 你的服务器 22
```

脚本可以重复执行（换密钥时也是再执行一次），它会：

- 安装 rsync（以及 rrsync 依赖的 python3），找到 `rrsync`。rsync 3.2.3 以后的 Debian、Ubuntu 自带 `/usr/bin/rrsync`；更早的版本从文档目录复制一份到 `/usr/local/bin/rrsync`；
- 创建系统用户 `lubobao-site`（shell 为 `/bin/sh`，sshd 要用它执行强制命令；不能用密码登录）；
- 创建 `/var/www/lubobao`，归 `lubobao-site` 所有，权限 755，nginx 只需要读；
- 写入 `/var/lib/lubobao-site/.ssh/authorized_keys`（归 root，部署用户改不了）：

  ```
  command="/usr/bin/rrsync /var/www/lubobao",restrict ssh-ed25519 AAAA… lubobao-site-deploy
  ```

- 打印 `SITE_DEPLOY_KNOWN_HOSTS` 要用的几行。

网站目录默认是 `/var/www/lubobao`。用宝塔管理 nginx 时可以放在宝塔的站点目录，运行时用环境变量指定（`rrsync` 会限定在这个目录）：

```sh
sudo SITE_DIR=/www/wwwroot/lubobao.xiaofany.com sh site-setup.sh lubobao-site-deploy.pub 你的服务器 22
```

这时第 3、4 步改在宝塔面板里做：添加纯静态站点并把根目录设为同一个目录，用面板申请 Let's Encrypt 证书，再把 [`deploy/nginx/lubobao.xiaofany.com.conf`](../deploy/nginx/lubobao.xiaofany.com.conf) 里的 `map`、`add_header`、安装脚本和隐藏文件的 `location` 加进站点的配置文件（隐藏文件规则写成 `location ~ /\.(?!well-known/)`，免得挡住证书续期）。宝塔建站后会往目录里放默认文件并改属主，要删掉这些文件，再把目录交还给 `lubobao-site`。

如果 `/etc/ssh/sshd_config` 里设置了 `AllowUsers` 或 `AllowGroups`，把 `lubobao-site` 加进去，然后 `sudo sshd -t && sudo systemctl reload ssh`（RHEL 系是 `sshd`）。

## 3. 证书

证书要在 nginx 配置生效前准备好。下面两种做法选一种，都用 HTTP-01 webroot 验证：nginx 配置里的 80 端口已经为 `/.well-known/acme-challenge/` 留了位置。

**第一次申请时 443 还没有证书**，可以先只放 80 端口那一段 `server`（或者先用 standalone 模式），拿到证书后再放完整配置。

### acme.sh

```sh
sudo mkdir -p /var/www/acme
# 先让 80 端口生效（只放配置里的第一个 server，ACME 目录填 /var/www/acme），然后：
acme.sh --issue -d lubobao.xiaofany.com -w /var/www/acme
sudo mkdir -p /etc/nginx/ssl/lubobao.xiaofany.com
acme.sh --install-cert -d lubobao.xiaofany.com \
  --fullchain-file /etc/nginx/ssl/lubobao.xiaofany.com/fullchain.pem \
  --key-file       /etc/nginx/ssl/lubobao.xiaofany.com/key.pem \
  --reloadcmd      "systemctl reload nginx"
```

acme.sh 会自动续期，并在续期后执行 `--reloadcmd`。

### certbot

```sh
sudo mkdir -p /var/www/acme
sudo certbot certonly --webroot -w /var/www/acme -d lubobao.xiaofany.com \
  --deploy-hook "systemctl reload nginx"
```

证书在 `/etc/letsencrypt/live/lubobao.xiaofany.com/fullchain.pem` 和 `privkey.pem`，certbot 的定时任务会自动续期。

服务器上已经在用某一种（xiaofany.com 的证书怎么申请的就用哪种），沿用即可。

## 4. nginx

把 [`deploy/nginx/lubobao.xiaofany.com.conf`](../deploy/nginx/lubobao.xiaofany.com.conf) 放进宿主机 nginx 的站点目录，替换三个占位符：

| 占位符 | 例子 |
| --- | --- |
| `__SSL_CERTIFICATE__` | `/etc/nginx/ssl/lubobao.xiaofany.com/fullchain.pem` |
| `__SSL_CERTIFICATE_KEY__` | `/etc/nginx/ssl/lubobao.xiaofany.com/key.pem` |
| `__ACME_WEBROOT__` | `/var/www/acme` |

```sh
sudo cp lubobao.xiaofany.com.conf /etc/nginx/conf.d/        # 或 sites-available/ 再链接到 sites-enabled/
sudo sed -i \
  -e 's#__SSL_CERTIFICATE__#/etc/nginx/ssl/lubobao.xiaofany.com/fullchain.pem#' \
  -e 's#__SSL_CERTIFICATE_KEY__#/etc/nginx/ssl/lubobao.xiaofany.com/key.pem#' \
  -e 's#__ACME_WEBROOT__#/var/www/acme#' \
  /etc/nginx/conf.d/lubobao.xiaofany.com.conf
sudo nginx -t
sudo systemctl reload nginx
```

配置要点：

- HTTP 一律 301 到 HTTPS，只保留 ACME 验证路径；
- HTML、`install.sh`、`install-cli.sh` 为 `Cache-Control: no-cache`，两个脚本以 `text/plain; charset=utf-8` 返回；其他静态资源缓存 1 小时到 1 天；
- 开启 gzip，带 HSTS、`nosniff`、`X-Frame-Options`、`Referrer-Policy` 和 CSP（页面只加载本站资源，只允许请求 `https://xiaofany.com` 的版本接口）；
- 隐藏文件（包括 rsync 的临时目录）返回 404。

nginx 版本低于 1.25.1 时没有 `http2 on;`，删掉这一行，把 `listen 443 ssl;` 改成 `listen 443 ssl http2;`。

## 5. known_hosts

`site-setup.sh` 最后打印了几行形如下面的内容，直接从服务器的 `/etc/ssh/ssh_host_*_key.pub` 读出，比在别处 `ssh-keyscan` 可靠：

```
你的服务器 ssh-ed25519 AAAAC3Nza…
你的服务器 ecdsa-sha2-nistp256 AAAAE2Vj…
```

**每行开头的主机名必须与 `SITE_DEPLOY_HOST` 完全一致**：IP 对 IP、域名对域名；端口不是 22 时写成 `[你的服务器]:2222`。xiaofany.com 的部署曾因为这一点失败过。保存成文件 `site_known_hosts`。

也可以在自己的电脑上核对：先 `ssh-keyscan -t ed25519 你的服务器`，比较指纹与服务器上 `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` 的输出一致。

## 6. GitHub Secret

在自己的电脑上，在仓库目录里执行（需要已登录 `gh`）：

```sh
gh secret set SITE_DEPLOY_HOST --body "你的服务器"
gh secret set SITE_DEPLOY_USER --body "lubobao-site"
gh secret set SITE_DEPLOY_SSH_KEY < lubobao-site-deploy
gh secret set SITE_DEPLOY_KNOWN_HOSTS < site_known_hosts
# 只有 SSH 端口不是 22 时：
gh variable set SITE_DEPLOY_PORT --body 2222
```

在 Windows 上生成或复制的值可能带 `\r`，workflow 写入前会去掉。设置完成后可以删掉本地的私钥文件（换密钥时重新生成，并在服务器上再执行一次 `site-setup.sh`）。

官网地址不是 `https://lubobao.xiaofany.com` 时，设变量 `SITE_URL`，部署后的检查会用它。

## 7. 第一次部署

在 GitHub 仓库的 Actions 页面选「Deploy website」→「Run workflow」，或者：

```sh
gh workflow run deploy-site.yml --ref main
gh run watch
```

workflow 会依次核对私钥能否读取、known_hosts 里有没有与 `SITE_DEPLOY_HOST` 对应的记录（不对时直接指出是哪一项），然后：

```sh
rsync -rlz --checksum --delete --delay-updates --chmod=D755,F644 -p --exclude='.*' website/ lubobao-site@你的服务器:./
```

（`./` 由 rrsync 映射到 `/var/www/lubobao`。）最后从外网请求首页、`install.sh` 和 `install-cli.sh`，确认内容和 Content-Type。

部署成功后，在浏览器里打开 https://lubobao.xiaofany.com ，确认：

- 页脚有备案号；
- 下载区：lubobao 产品在 xiaofany.com 后台创建并发布之前显示「即将发布」和 GitHub Releases 链接；发布后显示版本、日期、大小和 SHA-256；
- `curl -fsSI https://lubobao.xiaofany.com/install.sh` 返回 `Content-Type: text/plain; charset=utf-8`。

## 出问题时

| 现象 | 原因 |
| --- | --- |
| Actions 里「跳过官网部署」 | 四个 Secret 有没配置的，提示里列出了名字 |
| `SITE_DEPLOY_SSH_KEY 不是可用的私钥` | 粘贴不完整、带了密码，或者粘的是公钥 |
| `SITE_DEPLOY_KNOWN_HOSTS 里没有与 SITE_DEPLOY_HOST 对应的记录` | 主机名不一致（见第 5 节） |
| `Permission denied (publickey)` | 公钥没写进服务器、sshd 的 `AllowUsers` 没有 `lubobao-site`，或者用户 shell 不是 `/bin/sh` |
| rsync 报 `rrsync` 相关错误 | 服务器上 rrsync 缺失或缺 python3；再执行一次 `site-setup.sh` |
| 首页 403 | `/var/www/lubobao` 权限不对，或 SELinux 拦截（RHEL 系执行 `restorecon -R /var/www/lubobao`） |
