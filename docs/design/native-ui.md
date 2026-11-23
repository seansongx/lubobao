# 原生界面设计说明

录播宝的内容区三个平台统一（2026-11-23 起统一为原 macOS 的样子，见「已确定的取舍」第 5 条），窗口外壳和平台行为随平台：Windows 11 用 Mica 和系统标题栏，macOS 用 Overlay 标题栏和侧栏 vibrancy，Linux 用系统标题栏和不透明底色。三个平台的布局、页面、功能和快捷键含义一致，差异只在窗口材质和标题栏、字体族、滚动条和快捷键写法。交互原型见 `docs/prototypes/native.html`。

> **控件样式决定（2026-11-23）**：按钮、输入框、下拉、开关、复选框、对话框、菜单、表格、提示等控件一律用 shadcn/ui Nova 默认实现，`src/components/ui/` 不做修改，新增组件用 `npx shadcn@latest add`。原生感只通过组件以外的部分体现：token（`src/tokens.css` 设置 shadcn 的语义变量 `--radius`、`--background`、`--card`、`--popover`、`--muted`、`--border`、`--input`、`--sidebar` 等和字号，三个平台共用一组值，组件自动跟着变；平台块只改字体族）、页面布局（顶部 52px 工具栏，macOS 红绿灯与侧栏一体，Windows 对话框主按钮在左，带数字的筛选按钮，去掉侧栏标志等）、窗口外壳（Mica、vibrancy、窗口状态）和行为（右键菜单、快捷键、浏览器行为屏蔽、选区复制）。原型里「按钮层级」「对话框」「焦点环」等控件外形保留为当初的探索，实际以 Nova 默认样式为准；圆角三个平台都是 6px。唯一的例外是焦点样式（2026-11-23 起三个平台统一，聚焦时只变边框颜色）：Nova 默认是变色边框加 3px 半透明光晕，看起来过粗。`index.css` 覆盖 `:focus-visible` 时的 `--tw-ring-shadow`：文本框和下拉框去掉光晕，只保留 Teal 边框；按钮、开关、复选框、toggle、badge 本身可能就是 Teal，只改边框看不出焦点，保留只在键盘操作时出现的 2px 实色 Teal 细环。鼠标点击文本框时浏览器也会显示焦点边框（输入框的默认行为），按钮等只在键盘操作时显示。选择器只列这些控件，浮层（对话框、菜单、下拉列表）的外框和 aria-invalid 的红色效果不受影响。

不变的部分：

- 主色保留 Teal 品牌色（浅色 teal-700 `#0f766e`，深色 teal-400 `#2dd4bf`），不跟随系统强调色。只有主操作用 Teal 实心按钮，其余是次级按钮或文字按钮。
- 任务状态色和图标沿用 `AGENTS.md`：录制中红色呼吸实心点、检测中蓝色旋转圆环、等待开播灰色空心圈、重试中琥珀色、仅通知 Teal 铃铛、需处理红色警告图标、已暂停灰色文字，各状态都有文字；任务行不加状态底色或透明度。
- 任务表用 shadcn `Table` 默认行高，状态栏内容不变（FFmpeg、录制数、码率、磁盘余量、下次检测），任务表选中后右侧显示详情，1024 宽时没有横向滚动条。
- 布局：左侧导航（直播间、录像库、动态、设置）、右侧内容，底部状态栏常驻。

## 内容区（三个平台相同）

2026-11-23 起三个平台的内容区用同一组 token 和同一套页面布局，取原来 macOS 的值：

