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

LuBoBao (录播宝) is a Tauri 2 desktop app for live-stream recording, with a headless CLI (`lubobao-cli`) and Docker image built on the same core. The 0.1.0 launch scope and acceptance status live in `docs/feature-map.md`.

- The Rust code is a Cargo workspace (root `Cargo.toml` and `Cargo.lock`, build output in the root `target/`). The version lives in `[workspace.package]`; members inherit it.
  - `crates/lubobao-core` (library): `platforms/` (adapters that resolve a room link into live status, qualities and stream URLs), `recorder/` (FFmpeg process lifecycle, MP4 post-processing), `ffmpeg/` (download, verify and install FFmpeg from `@seankit/ffmpeg`), `tasks/` (task model, SQLite storage, activity, settings), `scheduler/` (polling, concurrency, backoff, shutdown) and `library/`. It must not depend on Tauri or any GUI crate. The `e2e` feature adds a test-only adapter for `e2e+http://…` links; release builds never enable it.
  - `crates/lubobao-cli` (binary `lubobao-cli`): the headless CLI. `run` holds the data-dir lock and serves a local control channel (Unix socket `control.sock` with mode 0600, or a named pipe on Windows); other subcommands go through it, or open the database directly when `run` is not running. See `docs/cli.md`.
  - `src-tauri` (desktop app, depends on `lubobao-core`): `ffmpeg.rs`, `tasks.rs`, `scheduler.rs` and `library.rs` re-export the core modules and add the Tauri glue (`tauri_glue.rs`, `commands.rs`); `desktop/` holds the tray, quit confirmation and notifications. Put `tauri::` code, commands and event emission only here.

- `index.html` only mounts `#root`. `src/main.tsx` renders `src/App.tsx`, which holds the current page in React local state; there is no router.
- `src/components/` holds app components such as `app-sidebar.tsx` and `status-bar.tsx`. shadcn/ui components live in `src/components/ui/`; add them with `npx shadcn@latest add <name>` as pages need them (`components.json` uses the Nova style). Shared helpers such as `cn` live in `src/lib/`.
- `src/pages/pages.ts` lists the navigation pages; page components live in `src/pages/`.
- `src/tokens.css` defines the theme variables. `src/index.css` imports Tailwind CSS v4 and maps those variables into the Tailwind theme with `@theme inline`; light and dark follow `prefers-color-scheme`. Import with the `@/` alias, which points to `src/`.
- Keep the public homepage and its assets in `website/`. This directory is the Nginx document root and can be deployed directly.
- Implement native commands in `src-tauri/src/lib.rs` and register them with `tauri::generate_handler!`. The desktop entry point is `src-tauri/src/main.rs`.
- Configure the window and installer in `src-tauri/tauri.conf.json`. The window defaults to 1280×800 with a 1024×680 minimum. Application icons live in `src-tauri/icons/`.

## Development and Build Commands

Run commands from the repository root. Install Node.js, npm, and stable Rust. On Windows, install WebView2 Runtime and Microsoft C++ Build Tools with the desktop C++ workload and Windows SDK.

- `npm ci` installs the dependencies pinned in `package-lock.json`.
- `npm run dev` starts Vite at `http://127.0.0.1:1420` for browser-based layout work. The dev server binds to IPv4 loopback because `localhost` can resolve to an unreachable `::1`, which leaves `tauri dev` waiting for the frontend.
- `npm run tauri dev` starts Vite and opens the desktop app with native commands available.
- `npm run build` type-checks with `tsc -b` and builds the frontend into `dist/`.
- `cargo check --workspace` checks the Rust side without building the app. `default-members` excludes `src-tauri`, so a plain `cargo build` or `cargo test` at the root only builds `lubobao-core` and `lubobao-cli` (no GTK/WebKit needed); pass `--workspace` or `-p lubobao` for the desktop crate. The workspace excludes `.claude/` so session worktrees keep their own layout.
- `cargo run -p lubobao-cli -- run` starts the CLI in the foreground; use `--data-dir` to keep test data apart. CLI release builds use `--profile release-cli` (like `release` but `panic = "unwind"`); output goes to `target/<triple>/release-cli/`.
- `npm run tauri build` builds the desktop app and writes installers to `target/release/bundle/`.

