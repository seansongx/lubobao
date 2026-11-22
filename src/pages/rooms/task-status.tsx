import { AlertTriangle, Bell } from "lucide-react";

import type { DisplayStatus, TaskSnapshot } from "@/lib/api";
import { cn } from "@/lib/utils";
import { statusLabels } from "@/pages/rooms/format";

/** 状态图标：颜色之外都有形状区分，并始终配文字。 */
export function StatusMark({ status, className }: { status: DisplayStatus; className?: string }) {
  switch (status) {
    case "recording":
      return (
        <span className={cn("relative inline-flex size-[9px] shrink-0", className)} aria-hidden>
          <span className="absolute inset-0 animate-ping rounded-full bg-status-recording opacity-60 [animation-duration:1.6s] motion-reduce:animate-none" />
          <span className="relative size-full rounded-full bg-status-recording" />
        </span>
      );
    case "checking":
      return (
        <span
          className={cn(
            "inline-block size-[10px] shrink-0 animate-spin rounded-full border-2 border-status-checking border-r-transparent motion-reduce:animate-none",
            className,
          )}
          aria-hidden
        />
      );
    case "waiting":
      return (
        <span
          className={cn("inline-block size-[10px] shrink-0 rounded-full border-2 border-status-waiting", className)}
          aria-hidden
        />
      );
    case "retrying":
      return <span className={cn("inline-block size-[9px] shrink-0 rounded-full bg-status-retrying", className)} aria-hidden />;
    case "notify":
      return <Bell className={cn("size-3.5 shrink-0 text-status-notify", className)} aria-hidden />;
    case "needsAction":
      return <AlertTriangle className={cn("size-3.5 shrink-0 text-status-recording", className)} aria-hidden />;
    case "paused":
      return <span className={cn("inline-block size-2 shrink-0 rounded-[2px] bg-status-waiting", className)} aria-hidden />;
  }
}

const textClass: Record<DisplayStatus, string> = {
  recording: "font-medium text-status-recording",
  checking: "text-status-checking",
  waiting: "text-muted-foreground",
  retrying: "text-status-retrying",
  notify: "text-status-notify",
  needsAction: "font-medium text-status-recording",
  paused: "text-muted-foreground",
};

export function statusText(task: TaskSnapshot): string {
  if (task.display === "retrying" && task.live.state === "checkFailed") {
    return `重试中 · 第 ${task.live.attempt} 次`;
  }
  if (task.display === "recording" && task.record.state === "finishing") return "录制中 · 收尾";
  // 手动停止后仍在直播：后端按仅通知显示，文字说明是用户停止的。
  if (task.display === "notify" && task.recordHeld && !task.notifyOnly) return "已开播 · 已停止录制";
  return statusLabels[task.display];
}

export function TaskStatus({ task }: { task: TaskSnapshot }) {
  return (
    <span
      className={cn("inline-flex min-w-0 items-center gap-[7px] text-[12.5px]", textClass[task.display])}
      title={task.displayReason ?? undefined}
    >
      <StatusMark status={task.display} />
      <span className="truncate">{statusText(task)}</span>
    </span>
  );
}