| 项 | 值 |
|---|---|
| 圆角 | `--radius` 6px（shadcn 刻度由它推出：`rounded-md` 4px、`rounded-lg` 6px、`rounded-xl` 10px） |
| 字号 | 正文、导航、设置、对话框 13px（`--text-ui`），任务表、详情、录像库、动态 13px（`--text-dense`），说明 11px（`--text-small`），页面标题 15px 粗体（`--text-title`、`--weight-title`）；行高 1.43 |
| 尺寸 | 导航项 28px（`--nav-item-h`），内容边距 20px（`--page-pad`），侧栏 208px（`--sidebar-width`），顶部工具栏 52px（`--header-h`），状态栏 24px |
| 配色（浅色） | 窗口 `#ececec`，侧栏 `#e8e8eb`，内容层 `#ffffff`，状态栏 `#f6f6f6`，浮层 `rgb(246 246 246 / .97)`，边框 `rgb(0 0 0 / .1)` |
| 配色（深色） | 窗口 `#2b2b2d`，侧栏 `#2a2a2d`，内容层 `#1e1e1e`，状态栏 `#252525`，浮层 `rgb(40 40 42 / .97)`，边框 `rgb(255 255 255 / .1)`（中性石墨，不偏蓝） |
| 工具栏 | 每页顶部 52px，下方一条分隔线；标题和摘要（数量、占用空间等）上下排列，右侧是搜索框和操作；「添加直播间」是无框「+」图标按钮。右侧详情的头部也是 52px，与工具栏对齐 |
| 侧栏 | 右侧一条分隔线；导航项图标为 Teal，当前项灰色底，没有选中竖条 |
| 列表与选中 | 任务表用 shadcn `Table` 默认的悬停和选中样式；录像库选中为淡 Teal 底；任务表、录像库和动态直接放在内容层上，不加卡片外框 |
| 设置页 | 每组是一个圆角分组表单（细边框、微填充底，行间分隔线） |

## 外壳与行为（随平台）

| | Windows | macOS | Linux |
|---|---|---|---|
| 窗口材质 | Windows 11：Mica，窗口底、侧栏和状态栏透明，内容层半透明（浅色 `rgb(255 255 255 / .5)`，深色 `rgb(58 58 58 / .3)`）；Windows 10：不透明，用上面的统一配色 | 侧栏 vibrancy（`sidebar` 材质），窗口底和侧栏透明；内容区不透明 | 不支持材质，不透明，用统一配色 |
| 标题栏 | 保留系统标题栏，Mica 延伸进标题栏 | `titleBarStyle: Overlay` + `hiddenTitle`；红绿灯落在侧栏顶部，侧栏顶部留出 52px，工具栏整条是拖拽区 | 保留系统标题栏（GNOME 下由 GTK/窗口管理器绘制） |
| 字体族 | `Segoe UI Variable Text`、`Segoe UI`、`Microsoft YaHei UI` | `-apple-system`（SF Pro）、`PingFang SC` | `Adwaita Sans`、`Cantarell`、`Noto Sans CJK SC`、`Ubuntu` |
| 对话框按钮 | 主按钮在左 | 主按钮在左 | 主按钮在左 |
| 滚动条 | 细线，悬停滚动区域时变粗 | 不定制，使用 WKWebView 原生悬浮滚动条 | 细线，悬停变粗，贴近 GTK 悬浮滚动条 |
| 快捷键写法 | Ctrl+N | ⌘N | Ctrl+N |

字号只统一大小，不统一字体族：Windows 上中文由微软雅黑 UI 显示，13px 正文、11px 说明比原来 Windows 的 14px / 12px 小一号。

系统开启「减少动态效果」（`prefers-reduced-motion`）时，录制点的呼吸、检测圆环的旋转和动态页新条目的入场动画改为静止（在调用处用 `motion-reduce:` 处理）；控件自带的动画由 Nova 组件自己处理。不要用全局 `*` 规则把所有动画时长压成 0：Radix Select 的「对齐选中项」定位依赖入场动画，压掉后下拉框会先出现再跳到正确位置（2026-11-23 修复）。

浅色/深色跟随系统。深色模式在 Windows 上由 Mica 自带深色变体，macOS 用系统深色 vibrancy，不透明的地方三个平台都用统一的石墨深色。

### 原型的近似

HTML 原型画不出真实的 Mica 和毛玻璃：

- Mica 用一张模糊的低饱和渐变代替桌面壁纸取色，叠一层窗口底色（浅色 `#ececec` 约 80%，深色 `#2b2b2d` 约 82%）。真实 Mica 只取桌面壁纸颜色，不透出下层窗口，比原型更均匀。
- macOS 侧栏用 `backdrop-filter: blur()` 叠在同一张背景上表示 vibrancy。真实 vibrancy 会透出下层窗口内容。
- 原型自带的窗口外框、Windows 和 Linux 标题栏和红绿灯都是示意，真实应用里由系统绘制。
- 原型的控件（按钮、输入框、开关、分段筛选、菜单）是手写的近似，不是 Nova 组件；三个平台切换后内容区一致，只剩外壳差异。控件细节以应用为准。

