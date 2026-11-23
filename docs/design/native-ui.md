# 原生界面设计说明

录播宝 0.1.0 起界面跟随系统：Windows 用 Windows 11 Fluent 风格，macOS 用原生 AppKit 风格，Linux 用接近 GNOME/Adwaita 的朴素风格。三个平台的布局、页面、功能和快捷键含义一致，差异只在材质、字体、尺寸、控件外形和对话框按钮顺序。交互原型见 `docs/prototypes/native.html`。

不变的部分：

- 主色保留 Teal 品牌色（浅色 teal-700 `#0f766e`，深色 teal-400 `#2dd4bf`），不跟随系统强调色。只有主操作用 Teal 实心按钮，其余是次级按钮或文字按钮。
- 任务状态色和图标沿用 `AGENTS.md`：录制中红色呼吸实心点、检测中蓝色旋转圆环、等待开播灰色空心圈、重试中琥珀色、仅通知 Teal 铃铛、需处理红色警告图标加淡红行底、已暂停整行降低透明度，各状态都有文字。
- 任务行高 44px，状态栏内容不变（FFmpeg、录制数、码率、磁盘余量、下次检测），任务表选中后右侧显示详情，1024 宽时没有横向滚动条。
- 布局：左侧导航（直播间、录像库、动态、设置）、右侧内容，底部状态栏常驻。

## 三个平台的视觉语言

| | Windows 11（Fluent） | macOS | Linux（Adwaita 风格） |
|---|---|---|---|
| 窗口背景与材质 | Mica 铺满窗口；导航栏直接落在 Mica 上，内容区是一层半透明白（深色为 `#ffffff0d`）的「Layer」，左上角 8px 圆角加 1px 描边 | 侧栏用 vibrancy（`sidebar` 材质）；内容区不透明（浅色 `#ffffff`，深色 `#1e1e1e`） | 纯色：窗口 `#fafafb` / `#222226`，侧栏 `#ebebed` / `#2e2e32` |
| 标题栏 | 保留系统标题栏，Mica 延伸进标题栏，深浅色跟随系统 | `titleBarStyle: Overlay` + `hiddenTitle`；红绿灯落在侧栏顶部，内容区顶部 52px 是统一工具栏（页面标题 + 操作），整条是拖拽区 | 保留系统标题栏（GNOME 下由 GTK/窗口管理器绘制） |
| 字体 | `Segoe UI Variable Text`、`Segoe UI`、`Microsoft YaHei UI`；正文 14px，表格 13px，说明 12px，页面标题 20px 半粗 | `-apple-system`（SF Pro）、`PingFang SC`；正文 13px，说明 11px，工具栏标题 15px 粗体 | `Adwaita Sans`、`Cantarell`、`Noto Sans CJK SC`、`Ubuntu`；正文 14px，说明 12px，标题 18px 粗体 |
| 圆角 | 控件 4px，卡片/Layer/对话框 8px | 控件 6px，列表选中 6px，对话框 12px | 按钮 6px，分组列表 12px，对话框 14px |
| 间距与密度 | 4px 网格；控件高 32px，导航项 36px，内容边距 24px | 控件高 24px（工具栏 28px），导航项 28px，内容边距 20px | 控件高 34px，导航项 38px，内容边距 18px |
| 按钮层级 | 主操作：Teal 实心；次级：白色微填充加底边略深的描边（Fluent Standard）；命令栏和行内操作：无框 subtle 按钮，悬停才出底色 | 主操作：Teal 实心推按钮；次级：白色带细阴影的推按钮；工具栏和行内：无框图标按钮 | 主操作：Teal 实心（suggested-action）；次级：灰色平涂（`#0000000f`）；标题行和行内：flat 按钮 |
| 列表与选中 | 行底透明，悬停 `#0000000a`；选中为淡 Teal 底，左侧 3×16px Teal 竖条 | 斑马纹关闭；选中为圆角内嵌淡 Teal 底（见下方取舍 1）；悬停无底色 | 分隔线列表；选中为淡 Teal 底 |
| 对话框 | ContentDialog：标题 20px，正文区白底，底部灰色按钮带；按钮等宽铺满，**主按钮在左** | 居中面板（sheet 外观），12px 圆角；按钮右对齐，**主按钮在最右**，取消在其左 | AdwDialog 外观，14px 圆角；按钮右对齐，主按钮在右 |
| 滚动条 | 细线（2px），悬停区域时扩展为 6px 圆角拇指 | 不定制，使用 WKWebView 原生悬浮滚动条 | 细线，悬停扩展为 8px，贴近 GTK 悬浮滚动条 |
| 焦点环 | 仅键盘操作：2px Teal 实线，外偏 1px，跟随控件圆角 | 仅键盘操作：3px 半透明 Teal 光晕，紧贴控件 | 仅键盘操作：2px 半透明 Teal 实线，内收 |
| 动效 | 167–250ms，减速曲线 `cubic-bezier(0,0,0,1)`；对话框从 1.05 缩放淡入，页面切换上移 8px 淡入 | 200ms `ease-out`；对话框自上方下滑淡入 | 200ms `ease-out`；对话框从 0.95 缩放淡入 |

