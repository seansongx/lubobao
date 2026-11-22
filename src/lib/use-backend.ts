// 订阅后端快照的 React hooks：先取一次快照，再按事件增量更新。

import { useEffect, useState } from "react";

import { api, type FfmpegStatus, type TaskSnapshot } from "@/lib/api";

function subscribe(register: Promise<() => void>): () => void {
  let disposed = false;
  let unlisten: (() => void) | undefined;
  void register.then((fn) => {
    if (disposed) fn();
    else unlisten = fn;
  });
  return () => {
    disposed = true;
    unlisten?.();
  };
}

export function useFfmpegStatus(): FfmpegStatus | null {
  const [status, setStatus] = useState<FfmpegStatus | null>(null);
  useEffect(() => {
    let fresh = false;
    const stop = subscribe(
      api.onFfmpegStatus((next) => {
        fresh = true;
        setStatus(next);
      }),
    );
    void api.getFfmpegStatus().then((initial) => {
      // 事件比快照新时不覆盖。
      if (!fresh) setStatus(initial);
    });
    return stop;
  }, []);
  return status;
}

export function useTasks(): TaskSnapshot[] {
  const [tasks, setTasks] = useState<TaskSnapshot[]>([]);
  useEffect(() => {
    const stopUpdated = subscribe(
      api.onTaskUpdated((task) =>
        setTasks((current) => {
          const index = current.findIndex((item) => item.id === task.id);
          if (index < 0) return [...current, task];
          const next = current.slice();
          next[index] = task;
          return next;
        }),
      ),
    );
    const stopRemoved = subscribe(
      api.onTaskRemoved(({ ids }) =>
        setTasks((current) => current.filter((task) => !ids.includes(task.id))),
      ),
    );
    void api.listTasks().then(setTasks);
    return () => {
      stopUpdated();
      stopRemoved();
    };
  }, []);
  return tasks;
}
