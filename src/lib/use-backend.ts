// 订阅后端快照的 React hooks。
// 顺序：先等监听注册完成，再取快照，再把这段时间收到的事件叠加到快照上，
// 这样既不会漏掉事件，晚到的快照也不会覆盖更新的事件。

import { useEffect, useState } from "react";

import { api, type FfmpegStatus, type TaskId, type TaskSnapshot } from "@/lib/api";

type Unlisten = () => void;

/** 注册若干监听并在全部就绪后调用 `ready`。返回的清理函数在任何时刻调用都安全。 */
function subscribeAll(registrations: Promise<Unlisten>[], ready: () => void): Unlisten {
  let disposed = false;
  let unlisteners: Unlisten[] = [];
  void Promise.all(registrations).then((fns) => {
    if (disposed) {
      fns.forEach((fn) => fn());
      return;
    }
    unlisteners = fns;
    ready();
  });
  return () => {
    disposed = true;
    unlisteners.forEach((fn) => fn());
  };
}

export function useFfmpegStatus(): FfmpegStatus | null {
  const [status, setStatus] = useState<FfmpegStatus | null>(null);
  useEffect(() => {
    let active = true;
    let sawEvent = false;
    const stop = subscribeAll(
      [
        api.onFfmpegStatus((next) => {
          sawEvent = true;
          setStatus(next);
        }),
      ],
      () => {
        void api.getFfmpegStatus().then((initial) => {
          // 取快照期间已收到事件时，事件更新，不用快照覆盖。
          if (active && !sawEvent) setStatus(initial);
        });
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, []);
  return status;
}

type TaskEvent = { type: "updated"; task: TaskSnapshot } | { type: "removed"; ids: TaskId[] };

function applyTaskEvent(tasks: TaskSnapshot[], event: TaskEvent): TaskSnapshot[] {
  if (event.type === "removed") return tasks.filter((task) => !event.ids.includes(task.id));
  const index = tasks.findIndex((item) => item.id === event.task.id);
  if (index < 0) return [...tasks, event.task].sort((a, b) => a.id - b.id);
  const next = tasks.slice();
  next[index] = event.task;
  return next;
}

export function useTasks(): TaskSnapshot[] {
  const [tasks, setTasks] = useState<TaskSnapshot[]>([]);
  useEffect(() => {
    let active = true;
    // 快照到达前收到的事件先缓存，快照到达后按顺序叠加。
    let pending: TaskEvent[] | null = [];
    const handle = (event: TaskEvent) => {
      if (pending) pending.push(event);
      else setTasks((current) => applyTaskEvent(current, event));
    };
    const stop = subscribeAll(
      [
        api.onTaskUpdated((task) => handle({ type: "updated", task })),
        api.onTaskRemoved(({ ids }) => handle({ type: "removed", ids })),
      ],
      () => {
        void api.listTasks().then((snapshot) => {
          if (!active) return;
          const buffered = pending ?? [];
          pending = null;
          setTasks(buffered.reduce(applyTaskEvent, snapshot));
        });
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, []);
  return tasks;
}
