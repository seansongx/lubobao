#!/bin/sh
# 录播宝 macOS 一键安装脚本
#
#   curl -fsSL https://lubobao.xiaofany.com/install.sh | sh
#
# 按芯片下载对应的 dmg，校验 SHA-256，把 app 复制到 /Applications，再去掉隔离标记。
# 可选环境变量：
#   LUBOBAO_VERSION       安装指定版本，例如 0.1.0（默认最新版）
#   LUBOBAO_INSTALL_DIR   安装目录（默认 /Applications，没有权限时可改为 "$HOME/Applications"）
#   LUBOBAO_API_BASE      分发平台地址（默认 https://xiaofany.com，仅用于测试）
#
# 整个脚本包在 main 函数里、最后一行才调用：下载到一半断开时不会执行半截脚本。

set -eu

main() {
    api_base=${LUBOBAO_API_BASE:-https://xiaofany.com}
    api_base=${api_base%/}
    install_dir=${LUBOBAO_INSTALL_DIR:-/Applications}
    install_dir=${install_dir%/}
    [ -n "$install_dir" ] || install_dir=/
    version=${LUBOBAO_VERSION:-}
    version=${version#v}
    script_url="https://lubobao.xiaofany.com/install.sh"

    tmp=""
    mnt=""
    mounted=""
    staging=""
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 129' HUP
    trap 'exit 143' TERM

    [ "$(uname -s)" = Darwin ] || die "这个脚本只用于 macOS。Windows 和 Linux 请到 https://lubobao.xiaofany.com/#download 下载。"
    for cmd in curl shasum hdiutil ditto xattr sw_vers sysctl; do
        command -v "$cmd" >/dev/null 2>&1 || die "缺少命令 ${cmd}。"
    done

    check_macos_version

    # 不用 uname -m：终端在 Rosetta 下运行时会报成 x86_64。
    if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
        arch=arm64
        arch_label="Apple 芯片"
    else
        arch=x64
        arch_label="Intel 芯片"
    fi

    query="platform=mac&arch=$arch&format=dmg"
    if [ -n "$version" ]; then
        case $version in
            *[!0-9A-Za-z.+-]*) die "LUBOBAO_VERSION 不是有效的版本号：$version" ;;
        esac
        query="$query&version=$version"
    fi

    say "正在查询录播宝${version:+ v$version}（macOS · ${arch_label}）……"
    fetch_info "$api_base/api/v1/products/lubobao/asset?$query"

    asset_version=$(info_get version)
    name=$(info_get name)
    sha256=$(info_get sha256 | tr 'A-F' 'a-f')
    url=$(info_get url)

    case $name in
        "" | *[!A-Za-z0-9._-]* | .*) die "分发平台返回的文件名无效：$name" ;;
        *.dmg) ;;
        *) die "分发平台返回的不是 dmg 文件：$name" ;;
    esac
    case $sha256 in
        *[!0-9a-f]*) die "分发平台返回的 SHA-256 无效。" ;;
    esac
    [ ${#sha256} -eq 64 ] || die "分发平台返回的 SHA-256 无效。"
    case $url in
        "$api_base"/*) ;;
        *) die "分发平台返回的下载地址不在 $api_base 下：$url" ;;
    esac

    tmp=$(mktemp -d "${TMPDIR:-/tmp}/lubobao-install.XXXXXX")
    dmg="$tmp/$name"

    say "正在下载 ${name}（v${asset_version}）……"
    curl -fL --retry 3 --connect-timeout 20 --progress-bar -o "$dmg" "$url" \
        || die "下载失败，请检查网络后重试。"

    actual=$(shasum -a 256 "$dmg" | awk '{print $1}')
    if [ "$actual" != "$sha256" ]; then
        die "SHA-256 校验失败，已中止安装，没有改动任何文件。
  期望：$sha256
  实际：$actual
文件可能在传输中损坏，请稍后重试。"
    fi
    say "SHA-256 校验通过。"

    mnt="$tmp/mnt"
    mkdir "$mnt"
    # stdin 指向 /dev/null：脚本通过管道运行时，hdiutil 不能读走后面的脚本内容。
    hdiutil attach -nobrowse -readonly -noautoopen -quiet -mountpoint "$mnt" "$dmg" </dev/null \
        || die "无法打开 dmg。"
    mounted=1

    app_src=""
    for candidate in "$mnt"/*.app; do
        if [ ! -d "$candidate" ] || [ -L "$candidate" ]; then
            continue
        fi
        [ -z "$app_src" ] || die "dmg 里有不止一个应用，已中止。"
        app_src=$candidate
    done
    [ -n "$app_src" ] || die "dmg 里没有找到应用。"
    app_name=${app_src##*/}
    dest="$install_dir/$app_name"

    wait_until_not_running "$app_name"
    check_writable

    staging="$install_dir/.$app_name.installing.$$"
    rm -rf "$staging"
    ditto "$app_src" "$staging" || die "复制应用失败。"
    xattr -dr com.apple.quarantine "$staging" 2>/dev/null || true
    if [ -e "$dest" ] || [ -L "$dest" ]; then
        rm -rf "$dest" || die "无法替换已安装的 ${dest}。"
    fi
    mv "$staging" "$dest" || die "无法安装到 ${dest}。"
    staging=""
    # 再执行一次，确保最终位置上没有隔离标记
    xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true

    hdiutil detach -quiet "$mnt" </dev/null || hdiutil detach -quiet -force "$mnt" </dev/null || true
    mounted=""

    say ""
    say "已安装录播宝 v$asset_version 到 $dest"
    say "可以在「启动台」或「应用程序」里打开录播宝。"
}

