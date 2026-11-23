// 组件分组：FFmpeg 的状态、版本和路径，“重新校验”和“重试安装”。状态来自 ffmpeg-status 事件。

import { useState } from "react";
import { AlertTriangle, CheckCircle2, Loader2, RotateCw, ShieldCheck, XCircle } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { api, isCommandError, type FfmpegStatus } from "@/lib/api";
import { useFfmpegStatus } from "@/lib/use-backend";
import { FieldRow, SettingsCard } from "@/pages/settings/fields";

type Tone = "busy" | "ok" | "warning" | "error";

interface StatusView {
  label: string;
  detail: string;
  tone: Tone;
  busy: boolean;
  canReverify: boolean;
  canRetry: boolean;
}

function formatMiB(bytes: number) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function describe(status: FfmpegStatus | null): StatusView {
  const busy = (label: string, detail: string): StatusView => ({
    label,
    detail,
    tone: "busy",
    busy: true,
    canReverify: false,
    canRetry: false,
  });
  if (!status) return busy("读取中", "正在读取 FFmpeg 组件状态");
  switch (status.state) {
    case "checking":
      return busy("检查中", "正在检查本地已安装的版本");
    case "notInstalled":
      return busy("未安装", "本地没有可用版本，即将开始下载");
    case "waiting":
      return busy("等待中", "另一个录播宝窗口正在安装 FFmpeg，完成后自动使用");
    case "downloading": {
      const percent = status.total > 0 ? Math.floor((status.downloaded / status.total) * 100) : 0;
      return busy(
        `下载中 ${percent}%`,
        `已下载 ${formatMiB(status.downloaded)} / ${formatMiB(status.total)}，来源 ${status.source}。下载期间可以继续使用其他功能。`,
      );
    }
    case "verifying":
      return busy("校验中", "正在计算 SHA-256 并与发布清单核对");
    case "ready":
      if (status.installError) {
        return {
          label: "旧版本可用",
          detail: `新版本安装失败：${status.installError}。正在使用旧版本 ${status.ffmpegVersion}，可以重试安装。`,
          tone: "warning",
          busy: false,
          canReverify: true,
          canRetry: true,
        };
      }
      if (status.outdated) {
        return {
          label: "正在更新",
          detail: `正在使用旧版本 ${status.ffmpegVersion}，新版本在后台安装，完成后自动切换`,
          tone: "busy",
          busy: true,
          canReverify: false,
          canRetry: false,
        };
      }
      return {
        label: "可用",
        detail: "已校验 SHA-256，断网时也能使用",
        tone: "ok",
        busy: false,
        canReverify: true,
        canRetry: false,
      };
    case "failed":
      return {
        label: status.retryable ? "安装失败" : "不可用",
        detail: status.reason,
        tone: "error",
        busy: false,
        canReverify: false,
        canRetry: status.retryable,
      };
  }
}

const toneIcon: Record<Tone, typeof Loader2> = {
  busy: Loader2,
  ok: CheckCircle2,
  warning: AlertTriangle,
  error: XCircle,
};

const toneVariant: Record<Tone, "secondary" | "default" | "outline" | "destructive"> = {
  busy: "secondary",
  ok: "default",
  warning: "outline",
  error: "destructive",
};

export function ComponentsSection() {
  const status = useFfmpegStatus();
  const view = describe(status);
  const [actionError, setActionError] = useState<string | null>(null);
  const Icon = toneIcon[view.tone];

  const run = async (action: () => Promise<FfmpegStatus>) => {
    setActionError(null);
    try {
      await action();
    } catch (error) {
      setActionError(isCommandError(error) ? error.message : String(error));
    }
  };

  const ready = status?.state === "ready" ? status : null;

  return (
    <SettingsCard title="FFmpeg" description="录制使用的组件，从国内镜像下载，备用源提供相同文件">
      <FieldRow label="状态" description={<span aria-live="polite">{view.detail}</span>}>
        <Badge variant={toneVariant[view.tone]}>
          <Icon className={view.tone === "busy" ? "animate-spin" : undefined} aria-hidden />
          {view.label}
        </Badge>
      </FieldRow>
      {status?.state === "downloading" ? (
        <div className="px-4 py-3">
          <Progress
            value={status.total > 0 ? (status.downloaded / status.total) * 100 : 0}
            aria-label="FFmpeg 下载进度"
          />
        </div>
      ) : null}
      <FieldRow label="版本">
        <span className="text-(length:--text-dense) tabular-nums">
          {ready ? `${ready.ffmpegVersion}（@seankit/ffmpeg ${ready.packageVersion}）` : "—"}
        </span>
      </FieldRow>
      <FieldRow label="路径" stacked>
        <code className="block min-w-0 flex-1 truncate rounded-md bg-muted px-2 py-1 font-mono text-xs select-text" title={ready?.path}>
          {ready ? ready.path : "尚未安装"}
        </code>
      </FieldRow>
      <FieldRow
        label="维护"
        description="重新校验会重新计算文件哈希，不一致时自动重新下载；安装失败时保留可用的旧版本"
        error={actionError ?? undefined}
      >
        <Button
          type="button"
          variant="outline"
          disabled={!view.canReverify}
          onClick={() => void run(() => api.reverifyFfmpeg())}
        >
          <ShieldCheck aria-hidden />
          重新校验
        </Button>
        <Button
          type="button"
          variant="outline"
          disabled={!view.canRetry}
          onClick={() => void run(() => api.retryFfmpegInstall())}
        >
          <RotateCw aria-hidden />
          重试安装
        </Button>
      </FieldRow>
    </SettingsCard>
  );
}
