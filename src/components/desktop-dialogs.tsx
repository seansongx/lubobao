import { Loader2Icon } from "lucide-react";
import { useEffect, useId, useState, type MouseEvent } from "react";

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { api, type QuitState } from "@/lib/api";

/** 桌面外壳的对话框：退出确认与收尾提示、关窗三选一、托盘「全部暂停」确认。 */
export function DesktopDialogs() {
  return (
    <>
      <QuitDialog />
      <CloseDialog />
      <PauseAllDialog />
    </>
  );
}

/**
 * 退出确认与收尾提示。有录制时从退出入口（托盘「退出」、macOS 菜单「退出录播宝」、关窗选择
 * 退出）发起，Rust 发出 quit-state（confirming）；确认后进入 finishing，Rust 停止录制、等文件
 * 收尾后退出。
 */
function QuitDialog() {
  const [state, setState] = useState<QuitState>({ phase: "idle", recording: 0 });

  useEffect(() => {
    let disposed = false;
    const unlisten = api.onQuitState((next) => {
      if (!disposed) setState(next);
    });
    api
      .getQuitState()
      .then((next) => {
        if (!disposed) setState(next);
      })
      .catch(() => {});
    return () => {
      disposed = true;
      void unlisten.then((stop) => stop());
    };
  }, []);

  const finishing = state.phase === "finishing";

  function cancel() {
    if (finishing) return;
    setState((previous) => ({ ...previous, phase: "idle" }));
    api
      .cancelQuit()
      .then(setState)
      .catch(() => {});
  }

  function confirm(event: MouseEvent) {
    // 不让 Action 自己关闭对话框（会被当成取消）；收尾状态由 quit-state 推送。
    event.preventDefault();
    setState((previous) => ({ ...previous, phase: "finishing" }));
    void api.confirmQuit().catch(() => {});
  }

  return (
    <AlertDialog
      open={state.phase !== "idle"}
      onOpenChange={(open) => {
        if (!open) cancel();
      }}
    >
      <AlertDialogContent onEscapeKeyDown={(event) => finishing && event.preventDefault()}>
        {finishing ? (
          <AlertDialogHeader>
            <AlertDialogTitle className="flex items-center gap-2">
              <Loader2Icon className="size-4 animate-spin text-primary" aria-hidden />
              正在收尾…
            </AlertDialogTitle>
            <AlertDialogDescription>
              {state.recording > 0
                ? `正在停止 ${state.recording} 个录制并保存文件，完成后自动退出。`
                : "正在保存状态，完成后自动退出。"}
            </AlertDialogDescription>
          </AlertDialogHeader>
        ) : (
          <>
            <AlertDialogHeader>
              <AlertDialogTitle>退出录播宝？</AlertDialogTitle>
              <AlertDialogDescription>
                {state.recording > 0
                  ? `有 ${state.recording} 个录制正在进行，退出前会正常收尾以保证文件可播放。`
                  : "录制已经结束，可以直接退出。"}
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>取消</AlertDialogCancel>
              <AlertDialogAction onClick={confirm}>退出</AlertDialogAction>
            </AlertDialogFooter>
          </>
        )}
      </AlertDialogContent>
    </AlertDialog>
  );
}

/**
 * 关闭主窗口时的三选一（设置 close_window_action 为 ask 时）：最小化到托盘、退出、取消。
 * 勾选「记住我的选择」后写入设置，可在设置页「托盘与通知」改回。
 */
function CloseDialog() {
  const [open, setOpen] = useState(false);
  const [remember, setRemember] = useState(false);
  const rememberId = useId();

  useEffect(() => {
    const unlisten = api.onClosePrompt(() => {
      setRemember(false);
      setOpen(true);
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  function choose(choice: "tray" | "quit") {
    setOpen(false);
    void api.resolveClose(choice, remember).catch(() => {});
  }

  return (
    <AlertDialog open={open} onOpenChange={setOpen}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>关闭录播宝窗口</AlertDialogTitle>
          <AlertDialogDescription>
            最小化到托盘后，录播宝在后台继续检测和录制，可从托盘图标重新打开或退出。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <div className="flex items-center gap-2">
          <Checkbox id={rememberId} checked={remember} onCheckedChange={(value) => setRemember(value === true)} />
          <Label htmlFor={rememberId} className="text-sm font-normal">
            记住我的选择
          </Label>
        </div>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <Button variant="outline" onClick={() => choose("quit")}>
            退出录播宝
          </Button>
          <Button onClick={() => choose("tray")}>最小化到托盘</Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** 有录制时点托盘「全部暂停监控」的确认：暂停会同时停止录制。 */
function PauseAllDialog() {
  const [recording, setRecording] = useState<number | null>(null);

  useEffect(() => {
    const unlisten = api.onPauseAllPrompt((prompt) => setRecording(prompt.recording));
    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  return (
    <AlertDialog
      open={recording !== null}
      onOpenChange={(open) => {
        if (!open) setRecording(null);
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>全部暂停监控？</AlertDialogTitle>
          <AlertDialogDescription>
            {`有 ${recording ?? 0} 个录制正在进行，暂停后会停止录制并保存文件。之后可从托盘「全部恢复监控」恢复这些直播间。`}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction onClick={() => void api.confirmPauseAll().catch(() => {})}>全部暂停</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
