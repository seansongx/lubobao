#!/bin/sh
# 录播宝命令行（lubobao-cli）Linux 一键安装脚本
#
#   curl -fsSL https://lubobao.xiaofany.com/install-cli.sh | sh
#
# 按 CPU 架构下载 lubobao-cli 的 tar.gz，校验 SHA-256，安装到 /usr/local/bin。
# 可选环境变量：
#   LUBOBAO_VERSION       安装指定版本，例如 0.1.0（默认最新版）
#   LUBOBAO_INSTALL_DIR   安装目录（默认 /usr/local/bin，没有权限时可改为 "$HOME/.local/bin"）
#   LUBOBAO_API_BASE      分发平台地址（默认 https://xiaofany.com，仅用于测试）
#
# 整个脚本包在 main 函数里、最后一行才调用：下载到一半断开时不会执行半截脚本。

set -eu

main() {
    api_base=${LUBOBAO_API_BASE:-https://xiaofany.com}
    api_base=${api_base%/}
    install_dir=${LUBOBAO_INSTALL_DIR:-/usr/local/bin}
    install_dir=${install_dir%/}
    [ -n "$install_dir" ] || install_dir=/
    version=${LUBOBAO_VERSION:-}
    version=${version#v}
    script_url="https://lubobao.xiaofany.com/install-cli.sh"
    docs_url="https://github.com/seansongx/lubobao/blob/main/docs/cli.md"

    tmp=""
    staging=""
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 129' HUP
    trap 'exit 143' TERM

    [ "$(uname -s)" = Linux ] || die "这个脚本只用于 Linux。桌面版请到 https://lubobao.xiaofany.com/#download 下载。"
    for cmd in curl tar gzip uname mktemp; do
        command -v "$cmd" >/dev/null 2>&1 || die "缺少命令 $cmd，请先安装。"
    done
    if command -v sha256sum >/dev/null 2>&1; then
        sha_cmd="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        sha_cmd="shasum -a 256"
    else
        die "缺少命令 sha256sum（通常在 coreutils 包里），请先安装。"
    fi

    machine=$(uname -m)
    case $machine in
        x86_64 | amd64) arch=x64 ;;
        aarch64 | arm64) arch=arm64 ;;
        *) die "暂不支持这种 CPU 架构：$machine（只提供 x64 和 arm64）。" ;;
    esac

    query="platform=linux&arch=$arch&format=tar.gz"
    if [ -n "$version" ]; then
        case $version in
            *[!0-9A-Za-z.+-]*) die "LUBOBAO_VERSION 不是有效的版本号：$version" ;;
        esac
        query="$query&version=$version"
    fi

    # 先确认能写入安装目录，免得下载完才发现没有权限
    check_writable

    say "正在查询 lubobao-cli${version:+ v$version}（Linux · $arch）……"
    fetch_info "$api_base/api/v1/products/lubobao/asset?$query"

    asset_version=$(info_get version)
    name=$(info_get name)
    sha256=$(info_get sha256 | tr 'A-F' 'a-f')
    url=$(info_get url)

    case $name in
        "" | *[!A-Za-z0-9._-]* | .*) die "分发平台返回的文件名无效：$name" ;;
        *.tar.gz) ;;
        *) die "分发平台返回的不是 tar.gz 文件：$name" ;;
    esac
    case $sha256 in
        *[!0-9a-f]*) die "分发平台返回的 SHA-256 无效。" ;;
    esac
    [ ${#sha256} -eq 64 ] || die "分发平台返回的 SHA-256 无效。"
    case $url in
        "$api_base"/*) ;;
        *) die "分发平台返回的下载地址不在 $api_base 下：$url" ;;
    esac

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/lubobao-cli-install.XXXXXX")
    archive="$tmp/$name"

    say "正在下载 $name（v$asset_version）……"
    curl -fL --retry 3 --connect-timeout 20 --progress-bar -o "$archive" "$url" \
        || die "下载失败，请检查网络后重试。"

    actual=$($sha_cmd "$archive" | awk '{print $1}')
    if [ "$actual" != "$sha256" ]; then
        die "SHA-256 校验失败，已中止安装，没有改动任何文件。
  期望：$sha256
  实际：$actual
文件可能在传输中损坏，请稍后重试。"
    fi
    say "SHA-256 校验通过。"

    mkdir "$tmp/x"
    tar -xzf "$archive" -C "$tmp/x" || die "解压失败。"
    bin="$tmp/x/${name%.tar.gz}/lubobao-cli"
    if [ ! -f "$bin" ]; then
        bin=$(find "$tmp/x" -type f -name lubobao-cli | head -n 1)
    fi
    [ -n "$bin" ] && [ -f "$bin" ] || die "压缩包里没有找到 lubobao-cli。"

    dest="$install_dir/lubobao-cli"
    # 先复制到同一目录下的临时文件，再改名替换：正在运行的旧版本不受影响。
    staging="$install_dir/.lubobao-cli.installing.$$"
    cp "$bin" "$staging" || die "无法写入 $install_dir。"
    chmod 755 "$staging"
    mv -f "$staging" "$dest" || die "无法安装到 $dest。"
    staging=""

    installed=$("$dest" --version 2>/dev/null || true)
    say ""
    say "已安装 ${installed:-lubobao-cli v$asset_version} 到 $dest"

    case ":${PATH:-}:" in
        *":$install_dir:"*) cli=lubobao-cli ;;
        *)
            cli=$dest
            say "注意：$install_dir 不在 PATH 里，下面的命令请用完整路径，或把它加入 PATH。"
            ;;
    esac

    say ""
    say "下一步："
    say "  $cli run                                  # 前台运行，监控并自动录制"
    say "  $cli add https://live.douyin.com/123456   # 在另一个终端添加直播间"
    say "  $cli list"
    say ""
    say "长期运行建议用 systemd 服务，步骤见：$docs_url#systemd"
    if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet lubobao 2>/dev/null; then
        say "检测到 lubobao 服务正在运行，执行 sudo systemctl restart lubobao 让新版本生效（会先正常收尾正在进行的录制）。"
    fi
}

say() {
    printf '%s\n' "$*"
}

die() {
    printf '\n错误：%s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "${staging:-}" ]; then
        rm -f "$staging"
    fi
    if [ -n "${tmp:-}" ] && [ -d "$tmp" ]; then
        rm -rf "$tmp"
    fi
}

# 请求纯文本的文件信息。找不到时显示分发平台的说明。
fetch_info() {
    response=$(curl -sSL --retry 3 --connect-timeout 20 -w '\n%{http_code}' "$1") \
        || die "无法连接分发平台，请检查网络后重试。"
    status=$(printf '%s\n' "$response" | tail -n 1)
    info=$(printf '%s\n' "$response" | sed '$d' | tr -d '\r')
    if [ "$status" = 404 ]; then
        die "没有找到可下载的文件${version:+（版本 $version）}。lubobao-cli 可能还没有发布这个版本。
${info:+分发平台说明：$info
}可以到 https://github.com/seansongx/lubobao/releases 查看。"
    fi
    [ "$status" = 200 ] || die "分发平台返回错误（HTTP $status），请稍后重试。"
}

info_get() {
    printf '%s\n' "$info" | sed -n "s/^$1=//p" | head -n 1
}

check_writable() {
    if [ ! -d "$install_dir" ]; then
        mkdir -p "$install_dir" 2>/dev/null || true
    fi
    if [ -d "$install_dir" ] && [ -w "$install_dir" ]; then
        return 0
    fi
    die "没有写入 $install_dir 的权限。可以：
  1. 用 sudo 重新运行：
     curl -fsSL $script_url | sudo sh
  2. 或者装到自己的目录（不需要 root）：
     curl -fsSL $script_url | LUBOBAO_INSTALL_DIR=\"\$HOME/.local/bin\" sh"
}

main "$@"