## Coding Style

桌面 CSS token 统一维护在 `src/tokens.css`。通用颜色沿用 shadcn/ui 语义名称，任务状态使用 `--status-recording`、`--status-waiting`、`--status-checking`、`--status-retrying`、`--status-notify`，需处理行底色使用 `--status-recording-soft`，任务行高使用 `--task-row-height`。接入 Tailwind 时将主题映射到同一组变量。

Use two spaces in TypeScript, HTML, and CSS. Name React components in PascalCase and other TypeScript identifiers in camelCase; use kebab-case file names, such as `AppSidebar` in `src/components/app-sidebar.tsx`. Strict TypeScript checks include unused locals and parameters. Use four spaces and snake_case function names in Rust.

## Testing

Rust tests live beside the implementation in `#[cfg(test)]` modules, plus offline fixtures and `#[ignore]` live-network tests under `crates/lubobao-core/tests/`. Name each test after the behavior it checks. There is no frontend test runner yet. Before committing, run `npm run build`, then `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` from the repository root. Set `LUBOBAO_TEST_FFMPEG` to an ffmpeg binary to run the recorder tests; without it they are skipped. Run live tests with `cargo test -p lubobao-core -- --ignored --nocapture` when a change touches a platform adapter. The CLI end-to-end test (`run`, `add`, recording, `stop`, stream end, SIGTERM) needs `LUBOBAO_TEST_FFMPEG` and runs with `cargo test -p lubobao-cli --features e2e --test e2e -- --nocapture`.

To check the shell manually, run `npm run dev` and open `http://127.0.0.1:1420`. Switch between the four pages, check the sidebar, content area, and status bar in both light and dark system themes, and confirm that a 1024px-wide window has no horizontal scrollbar.

## CI and Releases