say() {
    printf '%s\n' "$*"
}

die() {
    printf '\n错误：%s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "${mounted:-}" ] && [ -n "${mnt:-}" ]; then
        hdiutil detach -quiet "$mnt" </dev/null 2>/dev/null \
            || hdiutil detach -quiet -force "$mnt" </dev/null 2>/dev/null || true
    fi
    if [ -n "${staging:-}" ]; then
        rm -rf "$staging"
    fi
    if [ -n "${tmp:-}" ] && [ -d "$tmp" ]; then
        rm -rf "$tmp"
    fi
}

check_macos_version() {
    product_version=$(sw_vers -productVersion)
    major=${product_version%%.*}
    rest=${product_version#*.}
    minor=${rest%%.*}
    [ "$rest" != "$product_version" ] || minor=0
    case $major$minor in
        *[!0-9]*) die "无法识别系统版本：$product_version" ;;
    esac
    if [ "$major" -lt 10 ] || { [ "$major" -eq 10 ] && [ "$minor" -lt 15 ]; }; then
        die "录播宝需要 macOS 10.15 或以后的版本，当前是 ${product_version}。"
    fi
}

# 请求纯文本的文件信息。找不到时显示分发平台的说明。
fetch_info() {
    response=$(curl -sSL --retry 3 --connect-timeout 20 -w '\n%{http_code}' "$1") \
        || die "无法连接分发平台，请检查网络后重试。"
    status=$(printf '%s\n' "$response" | tail -n 1)
    info=$(printf '%s\n' "$response" | sed '$d' | tr -d '\r')
    if [ "$status" = 404 ]; then
        die "没有找到可下载的文件${version:+（版本 ${version}）}。录播宝可能还没有发布这个版本。
${info:+分发平台说明：$info
}可以到 https://github.com/seansongx/lubobao/releases 查看。"
    fi
    [ "$status" = 200 ] || die "分发平台返回错误（HTTP ${status}），请稍后重试。"
}

info_get() {
    printf '%s\n' "$info" | sed -n "s/^$1=//p" | head -n 1
}

# 录播宝正在运行时请用户自己退出，不强制结束进程。
wait_until_not_running() {
    pattern="$1/Contents/MacOS/"
    pgrep -f "$pattern" >/dev/null 2>&1 || return 0
    say ""
    say "录播宝正在运行。请先退出录播宝（菜单栏「录播宝 → 退出」，或按 Cmd+Q；托盘里也可以选「退出」）。"
    say "有录制时退出会先确认，等文件写完再关闭。脚本会等待 2 分钟……"
    waited=0
    while pgrep -f "$pattern" >/dev/null 2>&1; do
        [ "$waited" -lt 120 ] || die "录播宝仍在运行，已中止安装。退出录播宝后重新运行安装命令即可。"
        sleep 2
        waited=$((waited + 2))
    done
    say "录播宝已退出，继续安装。"
}

check_writable() {
    if [ ! -d "$install_dir" ]; then
        if [ "$install_dir" = /Applications ]; then
            die "/Applications 不存在。"
        fi
        mkdir -p "$install_dir" 2>/dev/null || die "无法创建安装目录 ${install_dir}。"
    fi
    if [ ! -w "$install_dir" ] || { [ -e "$dest" ] && [ ! -w "$dest" ]; }; then
        die "没有写入 $install_dir 的权限（普通用户账户不能写「应用程序」文件夹）。可以：
  1. 用管理员账户登录后重新运行安装命令；或
  2. 装到你自己的「应用程序」文件夹：
     curl -fsSL $script_url | LUBOBAO_INSTALL_DIR=\"\$HOME/Applications\" sh"
    fi
}

main "$@"
