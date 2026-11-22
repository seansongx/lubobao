import { useState, type ReactNode } from "react";
import { AlertTriangle, Cpu, HardDrive, Loader2, RotateCw, Timer, Wifi } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { api, type FfmpegStatus } from "@/lib/api";
import { useFfmpegStatus, useTasks } from "@/lib/use-backend";
import { cn } from "@/lib/utils";

interface StatusItemProps {
  hint: ReactNode;
  className?: string;
  children: ReactNode;
}

function StatusItem({ hint, className, children }: StatusItemProps) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className={cn("flex min-w-0 items-center gap-1.5 whitespace-nowrap", className)} tabIndex={0}>
          {children}
        </span>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-sm whitespace-normal">
        {hint}
      </TooltipContent>
    </Tooltip>
  );
}

function formatMiB(bytes: number) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

interface FfmpegView {
  label: string;
  hint: string;
  busy?: boolean;
  tone?: "error" | "warning";
  retry?: boolean;
}

function describeFfmpeg(status: FfmpegStatus | null): FfmpegView {
  if (!status) return { label: "FFmpeg 检查中", hint: "正在读取 FFmpeg 组件状态", busy: true };
  switch (status.state) {
    case "checking":
      return { label: "FFmpeg 检查中", hint: "正在检查本地已安装的版本", busy: true };
    case "notInstalled":
      return { label: "FFmpeg 准备下载", hint: "本地没有可用版本，即将开始下载", busy: true };
    case "waiting":
      return { label: "FFmpeg 等待安装", hint: "另一个录播宝窗口正在安装 FFmpeg，完成后自动使用", busy: true };
    case "downloading": {
      const percent = status.total > 0 ? Math.floor((status.downloaded / status.total) * 100) : 0;
      return {
        label: `FFmpeg 下载中 ${percent}%`,
        hint: `已下载 ${formatMiB(status.downloaded)} / ${formatMiB(status.total)}，来源 ${status.source}`,
        busy: true,
      };
    }
    case "verifying":
      return { label: "FFmpeg 校验中", hint: "正在校验并安装下载的文件", busy: true };
    case "ready":
      if (status.installError) {
        // 旧版本可用，新版本安装失败：提示原因并允许重试。
        return {
          label: `FFmpeg ${status.ffmpegVersion}（旧版本）`,
          hint: `正在使用旧版本 ${status.ffmpegVersion}。新版本安装失败：${status.installError}`,
          tone: "warning",
          retry: true,
        };
      }
      if (status.outdated) {
        // 旧版本可用，新版本正在后台安装，属于正常状态。
        return {
          label: `FFmpeg ${status.ffmpegVersion}（正在更新）`,
          hint: `正在使用旧版本 ${status.ffmpegVersion}，新版本在后台安装，完成后自动切换`,
        };
      }
      return { label: `FFmpeg ${status.ffmpegVersion}`, hint: `FFmpeg 可用：${status.path}` };
    case "failed":
      return {
        label: `FFmpeg 不可用：${status.reason}`,
        hint: status.reason,
        tone: "error",
        retry: status.retryable,
      };
  }
}

function FfmpegItem() {
  const status = useFfmpegStatus();
  const [retrying, setRetrying] = useState(false);
  const view = describeFfmpeg(status);

  const retry = () => {
    setRetrying(true);
    void api.retryFfmpegInstall().finally(() => setRetrying(false));
  };

  return (
    <span className="flex min-w-0 items-center gap-1.5">
      <StatusItem
        hint={view.hint}
        className={cn(
          view.tone === "error" && "text-destructive",
          view.tone === "warning" && "text-status-retrying",
        )}
      >
        {view.busy ? (
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
        ) : view.tone ? (
          <AlertTriangle className="size-3.5" aria-hidden />
        ) : (
          <Cpu className="size-3.5" aria-hidden />
        )}
        <span className="max-w-[22rem] truncate">{view.label}</span>
      </StatusItem>
      {view.retry && (
        <Button variant="ghost" size="xs" className="h-5 px-1.5" onClick={retry} disabled={retrying}>
          <RotateCw aria-hidden />
          重试
        </Button>
      )}
    </span>
  );
}

export function StatusBar() {
  const tasks = useTasks();
  const recording = tasks.filter((task) => task.display === "recording");
  const bitrateKbps = tasks.reduce(
    (sum, task) => sum + (task.record.state === "recording" ? task.record.bitrateKbps : 0),
    0,
  );

  return (
    <footer className="col-span-full flex h-7 items-center gap-3 border-t border-sidebar-border bg-sidebar px-3 text-xs text-muted-foreground">
      <FfmpegItem />
      <Separator orientation="vertical" className="h-3.5!" />
      <StatusItem hint="正在录制的任务数">
        <span className="size-[7px] rounded-full bg-status-recording" aria-hidden />
        录制中 <span className="tabular-nums">{recording.length}</span>
      </StatusItem>
      <Separator orientation="vertical" className="h-3.5!" />
      <StatusItem hint="所有录制任务的总码率">
        <Wifi className="size-3.5" aria-hidden />
        <span className="tabular-nums">{(bitrateKbps / 1000).toFixed(1)} Mbps</span>
      </StatusItem>
      <Separator orientation="vertical" className="h-3.5!" />
      {/* 磁盘余量和下次检测在录制引擎、轮询调度接入后提供。 */}
      <StatusItem hint="保存目录所在磁盘的剩余空间（尚未接入）">
        <HardDrive className="size-3.5" aria-hidden />
        磁盘余量 —
      </StatusItem>
      <StatusItem hint="距离下一轮开播检测的时间（尚未接入）" className="ml-auto">
        <Timer className="size-3.5" aria-hidden />
        下次检测 —
      </StatusItem>
    </footer>
  );
}