## 去网页感的行为清单

所有平台一致，在前端统一实现（Windows 另用 WebView2 设置做第一道屏蔽，见下一节）：

1. **右键菜单**：全局屏蔽浏览器右键菜单。输入框、文本框保留系统编辑菜单（剪切、复制、粘贴）。任务行和录像条目弹出应用自己的菜单，菜单项与现有操作一致：
   - 任务行：开始录制/停止录制、重新录制、暂停监控/恢复监控、编辑、打开文件夹、复制直播间链接、删除。
   - 录像条目：用系统播放器打开或进入文件夹、在文件夹中显示、复制路径（后端没有删除录像的操作）。键盘上按 Shift+F10 或菜单键，在当前选中的条目上打开。
   - 可选中文字（路径、链接、日志、错误原因）上有选区、且右键落在选区内时，弹出只含「复制」的应用菜单；关闭后焦点回到原来的位置。
   菜单用 shadcn `ContextMenu` 默认样式，三个平台相同。
2. **文字选择**：界面框架（导航、按钮、表头、标签、状态栏、行内容）不可选中。输入框、详情里的文件路径和链接、动态日志、FFmpeg 路径、错误原因（`role="alert"`）、字段错误提示和提示框的说明可以选中复制。
3. **浏览器快捷键**：正式构建屏蔽 F5、Ctrl+R、Ctrl+Shift+R、Ctrl+P、Ctrl+F（浏览器查找）、Ctrl+G/F3、Ctrl+U、Ctrl+S、Ctrl+J、Ctrl+H、F7（光标浏览）、Alt+←/→（前进后退）、Ctrl+加号/减号/0 与 Ctrl+滚轮缩放。开发构建保留 F5 和开发者工具，方便调试。输入框里的 Alt+方向键留给编辑。macOS 的 WKWebView 本来没有这些浏览器快捷键，前端一律不拦截，免得挡住 Option+方向键（按词移动光标）、Cmd+H（隐藏）、Ctrl+Cmd+F（全屏）等系统快捷键。
4. **拖拽**：图片和链接不可拖出（`draggable=false`，并拦截 `dragstart`）；拖入窗口的文件不会让 WebView 打开：`dragDropEnabled` 保持 Tauri 默认（开启），系统拖放由 Tauri 接管、不交给 WebView；页面里另拦截 `dragover` 和 `drop`。
5. **光标**：按钮、导航项、列表行用系统箭头光标；只有真正的外部链接（例如「查看更新说明」「官网」）用手形。
6. **焦点环**：只在键盘操作时显示（`:focus-visible`），鼠标点击不出现焦点框。
7. **滚动条**：随平台（见「外壳与行为」）；`overscroll-behavior: none`，去掉橡皮筋回弹带出的空白（macOS 内容区保留系统回弹）。
8. **其他**：关闭输入框自动填充和拼写检查（链接、路径类字段）；页面标题之外不出现网页式的超链接下划线；空白处点击不出现蓝色点击高亮。

### 应用快捷键

macOS 用 Cmd，其他平台用 Ctrl：

| 快捷键 | 作用 |
|---|---|
| Ctrl/Cmd+N | 添加直播间 |
| Ctrl/Cmd+F | 聚焦当前页的搜索框；没有搜索框的页面不做任何事 |
| Delete（macOS 还有 Cmd+Backspace） | 删除选中项，弹确认 |
| Esc | 关闭对话框；没有对话框时关闭详情面板 |
| Cmd+,（仅 macOS） | 打开设置 |

主修饰键要单独按下：macOS 上同时按着 Ctrl、其他平台上同时按着 Win 时不算（Ctrl+Cmd+F 不会被当成 Cmd+F）。用快捷键切换页面前，先让当前输入框失焦，设置页在失焦时保存，未按 Enter 的修改不会丢。

