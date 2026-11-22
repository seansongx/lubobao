# 录播宝 LuBoBao

录播宝是一款从零开发的多平台直播录制桌面应用，目标是监控抖音、快手等平台的直播间，在主播开播后自动录制并保存到本地。

项目处于初始化阶段，当前包含 Tauri 2 桌面工程和 TypeScript 示例界面。直播间解析、开播监控与录制功能待实现。

各页面、录制流程、平台覆盖与分阶段验收条件见[功能地图](docs/feature-map.md)。

## 本地开发

开发环境需要 Node.js、npm 和 Rust 稳定版。Windows 还需要 Microsoft C++ Build Tools（含 C++ 桌面开发工作负载和 Windows SDK）以及 WebView2 Runtime。

在项目根目录执行以下命令，安装依赖并启动桌面应用。

```sh
npm install
npm run tauri dev
```

## 构建

仅构建前端可运行 `npm run build`。构建桌面应用和安装包可运行：

```sh
npm run tauri build
```

默认安装包输出目录为 `src-tauri/target/release/bundle/`。

## 目录结构

- `index.html` 和 `src/` 是桌面应用的前端入口、TypeScript 代码和样式。
- `src-tauri/src/` 包含 Rust 桌面端入口。
- `src-tauri/tauri.conf.json` 配置应用标识、窗口和打包选项。
- `website/` 是独立的官网目录，包含首页和同域加载的样式，可直接作为静态站点发布。

## 官网预览与 Nginx 部署

直接打开 `website/index.html` 即可预览。若本机已安装 Python，也可以在仓库根目录运行 `python -m http.server 8080 --bind 127.0.0.1 --directory website`，然后访问 `http://127.0.0.1:8080/`。

发布时，将 `website/` 的内容复制到官网子域名专用的站点目录，例如 `/var/www/lubobao-site/`。将子域名解析到服务器，在该子域名的 Nginx HTTPS 站点配置中设置以下两项，沿用服务器的证书管理方式：

```nginx
root /var/www/lubobao-site;
index index.html;
```

修改 Nginx 配置后，先执行 `nginx -t`，通过后再重载。以后更新首页只需同步静态文件。
