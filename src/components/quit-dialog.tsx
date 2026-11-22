import { Loader2Icon } from "lucide-react";
import { useEffect, useState, type MouseEvent } from "react";

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
import { api, type QuitState } from "@/lib/api";

/**
 * 退出确认与收尾提示。有录制时托盘「退出」或 Cmd+Q 由 Rust 发出 quit-state（confirming），
 * 确认后进入 finishing，Rust 停止录制、等文件收尾后退出。
 */
export function QuitDialog() {
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
