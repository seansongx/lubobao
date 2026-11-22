# Repository Guidelines

## StreamCap 重构规则

- 以 [StreamCap](https://github.com/ihmily/StreamCap) 为主要行为参考，本机参考目录为 `C:\Users\soch\Desktop\Code\StreamCap`（与本仓库同级）。参考目录只读，构建和运行录播宝应独立于该路径。目标是在 Tauri 2 桌面端重新实现全部界面和用户可见功能，网站继续独立维护。
- 每批功能先记录参考版本或快照、页面入口、默认值、操作结果及异常行为。以 `app/ui/views`、`app/core`、`app/lifecycle`、`app/messages` 和实际使用的 StreamGet 平台能力为起点，持续维护覆盖清单；逐项标注待实现、已实现待验证、已验证及有意差异。分批交付，保留后续平台和功能的范围。
- 界面按桌面操作重新设计。React 展示任务和收集操作，Rust 持有任务状态、调度、平台解析、文件写入和进程生命周期。前端通过 Tauri commands 获取快照和提交操作，通过事件接收状态变化；切换页面或隐藏到托盘后，录制应继续。
- 桌面前端采用 React、TypeScript、Vite、shadcn/ui 和 Tailwind CSS。按页面需要引入组件，筛选条件、选中项和表单草稿优先使用 React 本地状态。任务表需要排序、筛选和批量选择时接入 TanStack Table。
- 桌面主题使用 shadcn/ui 的 Nova 样式、Slate 基础色和 Teal 主色，明暗模式默认跟随系统，圆角 `0.375rem`。窗口、侧栏和表格使用 Slate 灰阶；深色模式的背景、卡片、侧栏和边框改用中性石墨色，避免整体偏蓝。主按钮、选中项和焦点环使用 Teal（浅色 teal-700、深色 teal-400），不用红色或琥珀色，以免与录制状态混淆。界面结构以 `docs/prototypes/redesign.html` 为准：左侧导航（直播间、录像库、动态、设置），底部状态栏常驻显示 FFmpeg、录制数、码率、磁盘余量和下次检测，任务表选中后在右侧显示详情。
- 任务列表行高约 `44px`。录制中用带呼吸动画的红色实心点、检测中用蓝色旋转圆环、等待开播用灰色空心圈、重试中用琥珀色、仅通知用 Teal 铃铛、需处理用红色警告图标加淡红行底、已暂停整行降低透明度；各状态均显示文字。
- 后端按功能引入 Tokio、reqwest 和 rusqlite，沿用 serde。平台适配器统一输出直播状态、画质和流信息，参考 StreamGet 的平台行为，逐个平台验证。
- FFmpeg 独立于安装包分发。启动时由 Rust 检查应用本地数据目录中的已验证版本，缺失时在后台按系统和架构自动下载。发布时固定版本、下载地址和 SHA-256，优先国内源，备用源须提供相同文件。分发包使用公用 npm 包 `@seankit/ffmpeg`：读取其 `manifest.json` 中当前平台包的地址（npmmirror 为国内源、npmjs 为备用源）和 SHA-256，tarball 内可执行文件位于 `package/bin/`。该包在独立仓库 `C:\Users\soch\Desktop\Code\ffmpeg-npm` 维护。先下载到临时目录，校验通过后原子安装，再按绝对路径启动。已有可用版本时离线复用；下载期间可使用界面，失败时提供重试并保留可用版本。
- 验收以可观察结果为准。录制功能至少验证未开播、开始录制、断流、手动停止和生成文件可播放；持久化功能验证重启恢复。使用本地样本做可重复测试，平台兼容性另做真实直播间验证。依赖随首个使用场景引入。

## Project Structure

LuBoBao (录播宝) is a Tauri 2 desktop app for live-stream recording. The current scaffold demonstrates the TypeScript-to-Rust connection with a greeting form. Stream monitoring and recording are planned features.

- Edit the form markup in `index.html`, DOM event handlers in `src/main.ts`, and styles in `src/styles.css`. Frontend images live in `src/assets/`.
- Keep the public homepage and its assets in `website/`. This directory is the Nginx document root and can be deployed directly.
- Implement native commands in `src-tauri/src/lib.rs` and register them with `tauri::generate_handler!`. The desktop entry point is `src-tauri/src/main.rs`.
- Configure the window and installer in `src-tauri/tauri.conf.json`. Application icons live in `src-tauri/icons/`.

## Development and Build Commands

Run commands from the repository root. Install Node.js, npm, and stable Rust. On Windows, install WebView2 Runtime and Microsoft C++ Build Tools with the desktop C++ workload and Windows SDK.

- `npm ci` installs the dependencies pinned in `package-lock.json`.
- `npm run dev` starts Vite for browser-based layout work.
- `npm run tauri dev` starts Vite and opens the desktop app with native commands available.
- `npm run build` checks TypeScript and builds the frontend into `dist/`.
- `npm run tauri build` builds the desktop app and writes installers to `src-tauri/target/release/bundle/`.

## Coding Style

桌面 CSS token 统一维护在 `src/tokens.css`。通用颜色沿用 shadcn/ui 语义名称，任务状态使用 `--status-recording`、`--status-waiting`、`--status-checking`、`--status-retrying`、`--status-notify`，需处理行底色使用 `--status-recording-soft`，任务行高使用 `--task-row-height`。接入 Tailwind 时将主题映射到同一组变量。

Use two spaces in TypeScript, HTML, and CSS. Follow the existing camelCase TypeScript names and kebab-case HTML IDs, such as `greetInputEl` and `greet-input`. Strict TypeScript checks include unused locals and parameters. Use four spaces and snake_case function names in Rust.

## Testing

The project has no automated test cases, frontend test runner, or coverage threshold yet. Place new Rust unit tests beside the implementation in a `#[cfg(test)]` module, name each test after the behavior it checks, and run `cargo test --manifest-path src-tauri/Cargo.toml`. To check the desktop connection manually, launch the app, enter a name, click **Greet**, and verify the response includes that name.

## Commits and Pull Requests

Git history is unavailable in this workspace. Use a short imperative commit subject such as `Fix greeting form submission`. In a pull request, describe the behavior change, link any relevant issue, and list the checks performed. Include a screenshot for visible changes.
