// 调试面板的覆盖值（只在开发构建里使用，见 debug-panel.tsx）。
//
// 全部存在网址参数里，重新加载后保留，重启应用后恢复默认：
//   ?platform=windows|macos|linux   平台（index.html 首帧前读取）
//   ?material=mica|vibrancy|none     窗口材质（index.html 首帧前读取，platform.ts 不再向 Rust 确认）
//   ?theme=light|dark                明暗（仅桌面端，用 Tauri 窗口主题让 prefers-color-scheme 跟着变）
//   ?mock=1                          桌面端改用 mock-api.ts 的示例数据（api.ts 读取）
//   ?tasks=empty|fail                示例数据的直播间列表：空列表、前两次读取失败（mock-api.ts 读取）
// mock-api.ts 的其他参数（?ffmpeg= 等）照旧可用，「全部恢复默认」会一并清掉。

import { isTauri } from "@tauri-apps/api/core";

import type { Platform, WindowMaterial } from "@/lib/platform";

export type Theme = "light" | "dark";
export type TasksScenario = "empty" | "fail";

export interface DevOverrides {
  platform: Platform | null;
  material: WindowMaterial | null;
  theme: Theme | null;
  mock: boolean;
  tasks: TasksScenario | null;
}

function pick<T extends string>(value: string | null, allowed: readonly T[]): T | null {
  return allowed.includes(value as T) ? (value as T) : null;
}

export function readOverrides(): DevOverrides {
  const params = new URLSearchParams(location.search);
  return {
    platform: pick(params.get("platform"), ["windows", "macos", "linux"] as const),
    material: pick(params.get("material"), ["mica", "vibrancy", "none"] as const),
    theme: isTauri() ? pick(params.get("theme"), ["light", "dark"] as const) : null,
    // 浏览器里本来就是示例数据，参数无意义。
    mock: isTauri() && params.get("mock") === "1",
    tasks: pick(params.get("tasks"), ["empty", "fail"] as const),
  };
}

function writeParams(patch: Partial<DevOverrides>): URL {
  const url = new URL(location.href);
  for (const [key, value] of Object.entries(patch)) {
    if (key === "mock") {
      if (value) url.searchParams.set("mock", "1");
      else url.searchParams.delete("mock");
    } else if (value === null || value === undefined) {
      url.searchParams.delete(key);
    } else {
      url.searchParams.set(key, String(value));
    }
  }
  return url;
}

/** 改参数并重新加载页面（平台、材质、数据来源和列表场景都在加载时决定）。 */
export function reloadWith(patch: Partial<DevOverrides>): void {
  location.replace(writeParams(patch).href);
}

/** 只改参数不重新加载（明暗即时生效，参数用来在重新加载后保持）。 */
export function rememberTheme(theme: Theme | null): void {
  history.replaceState(history.state, "", writeParams({ theme }).href);
}

/** 去掉全部网址参数并重新加载。 */
export function resetAll(): void {
  const url = new URL(location.href);
  url.search = "";
  location.replace(url.href);
}

/** 当前强制了几项（入口上的提示用）。 */
export function overrideLabels(overrides: DevOverrides): string[] {
  const labels: string[] = [];
  if (overrides.platform) labels.push(platformNames[overrides.platform]);
  if (overrides.material) labels.push(`材质 ${materialNames[overrides.material]}`);
  if (overrides.theme) labels.push(overrides.theme === "dark" ? "深色" : "浅色");
  if (overrides.mock) labels.push("示例数据");
  if (overrides.tasks) labels.push(overrides.tasks === "empty" ? "空列表" : "加载失败");
  return labels;
}

export const platformNames: Record<Platform, string> = { windows: "Windows", macos: "macOS", linux: "Linux" };
export const materialNames: Record<WindowMaterial, string> = { mica: "Mica", vibrancy: "Vibrancy", none: "不透明" };

/** 真实环境的平台和材质：桌面端来自 Rust 注入的 __LUBOBAO_SHELL__，浏览器按 UA、材质为 none。 */
export function realShell(): { platform: Platform; material: WindowMaterial } {
  const shell = (window as { __LUBOBAO_SHELL__?: { platform: Platform; material: WindowMaterial } })
    .__LUBOBAO_SHELL__;
  if (shell) return shell;
  const ua = navigator.userAgent;
  const platform: Platform = /Macintosh|Mac OS X/.test(ua)
    ? "macos"
    : /Windows/.test(ua)
      ? "windows"
      : /Linux|X11/.test(ua)
        ? "linux"
        : "windows";
  return { platform, material: "none" };
}