- `.github/workflows/test.yml` runs on every push and pull request on Windows, macOS (arm64), and Ubuntu: `npm run build`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and the CLI end-to-end test. It installs FFmpeg from `@seankit/ffmpeg@9.0.1` and sets `LUBOBAO_TEST_FFMPEG`, so the recorder tests run too; `#[ignore]` network tests are skipped. Run the same three cargo commands locally before pushing.
- `.github/workflows/cli.yml` runs on every branch push and pull request: it builds static musl `lubobao-cli` for x64 (`ubuntu-24.04`) and arm64 (`ubuntu-24.04-arm`), checks the release binary has no test adapter, installs FFmpeg with the CLI itself, runs the end-to-end test, packages `lubobao-cli_<version>_linux_<arch>.tar.gz`, then builds the Docker image per architecture and runs the end-to-end test and a `docker stop` check inside it. Images are only pushed when `release.yml` calls it for a `v*` tag, after the build job's version check, and only as `<version>`; `docker-latest.yml` adds `latest` to that image when the Release is published by hand (skipped for pre-releases). The image is based on `gcr.io/distroless/cc-debian12` (the bundled BtbN FFmpeg needs glibc ≥ 2.28); base images are pinned by digest. Pin all actions to commit SHAs.
- `.github/workflows/release.yml` builds installers without code signing: Windows NSIS (x64), separate macOS dmg for arm64 and x64 (ad-hoc signed), and Linux AppImage and deb built on Ubuntu 22.04. It also calls `cli.yml` for the Linux CLI archives and the Docker image (`ghcr.io/seansongx/lubobao`, multi-arch, pushed as `<version>` only for `v*` tags). Only `v*` tag builds run in the GitHub Environment `release` (deployment rule: `v*` tags only), which holds the Secrets `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`; with them and the real `plugins.updater.pubkey` in `tauri.conf.json` it also builds the in-app update artifacts: `-setup.exe.sig`, macOS `.app.tar.gz` + `.sig` and `.AppImage.sig`, each checked against the configured public key by `scripts/verify-updater-signature.mjs`. Manual runs and forks never sign (warning, no update artifacts); a `v*` tag without the key or with the placeholder public key fails. The build job only has `contents: read`. Files are renamed to ASCII (`lubobao_<version>_<os>_<arch>…`, macOS update packages `lubobao_<version>_macos_<arch>.app.tar.gz`), uploaded as Actions artifacts, and the run summary lists each file's size and SHA-256. Manual runs (`workflow_dispatch`) only build; a `v*` tag also creates a draft GitHub Release with every file, `SHA256SUMS.txt`, an「更新内容」placeholder (the part above `<!-- 以上为应用内更新说明 -->` becomes the in-app update notes) and first-launch notes.
- `.github/workflows/publish-xiaofany.yml` runs when a Release is published (or by hand with a tag; drafts are refused): it downloads the Release files, checks `SHA256SUMS.txt`, builds a manifest with `scripts/xiaofany-manifest.mjs` (platform, arch, format, kind, signature and the GitHub download URL as fallback for every file) and publishes with `scripts/publish-xiaofany.mjs`, then checks the xiaofany.com version API and the Tauri update API. It needs the Secret `RELEASE_TOKEN` and skips with a notice without it. `scripts/publish-xiaofany.mjs` is a verbatim copy of `scripts/release.mjs` from the xiaofany.com repository (commit noted in its header); re-copy it instead of editing it.
- The desktop app updates itself with `tauri-plugin-updater` from `https://xiaofany.com/api/update/lubobao/tauri/{{target}}/{{arch}}/{{current_version}}` (`src-tauri/src/desktop/update.rs`, `src/components/update-dialog.tsx`). It sends a random `X-Client-Id` stored in the app data directory, requires the signature to match the announced version (`requireSignedVersion`), and finishes recordings like a quit before installing. It checks about 10 seconds after start and then every 24 hours when `auto_check_update` is on (desktop only; the CLI ignores this key). If installing fails after recordings were finished, the only way out is `restart_after_failed_update`. Debug builds accept `LUBOBAO_UPDATER_ENDPOINT` to point at a local test server. Linux deb installs only get a link to the website. The CLI does not update itself.
- `.github/workflows/deploy-site.yml` rsyncs `website/` to the official site (`lubobao.xiaofany.com`) on pushes to main that touch `website/**` or by manual run, through the restricted `lubobao-site` user (rrsync); it skips with a notice when the `SITE_DEPLOY_*` secrets are missing. Server setup is in `docs/website-deploy.md`, `deploy/site-setup.sh` and `deploy/nginx/`. `.github/workflows/install-scripts.yml` tests `website/install.sh` (macOS) and `website/install-cli.sh` (Linux) against a local fake `/asset` server, runs shellcheck, and checks the rrsync deploy path.
- Release steps (details, one-time setup and rollback in `docs/release.md`): `node scripts/set-version.mjs <x.y.z>` (updates `package.json`, `package-lock.json`, `tauri.conf.json`, the root `Cargo.toml` `[workspace.package]`, and the workspace crates in `Cargo.lock`; `node scripts/check-version.mjs` verifies them), commit, then `git tag v<x.y.z>` and push the tag. The workflow fails if the tag differs from the app version. Review the draft release by hand, replace the「更新内容」placeholder with `docs/release-notes/<x.y.z>.md` (written and committed with the version bump) and publish it; publishing uploads to xiaofany.com and tags the Docker image `latest`.
- A local `npm run tauri build` needs the update signing key; without it pass `--config '{"bundle":{"createUpdaterArtifacts":false}}'`.
- The icon source is `docs/brand/app-icon.svg` (full-bleed, for Windows and Linux) and `docs/brand/app-icon-macos.svg` (on the macOS icon grid). Regenerate with `npx tauri icon docs/brand/app-icon.png`, then replace `src-tauri/icons/icon.icns` with the one generated from `app-icon-macos.png`, and delete the `android/` and `ios/` folders.

## Commits and Pull Requests

Git history is unavailable in this workspace. Use a short imperative commit subject such as `Fix greeting form submission`. In a pull request, describe the behavior change, link any relevant issue, and list the checks performed. Include a screenshot for visible changes.
