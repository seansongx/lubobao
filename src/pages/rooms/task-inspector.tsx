import { memo, useEffect, useState, type ReactNode } from "react";
import { Circle, ExternalLink, FolderOpen, Loader2, Pause, Pencil, Play, Square, Trash2, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { withShortcut } from "@/lib/platform";
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
import {
  canRestartRecording,
  openFolder,
  openInBrowser,
  setMonitoring,
  startRecording,
  stopRecording,
} from "@/pages/rooms/task-actions";
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
  convert: "bg-primary",
};

function Section({ title, action, children }: { title: string; action?: ReactNode; children: ReactNode }) {
  return (
    <section>
      <div className="mb-2 flex items-center gap-2">
        <h3 className="m-0 flex-1 text-xs font-semibold text-muted-foreground win:text-[13px] win:text-foreground">{title}</h3>
        {action}
      </div>
      {children}
    </section>
  );
}

function StateCell({ label, value, reason }: { label: string; value: string; reason: string | null }) {
  return (
    <div className="min-w-0 rounded-md border border-black/5 bg-card px-2.5 py-1.5 dark:border-white/5 linux:rounded-lg">
      <small className="block text-[11px] text-muted-foreground">{label}</small>
      <b className="block truncate text-[12.5px] font-medium">{value}</b>
      {reason && <span className="mt-0.5 block text-[11px] leading-snug break-words text-muted-foreground">{reason}</span>}
    </div>
  );
}

function KeyValues({ rows }: { rows: [string, ReactNode, string?][] }) {
  return (
    <dl className="m-0 grid grid-cols-[64px_minmax(0,1fr)] gap-y-1.5 text-(length:--text-dense)">
      {rows.map(([key, value, title]) => (
        <div key={key} className="contents">
          <dt className="text-muted-foreground">{key}</dt>
          <dd className="m-0 truncate tabular-nums select-text" title={title}>
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
  /** 跳到动态页，只看这个任务。 */
  onShowActivity?: (id: TaskId) => void;
}

/** 回调由页面用 useCallback 固定，只有当前任务的快照变化时才重渲染。 */
export const TaskInspector = memo(function TaskInspector({ task, onClose, onEdit, onDelete, onShowActivity }: TaskInspectorProps) {
  const { entries: activity, failed: activityFailed } = useTaskActivity(task.id);
  const [busy, setBusy] = useState<"stop" | "start" | "monitor" | null>(null);
  const recording = isRecordingActive(task.record);
  const paused = task.monitoring === "paused";
  const canRestart = canRestartRecording(task);
  const live = describeLive(task.live);
  const record = describeRecord(task);

  async function run(kind: "stop" | "start" | "monitor", action: () => Promise<unknown>) {
    setBusy(kind);
    try {
      await action();
    } finally {
      setBusy(null);
    }
  }

  return (
    <aside
      className="flex w-[300px] shrink-0 flex-col border-l text-(length:--text-dense) xl:w-[340px] linux:bg-sidebar/50"
      aria-label={`${task.anchorName} 详情`}
    >
      <header className="flex items-center gap-2.5 px-4 pt-4 pb-3 mac:h-(--header-h) mac:border-b mac:py-0">
        <AnchorAvatar task={task} className="size-[34px] text-sm" />
        <div className="min-w-0 flex-1">
          <div className="truncate text-(length:--text-ui) font-semibold" title={task.anchorName}>
            {task.anchorName}
          </div>
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <PlatformTag task={task} />
            <span className="truncate tabular-nums">{task.roomId}</span>
          </div>
        </div>
        <IconAction label="在浏览器打开直播间" onClick={() => void openInBrowser(task)}>
          <ExternalLink aria-hidden />
        </IconAction>
        <IconAction label="关闭详情" hint={withShortcut("关闭详情", "Escape", false)} onClick={onClose}>
          <X aria-hidden />
        </IconAction>
      </header>

      <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto px-4 pt-1 pb-3.5 mac:pt-3.5">
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

        <Section
          title="最近动态"
          action={
            onShowActivity && (
              <Button variant="link" size="xs" className="h-auto p-0" onClick={() => onShowActivity(task.id)}>
                全部动态
              </Button>
            )
          }
        >
          {activity === null ? (
            <p className="m-0 text-xs text-muted-foreground">正在读取…</p>
          ) : activity.length === 0 ? (
            <p className="m-0 text-xs text-muted-foreground">{activityFailed ? "动态读取失败" : "暂无动态"}</p>
          ) : (
            <ul className="m-0 flex list-none flex-col gap-2.5 p-0">
              {activity.map((entry) => (
                <li key={entry.id} className="grid grid-cols-[40px_8px_minmax(0,1fr)] items-start gap-2">
                  <time className="pt-px text-[11.5px] text-muted-foreground tabular-nums" title={formatDateTime(entry.at)}>
                    {formatClock(entry.at)}
                  </time>
                  <span className={cn("mt-1.5 size-2 rounded-full", activityDot[entry.kind])} aria-hidden />
                  <span className="break-words select-text">{entry.message}</span>
                </li>
              ))}
            </ul>
          )}
        </Section>
      </div>

      <footer className="flex items-center gap-1.5 border-t px-4 py-3">
        {recording ? (
          <Button
            variant="destructive"
            className="flex-1"
            disabled={busy !== null || task.record.state === "finishing"}
            onClick={() => void run("stop", () => stopRecording(task))}
          >
            {task.record.state === "finishing" ? <Loader2 className="animate-spin" aria-hidden /> : <Square aria-hidden />}
            {task.record.state === "finishing" ? "正在收尾…" : "停止录制"}
          </Button>
        ) : canRestart ? (
          <Button className="flex-1" disabled={busy !== null} onClick={() => void run("start", () => startRecording(task))}>
            {busy === "start" ? <Loader2 className="animate-spin" aria-hidden /> : <Circle className="fill-current" aria-hidden />}
            重新开始录制
          </Button>
        ) : (
          <Button
            variant={paused ? "default" : "outline"}
            className="flex-1"
            disabled={busy !== null}
            onClick={() => void run("monitor", () => setMonitoring([task.id], paused))}
          >
            {paused ? <Play aria-hidden /> : <Pause aria-hidden />}
            {paused ? "恢复监控" : "暂停监控"}
          </Button>
        )}
        {(recording || canRestart) && (
          <IconAction
            label={paused ? "恢复监控" : "暂停监控"}
            disabled={busy !== null}
            onClick={() => void run("monitor", () => setMonitoring([task.id], paused))}
          >
            {paused ? <Play aria-hidden /> : <Pause aria-hidden />}
          </IconAction>
        )}
        <IconAction label="编辑" onClick={() => onEdit(task.id)}>
          <Pencil aria-hidden />
        </IconAction>
        <IconAction label="打开文件夹" onClick={() => void openFolder(task)}>
          <FolderOpen aria-hidden />
        </IconAction>
        <IconAction
          label="删除"
          hint={withShortcut("删除", "Delete", false)}
          className="text-destructive hover:text-destructive"
          onClick={() => onDelete([task])}
        >
          <Trash2 aria-hidden />
        </IconAction>
      </footer>
    </aside>
  );
});

/** 详情面板里的图标按钮：悬停提示写出名称和快捷键。 */
function IconAction({
  label,
  hint,
  className,
  disabled,
  onClick,
  children,
}: {
  label: string;
  hint?: string;
  className?: string;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={label} className={className} disabled={disabled} onClick={onClick}>
          {children}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{hint ?? label}</TooltipContent>
    </Tooltip>
  );
}
