import { memo, useEffect, useState, type ReactNode } from "react";
import { ExternalLink, FolderOpen, Loader2, Pause, Pencil, Play, Square, Trash2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { api, type ActivityEntry, type ActivityKind, type TaskId, type TaskSnapshot } from "@/lib/api";
import { cn } from "@/lib/utils";
import { AnchorAvatar, PlatformTag } from "@/pages/rooms/anchor";
import {
  describeLive,
  describeRecord,
  formatBitrate,
  formatBytes,
  formatClock,
  formatDateTime,
  formatDuration,
  isRecordingActive,
  qualityLabels,
} from "@/pages/rooms/format";
import { Notice } from "@/pages/rooms/notice";
import { openFolder, openInBrowser, setMonitoring, stopRecording } from "@/pages/rooms/task-actions";
import { TaskStatus } from "@/pages/rooms/task-status";
import { useNow } from "@/pages/rooms/use-now";

const ACTIVITY_LIMIT = 20;

type ActivityState = { entries: ActivityEntry[] | null; failed: boolean };

/** 订阅某个任务的最近动态：先取一页，再把推送的新动态叠加到前面。订阅或读取失败时标记 failed。 */
function useTaskActivity(taskId: number): ActivityState {
  const [entries, setEntries] = useState<ActivityEntry[] | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    let pending: ActivityEntry[] | null = [];
    setEntries(null);
    setFailed(false);
    const fail = () => {
      if (!active) return;
      setFailed(true);
      setEntries((current) => current ?? []);
    };
    const merge = (incoming: ActivityEntry[], base: ActivityEntry[]) => {
      const seen = new Set(base.map((entry) => entry.id));
      return [...incoming.filter((entry) => !seen.has(entry.id)).reverse(), ...base].slice(0, ACTIVITY_LIMIT);
    };
    const unlistenPromise = api.onActivity((entry) => {
      if (entry.taskId !== taskId) return;
      if (pending) pending.push(entry);
      else setEntries((current) => merge([entry], current ?? []));
    });
    // 监听注册失败也继续读取一页，只是不再有实时更新。
    void unlistenPromise
      .catch(() => undefined)
      .then(() =>
        api.listActivity({ taskId, limit: ACTIVITY_LIMIT }).then((page) => {
          if (!active) return;
          const buffered = pending ?? [];
          pending = null;
          setEntries(merge(buffered, page.items));
        }, fail),
      );
    void unlistenPromise.catch(fail);
    return () => {
      active = false;
      void unlistenPromise.then(
        (unlisten) => unlisten(),
        () => undefined,
      );
    };
  }, [taskId]);
  return { entries, failed };
}

/** 已录时长：只有这一处随时钟刷新。 */
function Elapsed({ since }: { since: number }) {
  const now = useNow();
  return <>{formatDuration(now - since)}</>;
}

const activityDot: Record<ActivityKind, string> = {
  live_start: "bg-primary",
  live_end: "bg-status-waiting",
  record_start: "bg-status-recording",
  record_end: "bg-status-waiting",
  failure: "bg-status-recording",
  warning: "bg-status-retrying",
  user_action: "bg-muted-foreground/50",
};

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <h3 className="m-0 mb-2 text-xs font-semibold text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}

function StateCell({ label, value, reason }: { label: string; value: string; reason: string | null }) {
  return (
    <div className="min-w-0 rounded-md border px-2.5 py-1.5">
      <small className="block text-[11px] text-muted-foreground">{label}</small>
      <b className="block truncate text-[12.5px] font-medium">{value}</b>
      {reason && <span className="mt-0.5 block text-[11px] leading-snug break-words text-muted-foreground">{reason}</span>}
    </div>
  );
}

function KeyValues({ rows }: { rows: [string, ReactNode, string?][] }) {
  return (
    <dl className="m-0 grid grid-cols-[64px_minmax(0,1fr)] gap-y-1.5 text-[12.5px]">
      {rows.map(([key, value, title]) => (
        <div key={key} className="contents">
          <dt className="text-muted-foreground">{key}</dt>
          <dd className="m-0 truncate tabular-nums" title={title}>
            {value}
          </dd>
        </div>
      ))}
    </dl>
  );
}

interface TaskInspectorProps {
  task: TaskSnapshot;
  onClose: () => void;
  onEdit: (id: TaskId) => void;
  onDelete: (tasks: TaskSnapshot[]) => void;
}

