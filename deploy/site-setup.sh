#!/bin/sh
# 在官网服务器上准备录播宝官网的部署用户。以 root 执行，可以重复执行。
#
#   sh site-setup.sh <部署公钥或 .pub 文件> [部署时连接的主机名或 IP] [SSH 端口]
#
# 做的事：
#   1. 安装 rsync（以及 rrsync 需要的 python3），找到 rrsync；
#   2. 创建系统用户 lubobao-site 和网站目录 /var/www/lubobao；
#   3. 写入只允许 rrsync 同步这个目录的 authorized_keys：
#        command="<rrsync> /var/www/lubobao",restrict <公钥>
#      这把钥匙不能登录 shell、不能转发端口，也不能读写网站目录以外的文件；
#   4. 打印 GitHub Secret 要用的值和后续步骤。
# 详见仓库 docs/website-deploy.md。

set -eu

SITE_USER=lubobao-site
SITE_HOME=/var/lib/lubobao-site
SITE_DIR=/var/www/lubobao

main() {
    [ "$(id -u)" -eq 0 ] || die "请用 root 执行：sudo sh $0 <部署公钥>"
    [ $# -ge 1 ] || usage
    key_arg=$1
    host=${2:-}
    port=${3:-22}

    case $port in
        "" | *[!0-9]*) die "SSH 端口无效：$port" ;;
    esac

    if [ -f "$key_arg" ]; then
        pubkey=$(grep -v '^[[:space:]]*\(#\|$\)' "$key_arg" | tr -d '\r')
    else
        pubkey=$(printf '%s' "$key_arg" | tr -d '\r')
    fi
    [ "$(printf '%s\n' "$pubkey" | wc -l)" -eq 1 ] || die "只能传入一把公钥（一行）。"
    case $pubkey in
        ssh-ed25519\ * | ssh-rsa\ * | ecdsa-sha2-nistp256\ * | ecdsa-sha2-nistp384\ * | ecdsa-sha2-nistp521\ * | sk-ssh-ed25519@openssh.com\ *) ;;
        *) die "看起来不是 OpenSSH 公钥（应以 ssh-ed25519 等开头，不要带 command= 之类的选项）。" ;;
    esac
    case $pubkey in
        *'"'* | *\\*) die "公钥里不应有引号或反斜杠。" ;;
    esac

    install_packages
    rrsync=$(find_rrsync)
    say "rrsync：$rrsync"

    # 用户：shell 必须是 /bin/sh，sshd 通过它执行 authorized_keys 里的 command；
    # 密码字段设为 *（不能用密码登录，但不算「已锁定」，公钥登录可用）。
    if id "$SITE_USER" >/dev/null 2>&1; then
        say "用户 $SITE_USER 已存在。"
    else
        useradd --system --home-dir "$SITE_HOME" --no-create-home --shell /bin/sh "$SITE_USER"
        say "已创建用户 ${SITE_USER}。"
    fi
    usermod --shell /bin/sh --home "$SITE_HOME" "$SITE_USER"
    usermod -p '*' "$SITE_USER"
    group=$(id -gn "$SITE_USER")

    # 家目录和 authorized_keys 归 root 所有：部署用户自己不能改动这把钥匙的限制。
    install -d -o root -g root -m 755 "$SITE_HOME"
    install -d -o root -g root -m 755 "$SITE_HOME/.ssh"
    tmpkeys=$(mktemp "$SITE_HOME/.ssh/authorized_keys.XXXXXX")
    {
        echo "# 录播宝官网部署（GitHub Actions deploy-site.yml），由 deploy/site-setup.sh 生成"
        printf 'command="%s %s",restrict %s\n' "$rrsync" "$SITE_DIR" "$pubkey"
    } > "$tmpkeys"
    chown root:root "$tmpkeys"
    chmod 644 "$tmpkeys"
    mv -f "$tmpkeys" "$SITE_HOME/.ssh/authorized_keys"
    say "已写入 $SITE_HOME/.ssh/authorized_keys。"

    # 网站目录归部署用户，nginx 只需要读取权限。
    install -d -m 755 "$(dirname "$SITE_DIR")"
    install -d -o "$SITE_USER" -g "$group" -m 755 "$SITE_DIR"
    say "网站目录：$SITE_DIR"

    if command -v restorecon >/dev/null 2>&1; then
        restorecon -R "$SITE_HOME" "$SITE_DIR" 2>/dev/null || true
    fi

    print_next_steps
}

