import { useRef, useSyncExternalStore } from "react";

import { usePageActive } from "@/components/page-activity";

// 全页共用一个每秒跳动的时钟：只有订阅它的计时单元格会重新渲染。
// 所在页面隐藏时不订阅（没有订阅者时时钟停止），回到当前页时立即取一次当前时间。
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

function subscribeNothing() {
  return () => undefined;
}

/** 当前时间，每秒更新；所在页面隐藏时停在隐藏前的值。只在需要计时的小组件里使用。 */
export function useNow(): number {
  const active = usePageActive();
  const frozen = useRef(now);
  const value = useSyncExternalStore(active ? subscribe : subscribeNothing, active ? getNow : () => frozen.current);
  if (active) frozen.current = value;
  return value;
}
