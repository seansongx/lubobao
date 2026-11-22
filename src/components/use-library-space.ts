// 录像目录的磁盘空间与占用。
// - 磁盘剩余空间很快（Rust 只调一次系统接口）：状态栏和录像库头部共享，每分钟和窗口获得焦点时刷新。
// - 占用需要递归遍历，只给录像库页面用：Rust 缓存结果、同一时间只遍历一次；仍在统计时每 2 秒再问一次。
// 两者都保证同一时间最多一个请求在路上。

import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";

import { api, type DiskSpace, type UsageSnapshot } from "@/lib/api";

export const DISK_SPACE_INTERVAL_MS = 60_000;
const USAGE_POLL_MS = 2_000;

// ---- 磁盘空间（共享） ----

interface DiskSnapshot {
  space: DiskSpace | null;
  failed: boolean;
}

let disk: DiskSnapshot = { space: null, failed: false };
const diskListeners = new Set<() => void>();
let diskInFlight = false;
let diskTimer: ReturnType<typeof setInterval> | undefined;

export function refreshDiskSpace() {
  if (diskInFlight) return;
  diskInFlight = true;
  api
    .getDiskSpace()
    .then(
      (space) => ({ space, failed: false }),
      () => ({ space: disk.space, failed: true }),
    )
    .then((next) => {
      diskInFlight = false;
      disk = next;
      diskListeners.forEach((listener) => listener());
    });
}

function subscribeDisk(listener: () => void) {
  diskListeners.add(listener);
  if (diskListeners.size === 1) {
    refreshDiskSpace();
    diskTimer = setInterval(refreshDiskSpace, DISK_SPACE_INTERVAL_MS);
    window.addEventListener("focus", refreshDiskSpace);
  }
  return () => {
    diskListeners.delete(listener);
    if (diskListeners.size === 0) {
      clearInterval(diskTimer);
      window.removeEventListener("focus", refreshDiskSpace);
    }
  };
}

export function useDiskSpace(): DiskSnapshot {
  return useSyncExternalStore(subscribeDisk, () => disk);
}

// ---- 占用（录像库页面） ----

export function useLibraryUsage() {
  const [snapshot, setSnapshot] = useState<UsageSnapshot | null>(null);
  const inFlight = useRef(false);
  const mounted = useRef(true);
  const poll = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  // 请求在路上时又点了刷新，回来后再补一次。
  const pendingRefresh = useRef(false);

  const request = useCallback((refresh: boolean) => {
    if (inFlight.current) {
      if (refresh) pendingRefresh.current = true;
      return;
    }
    inFlight.current = true;
    clearTimeout(poll.current);
    const done = (next: UsageSnapshot | null) => {
      inFlight.current = false;
      if (!mounted.current) return;
      if (next) setSnapshot(next);
      if (pendingRefresh.current) {
        pendingRefresh.current = false;
        request(true);
      } else if (next?.computing) {
        poll.current = setTimeout(() => request(false), USAGE_POLL_MS);
      }
    };
    api.getLibraryUsage(refresh).then(done, () => done(null));
  }, []);

  useEffect(() => {
    mounted.current = true;
    request(false);
    return () => {
      mounted.current = false;
      clearTimeout(poll.current);
    };
  }, [request]);

  const refresh = useCallback(() => {
    request(true);
    refreshDiskSpace();
  }, [request]);

  return { snapshot, refresh };
}
