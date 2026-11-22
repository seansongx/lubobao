import type { ReactNode } from "react";
import { Cpu, HardDrive, Timer, Wifi } from "lucide-react";

import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

interface StatusItemProps {
  hint: string;
  className?: string;
  children: ReactNode;
}

function StatusItem({ hint, className, children }: StatusItemProps) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className={cn("flex items-center gap-1.5 whitespace-nowrap", className)} tabIndex={0}>
          {children}
        </span>
      </TooltipTrigger>
      <TooltipContent side="top">{hint}</TooltipContent>
    </Tooltip>
  );
}

// 占位内容：M1/M2 接入 Rust 快照后替换为真实状态。
export function StatusBar() {
  return (
    <footer className="col-span-full flex h-7 items-center gap-3 border-t border-sidebar-border bg-sidebar px-3 text-xs text-muted-foreground">
      <StatusItem hint="FFmpeg 组件状态">
        <Cpu className="size-3.5" aria-hidden />
        FFmpeg 未检测
      </StatusItem>
      <Separator orientation="vertical" className="h-3.5!" />
      <StatusItem hint="正在录制的任务数">
        <span className="size-[7px] rounded-full bg-status-recording" aria-hidden />
        录制中 <span className="tabular-nums">0</span>
      </StatusItem>
      <Separator orientation="vertical" className="h-3.5!" />
      <StatusItem hint="所有录制任务的总码率">
        <Wifi className="size-3.5" aria-hidden />
        <span className="tabular-nums">0.0 Mbps</span>
      </StatusItem>
      <Separator orientation="vertical" className="h-3.5!" />
      <StatusItem hint="保存目录所在磁盘的剩余空间">
        <HardDrive className="size-3.5" aria-hidden />
        磁盘余量 —
      </StatusItem>
      <StatusItem hint="距离下一轮开播检测的时间" className="ml-auto">
        <Timer className="size-3.5" aria-hidden />
        下次检测 —
      </StatusItem>
    </footer>
  );
}
