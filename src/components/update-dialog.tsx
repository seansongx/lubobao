// 应用内更新：检查、显示新版本和更新说明、下载进度、有录制时确认、收尾后安装并重启。
// 流程状态放在模块级 store 里，设置 →「关于」的「检查更新」按钮和启动后的自动检查共用。
// 下载、签名校验、收尾和安装都在 Rust（src-tauri/src/desktop/update.rs）。

import { useEffect, useSyncExternalStore } from "react";
import { AlertTriangleIcon, ExternalLink, Loader2Icon, ShieldAlertIcon } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Progress } from "@/components/ui/progress";
import {
  api,
  WEBSITE_URL,
  type QuitState,
  type UpdateError,
  type UpdateInfo,
  type UpdateInstallMode,
  type UpdateProgress,
} from "@/lib/api";
import { Markdown } from "@/pages/settings/markdown";

/** 启动后多久自动检查一次。 */
const AUTO_CHECK_DELAY_MS = 10_000;
/** 之后每隔多久再检查一次（常驻托盘的用户也能收到更新）。 */
const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

export type UpdateFlow =
  | { step: "idle" }
  | { step: "checking"; manual: boolean }
  | { step: "available"; update: UpdateInfo; install: UpdateInstallMode }
  | { step: "downloading"; update: UpdateInfo; progress: UpdateProgress | null }
  /** 下载并校验完成，有录制在进行，等用户确认收尾后安装。 */
  | { step: "confirm"; update: UpdateInfo; recording: number }
  | { step: "installing"; update: UpdateInfo }
  | { step: "failed"; update: UpdateInfo; error: UpdateError };

let flow: UpdateFlow = { step: "idle" };
const listeners = new Set<() => void>();

