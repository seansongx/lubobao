# 录播宝 LuBoBao

录播宝是一款从零开发的多平台直播录制桌面应用，目标是监控抖音、快手等平台的直播间，在主播开播后自动录制并保存到本地。

项目处于初始化阶段，当前包含 Tauri 2 桌面工程和 TypeScript 示例界面。直播间解析、开播监控与录制功能待实现。

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

- `src/` 包含前端 TypeScript 代码和样式。
- `src-tauri/src/` 包含 Rust 桌面端入口。
- `src-tauri/tauri.conf.json` 配置应用标识、窗口和打包选项。
