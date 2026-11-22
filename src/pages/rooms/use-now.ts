import { useSyncExternalStore } from "react";

// 全页共用一个每秒跳动的时钟：只有订阅它的计时单元格会重新渲染。
const listeners = new Set<() => void>();
let now = Date.now();
let timer: ReturnType<typeof setInterval> | undefined;

function subscribe(listener: () => void) {
  listeners.add(listener);
  if (!timer) {
    now = Date.now();
    timer = setInterval(() => {
      now = Date.now();
      for (const fn of listeners) fn();
    }, 1000);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      clearInterval(timer);
      timer = undefined;
    }
  };
}

function getNow() {
  return now;
}

/** 当前时间，每秒更新。只在需要计时的小组件里使用。 */
export function useNow(): number {
  return useSyncExternalStore(subscribe, getNow);
}
