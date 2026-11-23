// 应用快捷键分发给当前页面的命令。App 监听按键，页面用 useAppCommand 订阅自己关心的命令。
// 页面保活后隐藏页仍然挂载，订阅也还在：只有当前页的订阅会处理命令。

import { useEffect, useRef } from "react";

import { usePageActive } from "@/components/page-activity";

export type AppCommand = "add-task" | "delete-selection" | "dismiss";

type Handler = () => boolean | void;

const handlers = new Map<AppCommand, Set<{ current: Handler }>>();
const pending = new Set<AppCommand>();

/**
 * 发出命令，返回是否有页面处理。处理函数返回 false 表示没有处理，隐藏页的订阅一律不处理。
 * queue 为 true 时，若当前没有页面处理，命令留到下一个订阅它的页面成为当前页时执行（先切换页面再执行）。
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
  const active = usePageActive();
  const ref = useRef(handler);
  const activeRef = useRef(active);
  useEffect(() => {
    ref.current = handler;
    activeRef.current = active;
  });
  useEffect(() => {
    const entry = { current: () => (activeRef.current ? ref.current() : false) };
    let set = handlers.get(command);
    if (!set) handlers.set(command, (set = new Set()));
    set.add(entry);
    return () => {
      set.delete(entry);
    };
  }, [command]);
  // 页面挂载或回到当前页时，执行切页前排队的命令（等本次渲染提交、子组件就绪后再执行）。
  useEffect(() => {
    if (active && pending.delete(command)) queueMicrotask(() => ref.current());
  }, [active, command]);
}
