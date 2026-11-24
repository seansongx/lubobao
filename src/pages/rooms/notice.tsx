import type { ReactNode } from "react";
import { AlertTriangle, CheckCircle2, Info } from "lucide-react";

import { cn } from "@/lib/utils";

type Tone = "error" | "warning" | "success" | "info";

const toneClass: Record<Tone, string> = {
  error: "border-status-recording/35 bg-status-recording-soft [&>svg]:text-status-recording",
  warning: "border-status-retrying/35 bg-status-retrying/10 [&>svg]:text-status-retrying",
  success: "border-primary/30 bg-primary/10 [&>svg]:text-primary",
  info: "border-border bg-muted [&>svg]:text-muted-foreground",
};

const icons: Record<Tone, typeof Info> = {
  error: AlertTriangle,
  warning: AlertTriangle,
  success: CheckCircle2,
  info: Info,
};

/** 对话框和详情面板内的提示块。 */
export function Notice({
  tone,
  title,
  children,
  actions,
  className,
}: {
  tone: Tone;
  title: ReactNode;
  children?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  const Icon = icons[tone];
  return (
    <div
      role={tone === "error" ? "alert" : "status"}
      className={cn("flex gap-2.5 rounded-lg border px-3 py-2.5 text-(length:--text-dense) select-text", toneClass[tone], className)}
    >
      <Icon className="mt-0.5 size-4 shrink-0" aria-hidden />
      <div className="min-w-0 flex-1">
        <div className="font-medium wrap-anywhere">{title}</div>
        {children && <div className="mt-0.5 wrap-anywhere text-muted-foreground">{children}</div>}
        {actions && <div className="mt-2 flex flex-wrap gap-2">{actions}</div>}
      </div>
    </div>
  );
}