三套都遵守 `prefers-reduced-motion`：开启时去掉位移和缩放，只保留淡入，录制点的呼吸动画改成常亮。

浅色/深色跟随系统。深色模式在 Windows 上由 Mica 自带深色变体，macOS 用系统深色 vibrancy，Linux 用 Adwaita 深色灰阶（中性石墨，不偏蓝）。

### 原型的近似

HTML 原型画不出真实的 Mica 和毛玻璃：

- Mica 用一张模糊的低饱和渐变代替桌面壁纸取色，叠一层窗口底色（浅色 `#f3f3f3` 约 80%，深色 `#202020` 约 80%）。真实 Mica 只取桌面壁纸颜色，不透出下层窗口，比原型更均匀。
- macOS 侧栏用 `backdrop-filter: blur()` 叠在同一张背景上表示 vibrancy。真实 vibrancy 会透出下层窗口内容。
- 原型自带的窗口外框、Windows 标题栏和红绿灯都是示意，真实应用里由系统绘制。

## 去网页感的行为清单

所有平台一致，在前端统一实现（Windows 另用 WebView2 设置做第一道屏蔽，见下一节）：

1. **右键菜单**：全局屏蔽浏览器右键菜单。输入框、文本框保留系统编辑菜单（剪切、复制、粘贴）。任务行和录像条目弹出应用自己的菜单，菜单项与现有操作一致：
   - 任务行：开始录制/停止录制、重新录制、暂停监控/恢复监控、编辑、打开文件夹、复制直播间链接、删除。
   - 录像条目：打开、在文件夹中显示、复制路径、删除。
   - 可选中文字（路径、链接、日志）上有选区时，弹出只含「复制」的应用菜单。
   菜单外观随平台：Windows 8px 圆角加阴影、项高 32px；macOS 6px 圆角半透明、项高 22px；Linux 12px 圆角、项高 32px。
2. **文字选择**：界面框架（导航、按钮、表头、标签、状态栏、行内容）不可选中。输入框、详情里的文件路径和链接、动态日志、错误详情可以选中复制。
3. **浏览器快捷键**：正式构建屏蔽 F5、Ctrl+R、Ctrl+Shift+R、Ctrl+P、Ctrl+F（浏览器查找）、Ctrl+G/F3、Ctrl+U、Ctrl+S、Ctrl+J、Ctrl+H、F7（光标浏览）、Alt+←/→（前进后退）、Ctrl+加号/减号/0 与 Ctrl+滚轮缩放。开发构建保留 F5 和开发者工具，方便调试。
4. **拖拽**：图片和链接不可拖出（`draggable=false`，并拦截 `dragstart`）；窗口不接受拖入文件导航（`dragDropEnabled` 保持关闭浏览器默认行为，必要时由应用处理）。
5. **光标**：按钮、导航项、列表行用系统箭头光标；只有真正的外部链接（例如「查看更新说明」「官网」）用手形。
6. **焦点环**：只在键盘操作时显示（`:focus-visible`），鼠标点击不出现焦点框。
7. **滚动条**：按上表随平台；`overscroll-behavior: none`，去掉橡皮筋回弹带出的空白（macOS 内容区保留系统回弹）。
8. **其他**：关闭输入框自动填充和拼写检查（链接、路径类字段）；页面标题之外不出现网页式的超链接下划线；空白处点击不出现蓝色点击高亮。