按钮的悬停提示里写出快捷键，例如「添加直播间（Ctrl+N）」，macOS 显示为「添加直播间 ⌘N」。

## 平台检测与 Tauri 实现

### 检测

- **平台**：`index.html` 里一段同步内联脚本按 `navigator.userAgent` 判断（WebView2 含 `Windows NT`，WKWebView 含 `Macintosh`，WebKitGTK 含 `Linux`），首帧前写入 `<html data-platform="windows|macos|linux">`，不会闪烁。浏览器假数据模式下可用 `?platform=macos` 覆盖，方便在 Windows 上预览三个平台的外壳差异（内容区相同）。开发构建（`npm run dev`、`npm run tauri dev`）里桌面窗口也认 `?platform=` 和 `?material=`，由调试面板写入（见下文「开发调试面板」）；正式构建的桌面端只认 Rust 给出的值。
- **材质**：UA 分不出 Windows 10 和 11，所以材质由 Rust 决定。Rust 建窗前按系统版本定下材质（`mica` / `vibrancy` / `none`），用初始化脚本在首帧前写入 `window.__LUBOBAO_SHELL__`（同时带平台），`index.html` 的内联脚本据此写入 `data-platform` 和 `data-material`，启动时不会先画不透明底色再变透明。建窗后 `get_window_material` 返回实际结果，应用 Mica 出错时前端改回 `none`。
- **深浅色**：继续用 `prefers-color-scheme`。窗口 `theme` 不设置，跟随系统，Mica 和 vibrancy 随之切换深浅。只有开发构建的调试面板会临时设置窗口主题（`core:window:allow-set-theme` 只在调试构建里由 `lib.rs` 放开）。

### Windows

- 主窗口在 `tauri.conf.json` 里设 `create: false`，由 `src-tauri/src/desktop/window.rs` 按配置创建。建窗前用 `RtlGetVersion` 读系统 build 号：22000（Windows 11）起才加 `transparent(true)` 并应用 Mica，Windows 10 建普通的不透明窗口（透明窗口建好后改不回来，否则首帧之前、放大窗口新露出的区域和 WebView2 卡住时都会透出桌面）。`decorations` 保持默认，保留系统标题栏，贴靠布局（Snap Layouts）、拖拽、双击最大化等系统行为不受影响。
- Rust 在创建窗口后调用 `window_vibrancy::apply_mica(&window, None)`（Tauri 已依赖该库；也可用 `window.set_effects`，但它不返回是否成功）。`apply_mica` 在 Windows 11 22H2+ 用 `DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_MAINWINDOW`，在 21H2 用未公开的 `DWMWA_MICA_EFFECT`，其他版本返回错误。
- **Windows 10 退化**：窗口不透明、不应用 Mica，`data-material=none`，前端给 `body` 画不透明的统一底色（浅色 `#ececec`，深色 `#2b2b2d`）。字体栈里 `Segoe UI Variable` 不存在，自动回落到 `Segoe UI`。窗口没有圆角，属系统行为。
- 浏览器快捷键：正式构建里通过 `with_webview` 拿到 `ICoreWebView2Settings3`，调用 `SetAreBrowserAcceleratorKeysEnabled(false)`，失败时记一行日志。浏览器不再处理 Ctrl+F、Ctrl+P、F5 等，但按键仍会传给页面，Ctrl+F 可以用作应用快捷键。前端另加一层 `keydown` 拦截兜底。缩放热键由 Tauri 的 `zoomHotkeysEnabled: false`（默认）关闭，同时也关闭了 Ctrl+滚轮缩放。
- 透明窗口可能在调整大小时出现白边，`noRedirectionBitmap` 可以缓解。目前没有开启：实机验证时窗口被其他窗口遮挡，没有观察到调整大小时的画面，留待在桌面上实际拖动窗口时确认。

### macOS

