// 平台与窗口材质。说明见 docs/specs/native-ui.md。
//
// data-platform 由 index.html 的内联脚本在首帧前按 UA 写入（浏览器假数据模式可用 ?platform= 覆盖），
// 这里只读取。data-material 由 Rust 决定（Windows 11 的 Mica、macOS 的 vibrancy），拿到结果前按 none 渲染。

import { invoke, isTauri } from "@tauri-apps/api/core";

export type Platform = "windows" | "macos" | "linux";
export type WindowMaterial = "mica" | "vibrancy" | "none";

function readPlatform(): Platform {
  const value = document.documentElement.dataset.platform;
  return value === "macos" || value === "linux" ? value : "windows";
}

export const platform: Platform = readPlatform();
export const isMac = platform === "macos";

/**
 * 平台的主修饰键是否按下，且另一个修饰键（macOS 的 Ctrl、其他平台的 Win/Meta）没有按下：
 * macOS 为 Cmd，其他平台为 Ctrl。Ctrl+Cmd+F（macOS 全屏）这类组合因此不会被当成 Cmd+F。
 */
export function hasModifier(event: KeyboardEvent | MouseEvent | WheelEvent): boolean {
  return isMac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
}

const keyNames: Record<string, [mac: string, other: string]> = {
  Delete: ["⌘⌫", "Delete"],
  Escape: ["⎋", "Esc"],
};

/**
 * 悬停提示里的快捷键写法。key 为主键（"N"、","），withModifier 表示需要 Ctrl/Cmd。
 * macOS 写作「⌘N」，其他平台写作「Ctrl+N」。
 */
export function shortcutLabel(key: string, withModifier = true): string {
  const named = keyNames[key];
  if (named) return isMac ? named[0] : named[1];
  if (!withModifier) return key;
  return isMac ? `⌘${key}` : `Ctrl+${key}`;
}

/** 带快捷键的提示文字：macOS「添加直播间 ⌘N」，其他平台「添加直播间（Ctrl+N）」。 */
export function withShortcut(label: string, key: string, withModifier = true): string {
  const shortcut = shortcutLabel(key, withModifier);
  return isMac ? `${label} ${shortcut}` : `${label}（${shortcut}）`;
}

function setMaterial(material: WindowMaterial) {
  document.documentElement.dataset.material = material;
}

/**
 * 向 Rust 确认窗口材质是否生效，写入 data-material。首帧前 index.html 已按 Rust 的计划写入，
 * 这里只在应用失败（例如 Mica 应用出错）时改回 none。浏览器预览始终为 none。
 */
export function applyWindowMaterial(): void {
  // 开发构建的调试面板用 ?material= 强制了材质（index.html 已写入），不再改回。
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("material")) return;
  if (!isTauri()) {
    setMaterial("none");
    return;
  }
  invoke<WindowMaterial>("get_window_material").then(
    (material) => setMaterial(material),
    () => setMaterial("none"),
  );
}

/**
 * 窗口拖拽区：只有 macOS 用 Overlay 标题栏，需要在侧栏顶部和页面工具栏上拖动窗口；
 * Windows 和 Linux 保留系统标题栏，不设拖拽区。用法：<header {...dragRegion}>。
 */
export const dragRegion: { "data-tauri-drag-region"?: true } = isMac ? { "data-tauri-drag-region": true } : {};