/** 回调由页面用 useCallback 固定，只有当前任务的快照变化时才重渲染。 */
export const TaskInspector = memo(function TaskInspector({ task, onClose, onEdit, onDelete }: TaskInspectorProps) {
  const { entries: activity, failed: activityFailed } = useTaskActivity(task.id);
  const [busy, setBusy] = useState<"stop" | "monitor" | null>(null);
  const recording = isRecordingActive(task.record);
  const paused = task.monitoring === "paused";
  const live = describeLive(task.live);
  const record = describeRecord(task);

  async function run(kind: "stop" | "monitor", action: () => Promise<unknown>) {
    setBusy(kind);
    try {
      await action();
    } finally {
      setBusy(null);
    }
  }

  return (
    <aside
      className="flex w-[300px] shrink-0 flex-col border-l bg-card xl:w-[340px]"
      aria-label={`${task.anchorName} 详情`}
    >
      <header className="flex items-center gap-2.5 border-b px-3.5 pt-3.5 pb-3">
        <AnchorAvatar task={task} className="size-[34px] text-sm" />
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-semibold" title={task.anchorName}>
            {task.anchorName}
          </div>
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <PlatformTag task={task} />
            <span className="truncate tabular-nums">{task.roomId}</span>
          </div>
        </div>
        <Button variant="ghost" size="icon-sm" title="在浏览器打开直播间" aria-label="在浏览器打开直播间" onClick={() => void openInBrowser(task)}>
          <ExternalLink aria-hidden />
        </Button>
        <Button variant="ghost" size="icon-sm" title="关闭详情" aria-label="关闭详情" onClick={onClose}>
          <X aria-hidden />
        </Button>
      </header>

      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-3.5">
        <div className="flex items-center justify-between gap-2">
          <TaskStatus task={task} />
        </div>

        {task.displayReason && (
          // 后端给出原因时一律显示：需处理、重试中，以及手动停止后仍在播等情况。
          <Notice
            tone={task.display === "needsAction" ? "error" : task.display === "retrying" ? "warning" : "info"}
            title={task.display === "needsAction" ? "需要处理" : task.display === "retrying" ? "正在重试" : "说明"}
          >
            {task.displayReason}
          </Notice>
        )}

        <Section title="状态">
          <div className="grid grid-cols-3 gap-1.5">
            <StateCell label="监控" value={paused ? "已暂停" : "已启用"} reason={paused && recording ? "当前录制结束后不再检测" : null} />
            <StateCell label="直播" value={live.value} reason={live.reason} />
            <StateCell label="录制" value={record.value} reason={record.reason} />
          </div>
        </Section>

        {(task.record.state === "recording" || task.record.state === "finishing") && (
          <Section title="当前录制">
            <KeyValues
              rows={[
                ["文件", task.record.filePath.split(/[\\/]/).pop() ?? task.record.filePath, task.record.filePath],
                ["位置", task.record.filePath, task.record.filePath],
                ["已录", <Elapsed since={task.record.startedAt} />, `开始于 ${formatDateTime(task.record.startedAt)}`],
                ...(task.record.state === "recording"
                  ? ([
                      ["大小", formatBytes(task.record.bytesWritten)],
                      ["码率", formatBitrate(task.record.bitrateKbps)],
                    ] as [string, ReactNode][])
                  : []),
              ]}
            />
          </Section>
        )}

        <Section title="任务设置">
          <KeyValues
            rows={[
              ["画质", qualityLabels[task.quality]],
              ["录制方式", task.notifyOnly ? "仅通知，不录制" : "开播自动录制"],
              ["链接", task.url, task.url],
              ...(task.note ? ([["备注", task.note, task.note]] as [string, ReactNode, string][]) : []),
              ["添加于", formatDateTime(task.createdAt)],
            ]}
          />
        </Section>

        <Section title="最近动态">
          {activity === null ? (
            <p className="m-0 text-xs text-muted-foreground">正在读取…</p>
          ) : activity.length === 0 ? (
            <p className="m-0 text-xs text-muted-foreground">{activityFailed ? "动态读取失败" : "暂无动态"}</p>
          ) : (
            <ul className="m-0 flex list-none flex-col gap-2.5 p-0">
              {activity.map((entry) => (
                <li key={entry.id} className="grid grid-cols-[40px_8px_minmax(0,1fr)] items-start gap-2 text-[12.5px]">
                  <time className="pt-px text-[11.5px] text-muted-foreground tabular-nums" title={formatDateTime(entry.at)}>
                    {formatClock(entry.at)}
                  </time>
                  <span className={cn("mt-1.5 size-2 rounded-full", activityDot[entry.kind])} aria-hidden />
                  <span className="break-words">{entry.message}</span>
                </li>
              ))}
            </ul>
          )}
        </Section>
      </div>

      <footer className="grid grid-cols-2 gap-2 border-t px-3.5 py-3">
        {recording && (
          <Button
            variant="destructive"
            className="col-span-2"
            disabled={busy !== null || task.record.state === "finishing"}
            onClick={() => void run("stop", () => stopRecording(task))}
          >
            {task.record.state === "finishing" ? <Loader2 className="animate-spin" aria-hidden /> : <Square aria-hidden />}
            {task.record.state === "finishing" ? "正在收尾…" : "停止录制"}
          </Button>
        )}
        <Button
          variant={paused ? "default" : "outline"}
          disabled={busy !== null}
          onClick={() => void run("monitor", () => setMonitoring([task.id], paused))}
        >
          {paused ? <Play aria-hidden /> : <Pause aria-hidden />}
          {paused ? "恢复监控" : "暂停监控"}
        </Button>
        <Button variant="outline" onClick={() => onEdit(task.id)}>
          <Pencil aria-hidden />
          编辑
        </Button>
        <Button variant="outline" onClick={() => void openFolder(task)}>
          <FolderOpen aria-hidden />
          打开文件夹
        </Button>
        <Button variant="outline" className="text-destructive hover:text-destructive" onClick={() => onDelete([task])}>
          <Trash2 aria-hidden />
          删除
        </Button>
      </footer>
    </aside>
  );
});