- `desktop/window.rs` 在创建窗口时设 `title_bar_style(Overlay)`、`hidden_title(true)`、`traffic_light_position(18, 20)`、`transparent(true)` 和 `effects: [Sidebar]`（`FollowsWindowActiveState`）。Tauri 2.12 起透明背景所需的私有 API 默认启用，无需 `macOSPrivateApi`。
- vibrancy 铺满整个窗口，内容区自己画不透明底色，只有侧栏透出材质，效果与 Finder、备忘录一致。
- 内容区顶部 52px 和侧栏顶部带 `data-tauri-drag-region`，可拖动窗口、双击缩放；其中的按钮和搜索框不带该属性，照常响应点击。
- 侧栏顶部留出红绿灯的位置（约 52px），不放品牌标识；品牌名只出现在关于页和菜单栏。

### Linux

- 不支持窗口材质（`windowEffects` 在 Linux 上无效），保持不透明窗口和系统标题栏，`data-material=none`。
- 内容区与其他平台相同（2026-11-23 起不再做 Adwaita 风格的圆角分组列表和配色），只用系统字体。

### 窗口状态

用 `tauri-plugin-window-state` 只记住窗口大小、位置和最大化状态（`SIZE | POSITION | MAXIMIZED`），不恢复显示状态和 macOS 全屏。Windows 应用内更新由插件直接结束进程、不经过 Exit 事件，所以在 `on_before_exit` 里先保存窗口状态。首次启动仍是 1280×800、最小 1024×680。窗口先隐藏创建，插件在创建时恢复尺寸，之后再显示，避免先闪出默认大小。插件不恢复显示状态，托盘隐藏的窗口下次启动仍正常显示。

## 实现方式

- 根节点 `data-platform` 选择字体族和平台行为，`data-material` 决定窗口底是否透明。
- `src/tokens.css` 定义一组三个平台共用的 token：shadcn 语义颜色（含深色）、`--radius`、字号，以及页面布局用的窗口底、内容层、选中行、状态栏、导航项高度、内容边距、侧栏宽度和工具栏高度。平台块只设 `--font-ui`；材质块（`data-material` 为 `mica` 或 `vibrancy`）只把窗口底、侧栏（Mica 另有状态栏）设为透明，Mica 的内容层设为半透明。`index.css` 把 `text-sm`、`text-xs` 映射到这组字号，圆角刻度从 `--radius` 推出。shadcn/ui 组件保持 Nova 默认实现，不改类名。
- 页面里的 `win:`、`mac:`、`linux:` 变体只用于外壳差异，目前只有侧栏的 `mac:pt-(--header-h)`（为红绿灯留位置）。视觉风格不再按平台区分；`win:selection-bar` 及其 token 已删除。
- 对话框按钮顺序：组件里按「取消、主操作」的语义顺序书写，Nova 默认右对齐、主按钮在右；`index.css` 在三个平台上都把 `DialogFooter` 反转成主按钮在左（`flex-direction: row-reverse`），不改按钮样式。
- 高对比度（Windows 强制颜色）下，窗口、内容层、侧栏和状态栏改用系统 `Canvas`，不会因透明底变黑。

## 已知限制

- **多显示器**：窗口位置按上次关闭时恢复。如果那块显示器已经断开或分辨率变小，窗口可能只露出一角（窗口状态插件不做越界校正）。
- **vibrancy 是否生效无法校验**：macOS 的材质在建窗时由 Tauri 应用，不返回结果，`get_window_material` 一律报 `vibrancy`。
- **控件外形不随平台**：按钮、开关、复选框等用 Nova 默认外形，圆角、字号和配色三个平台也相同，不模仿 Fluent、AppKit 或 Adwaita 的控件。
- **Windows 对话框的 Tab 顺序**：按钮视觉顺序由 CSS 反转，Tab 键仍按 DOM 顺序（先取消后主操作），与视觉顺序相反。

## 已确定的取舍（2026-11-23）