### 应用快捷键

macOS 用 Cmd，其他平台用 Ctrl：

| 快捷键 | 作用 |
|---|---|
| Ctrl/Cmd+N | 添加直播间 |
| Ctrl/Cmd+F | 聚焦当前页的搜索框 |
| Delete（macOS 还有 Cmd+Backspace） | 删除选中项，弹确认 |
| Esc | 关闭对话框；没有对话框时关闭详情面板 |
| Cmd+,（仅 macOS） | 打开设置 |

按钮的悬停提示里写出快捷键，例如「添加直播间（Ctrl+N）」，macOS 显示为「添加直播间 ⌘N」。

## 平台检测与 Tauri 实现

### 检测

- **平台**：`index.html` 里一段同步内联脚本按 `navigator.userAgent` 判断（WebView2 含 `Windows NT`，WKWebView 含 `Macintosh`，WebKitGTK 含 `Linux`），首帧前写入 `<html data-platform="windows|macos|linux">`，不会闪烁。浏览器假数据模式下可用 `?platform=macos` 覆盖，方便在 Windows 上预览三套主题。
- **材质**：UA 分不出 Windows 10 和 11，所以材质由 Rust 决定。Rust 在窗口创建后尝试应用材质，把结果（`mica` / `vibrancy` / `none`）通过 command 交给前端，前端写入 `data-material`。在拿到结果之前按 `none`（不透明底色）渲染，最坏情况是启动时一帧不透明，不会出现透明窗口。
- **深浅色**：继续用 `prefers-color-scheme`。窗口 `theme` 不设置，跟随系统，Mica 和 vibrancy 随之切换深浅。

### Windows

- 主窗口在 `tauri.conf.json` 里设 `create: false`，由 `src-tauri/src/desktop/window.rs` 按配置创建并加上 `transparent(true)`，`decorations` 保持默认，保留系统标题栏，贴靠布局（Snap Layouts）、拖拽、双击最大化等系统行为不受影响。
- Rust 在创建窗口后调用 `window_vibrancy::apply_mica(&window, None)`（Tauri 已依赖该库；也可用 `window.set_effects`，但它不返回是否成功）。`apply_mica` 在 Windows 11 22H2+ 用 `DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_MAINWINDOW`，在 21H2 用未公开的 `DWMWA_MICA_EFFECT`，其他版本返回错误。
- **Windows 10 退化**：`apply_mica` 返回错误，`data-material=none`，前端给 `body` 画不透明的 Windows 10 底色（浅色 `#f3f3f3`，深色 `#202020`）。透明窗口被网页内容完全盖住，看起来与普通窗口相同。字体栈里 `Segoe UI Variable` 不存在，自动回落到 `Segoe UI`。窗口没有圆角，属系统行为。
- 浏览器快捷键：正式构建里通过 `with_webview` 拿到 `ICoreWebView2Settings3`，调用 `SetAreBrowserAcceleratorKeysEnabled(false)`。浏览器不再处理 Ctrl+F、Ctrl+P、F5 等，但按键仍会传给页面，Ctrl+F 可以用作应用快捷键。前端另加一层 `keydown` 拦截兜底。缩放热键由 Tauri 的 `zoomHotkeysEnabled: false`（默认）关闭，同时也关闭了 Ctrl+滚轮缩放。
- 透明窗口可能在调整大小时出现白边，`noRedirectionBitmap` 可以缓解。目前没有开启：实机验证时窗口被其他窗口遮挡，没有观察到调整大小时的画面，留待在桌面上实际拖动窗口时确认。

