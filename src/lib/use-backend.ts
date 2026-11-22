// 订阅后端快照的 React hooks。
// 顺序：先等监听注册完成，再取快照，再把这段时间收到的事件叠加到快照上，
// 这样既不会漏掉事件，晚到的快照也不会覆盖更新的事件。

import { useCallback, useEffect, useState } from "react";

import { api, type FfmpegStatus, type SchedulerStatus, type TaskId, type TaskSnapshot } from "@/lib/api";

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

/** 调度概况：先注册 scheduler-status 监听再取快照，事件比快照新时不被覆盖。 */
export function useSchedulerStatus(): SchedulerStatus | null {
  const [status, setStatus] = useState<SchedulerStatus | null>(null);
  useEffect(() => {
    let active = true;
    let sawEvent = false;
    const stop = subscribeAll(
      [
        api.onSchedulerStatus((next) => {
          sawEvent = true;
          setStatus(next);
        }),
      ],
      () => {
        api.getSchedulerStatus().then(
          (initial) => {
            if (active && !sawEvent) setStatus(initial);
          },
          () => undefined,
        );
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, []);
  return status;
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

export interface TaskList {
  /** null 表示首个快照尚未到达。 */
  tasks: TaskSnapshot[] | null;
  /** 读取快照失败的原因；重试成功后清空。 */
  error: unknown;
  reload: () => void;
}

/**
 * 订阅任务快照，带加载和失败状态。
 * 事件先进入队列，每帧最多合并更新一次，避免多个录制的进度事件逐条触发渲染。
 * 窗口隐藏时 requestAnimationFrame 会暂停，因此另设定时器兜底。
 */
export function useTaskList(): TaskList {
  const [tasks, setTasks] = useState<TaskSnapshot[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let active = true;
    // 快照到达前收到的事件先缓存，快照到达后按顺序叠加。
    let pending: TaskEvent[] | null = [];
    let queue: TaskEvent[] = [];
    let frame = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const flush = () => {
      cancelAnimationFrame(frame);
      clearTimeout(timer);
      frame = 0;
      timer = undefined;
      const events = queue;
      queue = [];
      if (active && events.length > 0) {
        setTasks((current) => (current ? events.reduce(applyTaskEvent, current) : current));
      }
    };
    const handle = (event: TaskEvent) => {
      if (pending) {
        pending.push(event);
        return;
      }
      queue.push(event);
      if (timer === undefined) {
        frame = requestAnimationFrame(flush);
        timer = setTimeout(flush, 250);
      }
    };
    const stop = subscribeAll(
      [
        api.onTaskUpdated((task) => handle({ type: "updated", task })),
        api.onTaskRemoved(({ ids }) => handle({ type: "removed", ids })),
      ],
      () => {
        api.listTasks().then(
          (snapshot) => {
            if (!active) return;
            const buffered = pending ?? [];
            pending = null;
            setError(null);
            setTasks(buffered.reduce(applyTaskEvent, snapshot));
          },
          (reason: unknown) => {
            if (active) setError(reason ?? new Error("读取任务失败"));
          },
        );
      },
    );
    return () => {
      active = false;
      cancelAnimationFrame(frame);
      clearTimeout(timer);
      stop();
    };
  }, [attempt]);
  const reload = useCallback(() => {
    setError(null);
    setAttempt((value) => value + 1);
  }, []);
  return { tasks, error, reload };
}

/** 只要任务数组的简化版本：加载中和失败时为空数组。 */
export function useTasks(): TaskSnapshot[] {
  return useTaskList().tasks ?? EMPTY_TASKS;
}

const EMPTY_TASKS: TaskSnapshot[] = [];