1. **列表选中色**：用淡 Teal 底，不用 Teal 实心底加白字，录制红点、检测蓝圈等状态色保持不变。录像库列表三个平台的选中行都是淡 Teal 底；任务表改用 shadcn `Table` 自带的选中样式；左侧导航和设置分组的当前项是灰色底（原来 Windows 的 Teal 竖条随第 5 条去掉）。
2. **Windows 字号**：原为任务表、详情、录像库和动态 13px，其余 14px；随第 5 条改为全部 13px，说明文字 11px。
3. **侧栏品牌标识**：三个平台都去掉。
4. **统计卡片**：直播间页顶部的 5 个统计卡片改成一行带数字的筛选按钮，筛选功能不变。
5. **内容区三个平台统一（2026-11-23）**：为降低三套主题的维护量，内容区统一为原 macOS 的样子（圆角、字号、尺寸、浅色和深色配色、52px 工具栏、圆角分组表单、侧栏和列表的样子），删掉只为视觉风格存在的 `win:`、`linux:` 变体，包括 Windows 的选中竖条、Fluent 设置卡片和内容层左上圆角，以及 Linux 的圆角分组列表和 Adwaita 配色。窗口外壳和平台行为保持各平台自己的：标题栏（Windows、Linux 系统标题栏，macOS Overlay 和红绿灯）、字体族、窗口材质（Mica、vibrancy 及其透明和半透明底色）、快捷键、右键菜单、浏览器快捷键屏蔽和滚动条；`src-tauri` 的窗口创建不变。
6. **对话框按钮顺序和添加按钮（2026-11-23）**：用户以 Windows 为主，对话框主按钮三个平台统一在左；直播间页工具栏的「添加直播间」用带文字的 Teal 实心按钮，不用 macOS 式的无框「+」图标。

## 实现备注

- **页面里不手写交互控件（2026-11-23）**：状态筛选用 `Tabs`（`line` 样式），搜索框用 `InputGroup`（图标 addon 加 `InputGroupButton` 清除），表头排序用 `Button variant="ghost"`，任务表主播名、动态里的主播名和添加对话框的假数据示例用 `Button variant="link"`，录像库路径用 `BreadcrumbLink`。都用默认 variant 和 size，className 只放布局类。只有左侧导航和设置页左侧分组保留手写，键盘焦点与按钮一致（2px 实色 Teal 细环）。
- **组件按 shadcn 官方示例的结构使用**：例如 `SelectItem` 一律放在 `SelectGroup` 里（有标题的分组再加 `SelectLabel`，分组之间用 `SelectSeparator`）。Nova 的 `SelectContent` 没有内边距，内边距在 `SelectGroup` 上；少了这一层，对齐选中项的菜单会比按钮靠右约 5px、靠下 2px，按钮边框和圆角从菜单左上边缘露出来。
- **Select 用 shadcn 默认弹出方式**（对齐选中项），所有调用处都不传 `position` / `align`。打开时跳动的问题来自全局的减少动态效果规则，已去掉（见上文），不要用 `position="popper"` 绕开。

- 「添加直播间」在三个平台上都是工具栏里的无框「+」图标按钮（悬停提示 macOS「添加直播间 ⌘N」，其他平台「添加直播间（Ctrl+N）」）；空列表时页面中间另有 Teal 实心的「添加第一个直播间」。
- 详情面板底部只保留一个带文字的主要操作（停止录制、重新开始录制、恢复或暂停监控），其余（暂停监控、编辑、打开文件夹、删除）是带悬停提示的图标按钮；这些操作也都在任务行的右键菜单里。
- 设置页：三个平台都是圆角分组表单。
- 录像库没有删除操作（后端不提供），右键菜单是「用系统播放器打开 / 进入文件夹」「在文件夹中显示」「复制路径」。
- 浏览器预览（`npm run dev`）可用 `?platform=windows|macos|linux` 切换平台（字体族、侧栏顶部留白、滚动条、快捷键写法），`data-material` 默认为 `none`。
- 开发调试面板（`src/dev/debug-panel.tsx`，只在开发构建里存在）：状态栏最右侧的「调试」或 Ctrl+Shift+D（macOS 为 Cmd+Shift+D）打开。可切换平台、窗口材质、明暗（仅桌面窗口）和示例数据（仅桌面窗口；浏览器预览始终是示例数据），示例数据下还能弹出「发现新版本」、直播间空列表和加载失败。覆盖值放在网址参数里（`?platform=`、`?material=`、`?theme=`、`?mock=1`、`?tasks=empty|fail`，见 `src/dev/overrides.ts`），重新加载后保留，重启应用后恢复默认；有覆盖时入口上显示「已覆盖 N 项」，面板里「全部恢复默认」一键清除。强制材质只改网页底色，窗口本身的材质不变。