usage() {
    printf '用法：sh %s <部署公钥或 .pub 文件> [部署时连接的主机名或 IP] [SSH 端口]\n' "$0" >&2
    exit 2
}

say() {
    printf '%s\n' "$*"
}

die() {
    printf '错误：%s\n' "$*" >&2
    exit 1
}

install_packages() {
    need=""
    command -v rsync >/dev/null 2>&1 || need="$need rsync"
    command -v python3 >/dev/null 2>&1 || need="$need python3"
    [ -n "$need" ] || return 0
    say "安装：$need"
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update
        # shellcheck disable=SC2086
        DEBIAN_FRONTEND=noninteractive apt-get install -y $need
    elif command -v dnf >/dev/null 2>&1; then
        # shellcheck disable=SC2086
        dnf install -y $need
    elif command -v yum >/dev/null 2>&1; then
        # shellcheck disable=SC2086
        yum install -y $need
    else
        die "请先手动安装：$need"
    fi
}

# rsync 3.2.3 起 Debian、Ubuntu 的 rsync 包自带 /usr/bin/rrsync；更早的版本只在文档目录里有一份。
find_rrsync() {
    if command -v rrsync >/dev/null 2>&1; then
        command -v rrsync
        return 0
    fi
    for f in /usr/share/doc/rsync/scripts/rrsync /usr/share/rsync/scripts/rrsync /usr/share/doc/rsync*/support/rrsync; do
        if [ -f "$f" ]; then
            install -m 755 "$f" /usr/local/bin/rrsync
            echo /usr/local/bin/rrsync
            return 0
        fi
    done
    for f in /usr/share/doc/rsync/scripts/rrsync.gz /usr/share/doc/rsync*/support/rrsync.gz; do
        if [ -f "$f" ]; then
            gzip -dc "$f" > /usr/local/bin/rrsync
            chmod 755 /usr/local/bin/rrsync
            echo /usr/local/bin/rrsync
            return 0
        fi
    done
    die "找不到 rrsync。请安装 3.2.3 以后的 rsync，或从 rsync 源码包的 support/rrsync 复制到 /usr/local/bin/rrsync 后重新执行。"
}

print_next_steps() {
    say ""
    say "========== 接下来 =========="
    say ""
    say "1. 在 GitHub 仓库里设置 Secret（在你自己的电脑上执行，见 docs/website-deploy.md）："
    say "     SITE_DEPLOY_USER         $SITE_USER"
    say "     SITE_DEPLOY_HOST         ${host:-<部署时连接的主机名或 IP>}"
    say "     SITE_DEPLOY_SSH_KEY      部署私钥文件的完整内容"
    say "     SITE_DEPLOY_KNOWN_HOSTS  下面这几行（每行开头的主机名必须与 SITE_DEPLOY_HOST 完全一致）"
    if [ "$port" != 22 ]; then
        say "   SSH 端口不是 22，另设变量：gh variable set SITE_DEPLOY_PORT --body $port"
    fi
    say ""
    if [ "$port" = 22 ]; then
        known_host=${host:-<主机>}
    else
        known_host="[${host:-<主机>}]:$port"
    fi
    for f in /etc/ssh/ssh_host_ed25519_key.pub /etc/ssh/ssh_host_ecdsa_key.pub /etc/ssh/ssh_host_rsa_key.pub; do
        [ -f "$f" ] || continue
        printf '%s %s\n' "$known_host" "$(cut -d' ' -f1,2 "$f")"
    done
    say ""
    say "   这些主机公钥是直接从本机 /etc/ssh 读出的，比在别处 ssh-keyscan 更可靠。"
    say ""
    say "2. 确认 sshd 允许 $SITE_USER 登录：如果 /etc/ssh/sshd_config 里有 AllowUsers 或 AllowGroups，"
    say "   把 $SITE_USER 加进去，然后 sshd -t && systemctl reload ssh（或 sshd）。"
    say ""
    say "3. 放好 nginx 配置 deploy/nginx/lubobao.xiaofany.com.conf，nginx -t 后 reload。"
    say ""
    say "4. 在 GitHub Actions 里手动运行一次「Deploy website」。"
}

main "$@"
