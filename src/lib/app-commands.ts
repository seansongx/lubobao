// 应用快捷键分发给当前页面的命令。App 监听按键，页面用 useAppCommand 订阅自己关心的命令。

import { useEffect, useRef } from "react";

export type AppCommand = "add-task" | "focus-search" | "delete-selection" | "dismiss";

type Handler = () => boolean | void;

const handlers = new Map<AppCommand, Set<{ current: Handler }>>();
const pending = new Set<AppCommand>();

/**
 * 发出命令，返回是否有页面处理。处理函数返回 false 表示没有处理。
 * queue 为 true 时，若当前没有订阅者，命令留到下一个订阅者挂载时执行（先切换页面再执行）。
 */
export function dispatchAppCommand(command: AppCommand, queue = false): boolean {
  const set = handlers.get(command);
  let handled = false;
  if (set) {
    for (const ref of set) if (ref.current() !== false) handled = true;
  }
  if (!handled && queue) pending.add(command);
  return handled;
}

export function useAppCommand(command: AppCommand, handler: Handler): void {
  const ref = useRef(handler);
  useEffect(() => {
    ref.current = handler;
  });
  useEffect(() => {
    const entry = { current: () => ref.current() };
    let set = handlers.get(command);
    if (!set) handlers.set(command, (set = new Set()));
    set.add(entry);
    if (pending.delete(command)) {
      // 等本次渲染提交、子组件就绪后再执行
      queueMicrotask(() => entry.current());
    }
    return () => {
      set.delete(entry);
    };
  }, [command]);
}
