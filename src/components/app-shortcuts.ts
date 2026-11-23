// 应用快捷键（macOS 用 Cmd，其他平台用 Ctrl）：
// Ctrl/Cmd+N 添加直播间，Ctrl/Cmd+F 聚焦当前页的搜索框，Delete（macOS 另有 Cmd+Backspace）删除选中项，
// Esc 关闭详情（对话框和菜单自己处理 Esc），macOS 上 Cmd+, 打开设置。

import { useEffect } from "react";

import type { Navigate } from "@/components/navigation";
import { dispatchAppCommand } from "@/lib/app-commands";
import { isEditable } from "@/lib/native-behavior";
import { hasModifier, isMac } from "@/lib/platform";
import type { PageId } from "@/pages/pages";

/** 对话框、菜单或下拉列表打开时，快捷键交给它们。 */
function overlayOpen(): boolean {
  return (
    document.querySelector(
      "[role=dialog][data-state=open], [role=alertdialog][data-state=open], [role=menu][data-state=open], [data-slot=select-content][data-state=open]",
    ) !== null
  );
}

/**
 * 切换页面前先让当前输入框失焦：设置页的输入框在失焦时保存，页面直接卸载不会触发 onBlur，
 * 未按 Enter 的修改会丢失。
 */
function leavePage(navigate: Navigate, target: PageId) {
  (document.activeElement as HTMLElement | null)?.blur();
  navigate(target);
}

/** 在直播间页执行命令；在其他页面时先切到直播间，等页面挂载后执行。 */
function runOnRooms(command: "add-task", current: PageId, navigate: Navigate) {
  if (current === "rooms") {
    dispatchAppCommand(command);
  } else {
    leavePage(navigate, "rooms");
    dispatchAppCommand(command, true);
  }
}

export function useAppShortcuts(navigate: Navigate, current: PageId): void {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.isComposing) return;
      const key = event.key.toLowerCase();
      const mod = hasModifier(event) && !event.altKey && !event.shiftKey;

      if (mod && key === "n") {
        event.preventDefault();
        if (overlayOpen()) return;
        runOnRooms("add-task", current, navigate);
        return;
      }
      if (mod && key === "f") {
        event.preventDefault();
        if (overlayOpen()) return;
        // 只聚焦当前页的搜索框；没有搜索框的页面不做任何事。
        const input = document.querySelector<HTMLInputElement>("[data-search-input]");
        input?.focus();
        input?.select();
        return;
      }
      if (isMac && mod && key === ",") {
        event.preventDefault();
        if (!overlayOpen() && current !== "settings") leavePage(navigate, "settings");
        return;
      }
      const deleteKey = key === "delete" || (isMac && event.metaKey && key === "backspace");
      if (deleteKey) {
        if (isEditable(event.target) || overlayOpen()) return;
        if (dispatchAppCommand("delete-selection")) event.preventDefault();
        return;
      }
      if (key === "escape") {
        if (event.defaultPrevented || overlayOpen() || isEditable(event.target)) return;
        dispatchAppCommand("dismiss");
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate, current]);
}
