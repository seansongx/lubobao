// 去掉网页感的全局行为，说明见 docs/design/native-ui.md「去网页感的行为清单」。
// 在 main.tsx 里、React 渲染前安装一次。

import { hasModifier, isMac } from "@/lib/platform";

/** 可以弹出系统编辑菜单（剪切、复制、粘贴）的元素。 */
export function isEditable(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  const field = target.closest("input, textarea, [contenteditable=''], [contenteditable='true']");
  if (!field) return false;
  if (field instanceof HTMLInputElement) {
    return !["checkbox", "radio", "button", "submit", "reset", "range", "color", "file"].includes(field.type);
  }
  return true;
}

/**
 * 浏览器自带、应用里没有意义的快捷键。开发构建保留刷新，方便调试。
 * macOS 的 WKWebView 本来没有这些快捷键，一律放行，免得挡住系统的 Cmd+H、Option+方向键等。
 */
function isBrowserShortcut(event: KeyboardEvent): boolean {
  if (isMac) return false;
  const key = event.key.toLowerCase();
  const reload = key === "f5" || (hasModifier(event) && key === "r");
  if (reload) return !import.meta.env.DEV;
  if (["f3", "f7"].includes(key)) return true;
  // 前进后退；输入框里的 Alt+方向键留给编辑
  if (event.altKey && (key === "arrowleft" || key === "arrowright")) return !isEditable(event.target);
  if (!hasModifier(event)) return false;
  // 打印、查看源代码、保存网页、查找下一个、下载、历史记录、缩放；Ctrl+F 由应用接管（聚焦搜索框）
  return ["p", "u", "s", "g", "j", "h", "f", "=", "+", "-", "_", "0"].includes(key);
}

export function installNativeBehavior(): void {
  // 记下最后一次输入方式，供 index.css 判断焦点环：Radix 的 Tabs 等组件在鼠标按下时用脚本聚焦，浏览器在之前
  // 没有焦点元素时会把这种聚焦也算作 :focus-visible；按钮类的焦点环只在最后一次输入来自键盘时显示。
  const root = document.documentElement;
  document.addEventListener("pointerdown", () => (root.dataset.input = "pointer"), { capture: true });
  document.addEventListener("keydown", () => (root.dataset.input = "keyboard"), { capture: true });

  // 右键：输入框保留系统编辑菜单，其余一律不弹浏览器菜单。应用自己的菜单（任务行、录像条目在 React 里，
  // 选中文字的「复制」在 document 上）先处理并 preventDefault；这里挂在 window 上，冒泡到最后兜底。
  window.addEventListener("contextmenu", (event) => {
    if (isEditable(event.target)) return;
    event.preventDefault();
  });

  document.addEventListener(
    "keydown",
    (event) => {
      if (isBrowserShortcut(event)) event.preventDefault();
    },
    { capture: true },
  );

  // Ctrl+滚轮缩放（Tauri 默认已关闭缩放热键，这里兜底）
  window.addEventListener(
    "wheel",
    (event) => {
      if (event.ctrlKey) event.preventDefault();
    },
    { passive: false },
  );

  // 图片和链接不可拖出；拖入的文件不让 WebView 打开
  document.addEventListener("dragstart", (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target?.closest("[draggable='true']")) event.preventDefault();
  });
  document.addEventListener("dragover", (event) => event.preventDefault());
  document.addEventListener("drop", (event) => event.preventDefault());
}