function setFlow(next: UpdateFlow) {
  flow = next;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useUpdateFlow() {
  return useSyncExternalStore(subscribe, () => flow);
}

function asUpdateError(error: unknown): UpdateError {
  if (typeof error === "object" && error !== null && "kind" in error && "message" in error) {
    return error as UpdateError;
  }
  return { kind: "network", message: String(error) };
}

/** 是否正在检查、下载或安装（这时不再接受新的检查）。 */
export function updateBusy(current: UpdateFlow = flow) {
  return current.step === "checking" || current.step === "downloading" || current.step === "installing";
}

/**
 * 检查更新。manual（「检查更新」按钮）时没有新版本提示「已是最新」，出错时提示原因；自动检查
 * 只在发现新版本时打扰用户。
 */
export async function checkForUpdates({ manual }: { manual: boolean }) {
  // 自动检查不打断已经打开的对话框；手动检查重新检查一次。
  if (updateBusy() || (!manual && flow.step !== "idle")) return;
  setFlow({ step: "checking", manual });
  try {
    const result = await api.checkUpdate();
    if (result.status === "upToDate") {
      setFlow({ step: "idle" });
      if (manual) toast.success(`已是最新版本（v${result.currentVersion}）`);
      return;
    }
    setFlow({ step: "available", update: result.update, install: result.install });
  } catch (error) {
    setFlow({ step: "idle" });
    const { kind, message } = asUpdateError(error);
    if (manual) {
      toast.error(kind === "network" ? `检查更新失败：${message}` : message);
    } else {
      console.warn("自动检查更新失败", kind, message);
    }
  }
}

async function startDownload(update: UpdateInfo) {
  setFlow({ step: "downloading", update, progress: null });
  const unlisten = api.onUpdateProgress((progress) => {
    if (flow.step === "downloading") setFlow({ ...flow, progress });
  });
  try {
    await api.downloadUpdate();
  } catch (error) {
    setFlow({ step: "failed", update, error: asUpdateError(error) });
    return;
  } finally {
    void unlisten.then((stop) => stop());
  }
  // 用户在下载时关了对话框：安装包留在后端，不自动安装，下次点「下载并安装」直接用。
  if (flow.step !== "downloading") {
    toast.info(`新版本 v${update.version} 已下载，可在 设置 → 关于 检查更新后安装`);
    return;
  }
  // 下载并校验完成：有录制时先确认，没有就直接收尾安装。
  const recording = await api
    .getSchedulerStatus()
    .then((status) => status.recording)
    .catch(() => 0);
  if (recording > 0) {
    setFlow({ step: "confirm", update, recording });
  } else {
    await install(update);
  }
}

async function install(update: UpdateInfo) {
  setFlow({ step: "installing", update });
  try {
    // 成功时应用随即退出（Windows 由安装器装好后再打开新版本，macOS、Linux 自动重启）。
    await api.installUpdate();
  } catch (error) {
    setFlow({ step: "failed", update, error: asUpdateError(error) });
  }
}

/** 安装中，或安装失败（录制已停止，只能重启）时对话框不能关闭。 */
function locked(current: UpdateFlow = flow) {
  return current.step === "installing" || (current.step === "failed" && current.error.kind === "install");
}

function later() {
  if (locked()) return;
  setFlow({ step: "idle" });
}

function formatBytes(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(0, Math.round(bytes / 1024))} KB`;
}

function formatDate(value: string | null) {
  if (!value) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date.toLocaleDateString("zh-CN");
}

/**
 * 挂在 App 上：按设置在启动约 10 秒后自动检查一次，之后每 24 小时再检查一次，发现新版本时
 * 弹出对话框。每次检查前重新读取设置，开关随时生效。
 */
export function UpdateDialog() {
  const current = useUpdateFlow();

  useEffect(() => {
    const autoCheck = () => {
      api
        .getSettings()
        .then((settings) => {
          if (settings.auto_check_update) void checkForUpdates({ manual: false });
        })
        .catch(() => {});
    };
    const timer = window.setTimeout(autoCheck, AUTO_CHECK_DELAY_MS);
    const interval = window.setInterval(autoCheck, AUTO_CHECK_INTERVAL_MS);
    return () => {
      window.clearTimeout(timer);
      window.clearInterval(interval);
    };
  }, []);

  const open = current.step !== "idle" && current.step !== "checking";
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) later();
      }}
    >
      {open ? <UpdateContent flow={current} /> : null}
    </Dialog>
  );
}

function UpdateContent({ flow: current }: { flow: Exclude<UpdateFlow, { step: "idle" } | { step: "checking" }> }) {
  const { update } = current;
  const fixed = locked(current);
  const title =
    current.step === "failed" && current.error.kind === "signature"
      ? "已拒绝安装更新"
      : current.step === "failed" && current.error.kind === "install"
        ? "安装更新失败"
        : `发现新版本 v${update.version}`;
  return (
    <DialogContent
      showCloseButton={!fixed}
      onEscapeKeyDown={(event) => fixed && event.preventDefault()}
      onInteractOutside={(event) => event.preventDefault()}
      className="sm:max-w-(--dialog-width)"
    >
      <DialogHeader>
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>
          当前版本 v{update.currentVersion}
          {formatDate(update.pubDate) ? ` · 发布于 ${formatDate(update.pubDate)}` : ""}
        </DialogDescription>
      </DialogHeader>

      {update.mandatory && current.step !== "failed" ? (
        <div
          role="note"
          className="flex items-start gap-2 rounded-md border border-primary/40 bg-primary/10 px-3 py-2 text-(length:--text-dense) text-foreground"
        >
          <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-primary" aria-hidden />
          <span>
            <strong className="font-semibold">重要更新：</strong>
            这个版本修复了影响使用的问题，建议尽快安装。
          </span>
        </div>
      ) : null}

      {current.step === "available" || current.step === "downloading" ? (
        <div className="max-h-[260px] overflow-y-auto rounded-md border bg-muted/40 px-3 py-2.5">
          {update.notes ? (
            <Markdown source={update.notes} restrictLinks />
          ) : (
            <p className="m-0 text-(length:--text-dense) text-muted-foreground">这个版本没有填写更新说明。</p>
          )}
        </div>
      ) : null}

      <UpdateStep flow={current} />
    </DialogContent>
  );
}

function UpdateStep({ flow: current }: { flow: Exclude<UpdateFlow, { step: "idle" } | { step: "checking" }> }) {
  switch (current.step) {
    case "available":
      if (current.install === "manual") {
        return (
          <>
            <p className="m-0 text-(length:--text-dense) text-muted-foreground">当前安装方式（例如 deb 包）不支持应用内更新，请到官网下载新版本。</p>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={later}>
                稍后
              </Button>
              <Button
                type="button"
                onClick={() => {
                  void api.openUrl(WEBSITE_URL);
                  later();
                }}
              >
                前往官网下载
                <ExternalLink aria-hidden />
              </Button>
            </DialogFooter>
          </>
        );
      }
      return (
        <DialogFooter>
          <Button type="button" variant="outline" onClick={later}>
            稍后
          </Button>
          <Button type="button" onClick={() => void startDownload(current.update)}>
            下载并安装
          </Button>
        </DialogFooter>
      );
    case "downloading": {
      const { progress } = current;
      const percent = progress?.total ? Math.min(100, (progress.downloaded / progress.total) * 100) : null;
      return (
        <>
          <div className="space-y-1.5">
            <Progress value={percent ?? 0} aria-label="下载进度" />
            <p className="m-0 text-xs text-muted-foreground tabular-nums">
              {progress && progress.downloaded > 0
                ? progress.total
                  ? `正在下载 ${formatBytes(progress.downloaded)} / ${formatBytes(progress.total)}（${Math.floor(percent ?? 0)}%）`
                  : `正在下载 ${formatBytes(progress.downloaded)}`
                : "正在连接下载服务器…"}
            </p>
            {progress?.fallback ? (
              <p className="m-0 text-xs text-muted-foreground">主线路下载失败，已改从备用线路下载。</p>
            ) : null}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={later}>
              隐藏
            </Button>
          </DialogFooter>
        </>
      );
    }
    case "confirm":
      return (
        <>
          <p className="m-0 text-(length:--text-dense)">
            新版本已下载并通过签名校验。有 {current.recording} 个录制正在进行，安装前会像退出一样正常收尾，保证文件可播放；安装完成后录播宝自动重启并恢复监控。
          </p>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={later}>
              稍后
            </Button>
            <Button type="button" onClick={() => void install(current.update)}>
              停止录制并安装
            </Button>
          </DialogFooter>
        </>
      );
    case "installing":
      return <InstallingNote />;
    case "failed":
      return <FailedStep update={current.update} error={current.error} />;
  }
}

/** 收尾和安装期间的说明。录制数跟随 quit-state（purpose 为 update）。 */
function InstallingNote() {
  const quit = useQuitRecording();
  return (
    <p className="m-0 flex items-start gap-2 text-(length:--text-dense)">
      <Loader2Icon className="mt-0.5 size-4 shrink-0 animate-spin text-primary" aria-hidden />
      <span>
        {quit.phase === "finishing" && quit.recording > 0
          ? `正在停止 ${quit.recording} 个录制并保存文件，完成后安装新版本并重启。`
          : "正在安装新版本，完成后录播宝会自动重启…"}
      </span>
    </p>
  );
}

function useQuitRecording(): QuitState {
  const quitState = useSyncExternalStore(subscribeQuit, () => quitSnapshot);
  return quitState;
}

let quitSnapshot: QuitState = { phase: "idle", recording: 0, purpose: "quit" };
const quitListeners = new Set<() => void>();
let quitUnlisten: Promise<() => void> | null = null;

function subscribeQuit(listener: () => void) {
  quitListeners.add(listener);
  if (!quitUnlisten) {
    quitUnlisten = api.onQuitState((next) => {
      quitSnapshot = next;
      for (const item of quitListeners) item();
    });
    void api
      .getQuitState()
      .then((next) => {
        quitSnapshot = next;
        for (const item of quitListeners) item();
      })
      .catch(() => {});
  }
  return () => {
    quitListeners.delete(listener);
    if (quitListeners.size === 0 && quitUnlisten) {
      void quitUnlisten.then((stop) => stop());
      quitUnlisten = null;
    }
  };
}

function FailedStep({ update, error }: { update: UpdateInfo; error: UpdateError }) {
  if (error.kind === "signature") {
    return (
      <>
        <div role="alert" className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-(length:--text-dense)">
          <ShieldAlertIcon className="mt-0.5 size-4 shrink-0 text-destructive" aria-hidden />
          <span>
            下载的安装包没有通过签名校验，可能被篡改或下载损坏，录播宝已拒绝安装，录制不受影响。请稍后再试，或到官网下载。
            <span className="mt-1 block break-all text-xs text-muted-foreground">{error.message}</span>
          </span>
        </div>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={later}>
            关闭
          </Button>
          <Button type="button" onClick={() => void api.openUrl(WEBSITE_URL)}>
            前往官网
            <ExternalLink aria-hidden />
          </Button>
        </DialogFooter>
      </>
    );
  }
  if (error.kind === "install") {
    // 录制已经收尾、调度已停止，录播宝不再检测和录制：不能关闭，只能重启。
    return (
      <>
        <div role="alert" className="space-y-1 text-(length:--text-dense)">
          <p className="m-0 font-medium text-destructive">新版本没有装上，已停止录制，重启录播宝后恢复监控。</p>
          <p className="m-0 break-all text-xs text-muted-foreground">{error.message}</p>
        </div>
        <DialogFooter>
          <Button
            type="button"
            onClick={() => {
              api.restartAfterFailedUpdate().catch((reason: unknown) => {
                toast.error(`无法重启：${asUpdateError(reason).message}`);
              });
            }}
          >
            重启录播宝
          </Button>
        </DialogFooter>
      </>
    );
  }
  return (
    <>
      <p role="alert" className="m-0 text-(length:--text-dense) text-destructive">
        {error.kind === "network" ? `下载失败：${error.message}` : error.message}
      </p>
      <DialogFooter>
        <Button type="button" variant="outline" onClick={later}>
          稍后
        </Button>
        <Button type="button" onClick={() => void startDownload(update)}>
          重试
        </Button>
      </DialogFooter>
    </>
  );
}
