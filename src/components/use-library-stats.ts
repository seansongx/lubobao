// 录像根目录的占用与磁盘空间。所有使用者共享同一份数据：有组件挂载时每分钟刷新一次，
// 窗口重新获得焦点时也刷新；任何一处调用 refresh（例如录像库页面的刷新按钮），状态栏同时更新。

import { useEffect, useSyncExternalStore } from "react";

import { api, type LibraryStats } from "@/lib/api";

export const LIBRARY_STATS_INTERVAL_MS = 60_000;

interface Snapshot {
  stats: LibraryStats | null;
  failed: boolean;
}

let snapshot: Snapshot = { stats: null, failed: false };
const listeners = new Set<() => void>();
let request = 0;

function publish(next: Snapshot) {
  snapshot = next;
  listeners.forEach((listener) => listener());
}

export function refreshLibraryStats() {
  const id = ++request;
  api.getLibraryStats().then(
    (stats) => {
      if (id === request) publish({ stats, failed: false });
    },
    () => {
      if (id === request) publish({ stats: snapshot.stats, failed: true });
    },
  );
}

let timer: ReturnType<typeof setInterval> | undefined;

function subscribe(listener: () => void) {
  listeners.add(listener);
  if (listeners.size === 1) {
    refreshLibraryStats();
    timer = setInterval(refreshLibraryStats, LIBRARY_STATS_INTERVAL_MS);
    window.addEventListener("focus", refreshLibraryStats);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      clearInterval(timer);
      window.removeEventListener("focus", refreshLibraryStats);
    }
  };
}

export function useLibraryStats() {
  const current = useSyncExternalStore(subscribe, () => snapshot);
  // 新挂载的使用者（例如打开录像库页面）取一次最新数据。
  useEffect(refreshLibraryStats, []);
  return { ...current, refresh: refreshLibraryStats };
}