### macOS

- `desktop/window.rs` 在创建窗口时设 `title_bar_style(Overlay)`、`hidden_title(true)`、`traffic_light_position(18, 20)`、`transparent(true)` 和 `effects: [Sidebar]`（`FollowsWindowActiveState`）。Tauri 2.12 起透明背景所需的私有 API 默认启用，无需 `macOSPrivateApi`。
- vibrancy 铺满整个窗口，内容区自己画不透明底色，只有侧栏透出材质，效果与 Finder、备忘录一致。
- 内容区顶部 52px 和侧栏顶部带 `data-tauri-drag-region`，可拖动窗口、双击缩放；其中的按钮和搜索框不带该属性，照常响应点击。
- 侧栏顶部留出红绿灯的位置（约 52px），不放品牌标识；品牌名只出现在关于页和菜单栏。

### Linux

- 不支持窗口材质（`windowEffects` 在 Linux 上无效），保持不透明窗口和系统标题栏，`data-material=none`。
- 视觉贴近 Adwaita：平涂灰色侧栏、圆角分组列表、平涂按钮。不引入 libadwaita 本身，WebKitGTK 里只是 CSS 近似。

### 窗口状态

用 `tauri-plugin-window-state` 记住窗口大小、位置和最大化状态。首次启动仍是 1280×800、最小 1024×680。窗口先隐藏创建，插件在创建时恢复尺寸，之后再显示，避免先闪出默认大小。插件不恢复显示状态，托盘隐藏的窗口下次启动仍正常显示。

## 实现方式

- 根节点 `data-platform` 选择平台主题，`data-material` 决定窗口底是否透明。
- `src/tokens.css` 按平台分组定义 token：颜色（含深色）、字体、字号、控件高度、圆角、导航项高度、内容边距、对话框与菜单圆角、动效时长和曲线。shadcn/ui 组件只读这些变量，按需调整类名，不另起组件库。
- 对话框按钮顺序用 `DialogFooter` 的 CSS（`flex-direction` 随平台）控制，组件里按「取消、主操作」的语义顺序书写。

## 已确定的取舍（2026-11-23）

1. **macOS 列表选中色**：用淡 Teal 底，不用 Teal 实心底加白字，录制红点、检测蓝圈等状态色保持不变。三个平台的选中行都是淡 Teal 底，Windows 另在左侧画 Teal 竖条。
2. **Windows 字号**：任务表、右侧详情、录像库和动态 13px（`--text-dense`），导航、设置、对话框等其余部分 14px（`--text-ui`）。
3. **侧栏品牌标识**：三个平台都去掉。
4. **统计卡片**：直播间页顶部的 5 个统计卡片改成一行带数字的筛选按钮，筛选功能不变。

## 实现备注

- macOS 的「添加直播间」是工具栏里的无框「+」图标按钮（悬停提示「添加直播间 ⌘N」），与系统应用的工具栏一致；Windows 和 Linux 是 Teal 实心主按钮。
- 详情面板底部只保留一个带文字的主要操作（停止录制、重新开始录制、恢复或暂停监控），其余（暂停监控、编辑、打开文件夹、删除）是带悬停提示的图标按钮；这些操作也都在任务行的右键菜单里。
- 设置页：Windows 每个设置项是一张 Fluent 设置卡片，macOS 是圆角分组表单，Linux 是 Adwaita 分组列表。
- 录像库没有删除操作（后端不提供），右键菜单是「用系统播放器打开 / 进入文件夹」「在文件夹中显示」「复制路径」。
- 浏览器预览（`npm run dev`）可用 `?platform=windows|macos|linux` 切换平台主题，`data-material` 始终为 `none`。
