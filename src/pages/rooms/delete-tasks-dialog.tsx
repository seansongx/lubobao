import { useEffect, useState } from "react";
import { Loader2, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { api, type TaskId, type TaskSnapshot } from "@/lib/api";
import { describeError, type ErrorView } from "@/pages/rooms/format";
import { Notice } from "@/pages/rooms/notice";

interface DeleteTasksDialogProps {
  /** 待删除的任务；null 表示关闭。 */
  tasks: TaskSnapshot[] | null;
  onOpenChange: (open: boolean) => void;
  onDeleted: (ids: TaskId[]) => void;
}

export function DeleteTasksDialog({ tasks, onOpenChange, onDeleted }: DeleteTasksDialogProps) {
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<ErrorView | null>(null);
  const open = tasks !== null && tasks.length > 0;

  useEffect(() => {
    if (open) {
      setError(null);
      setDeleting(false);
    }
  }, [open]);

  const list = tasks ?? [];
  const names = list.slice(0, 5).map((task) => task.anchorName).join("、");
  const more = list.length > 5 ? ` 等 ${list.length} 个任务` : "";

  async function confirm() {
    setDeleting(true);
    setError(null);
    try {
      const removed = await api.deleteTasks(list.map((task) => task.id));
      onDeleted(removed);
      toast.success(`已删除 ${removed.length} 个任务`);
      onOpenChange(false);
    } catch (reason) {
      // 后端拒绝时整批不删除，保持对话框打开并显示原因。
      setError(describeError(reason));
    } finally {
      setDeleting(false);
    }
  }

  return (
    <AlertDialog open={open} onOpenChange={(next) => !deleting && onOpenChange(next)}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>删除 {list.length === 1 ? list[0].anchorName : `${list.length} 个任务`}？</AlertDialogTitle>
          <AlertDialogDescription>
            {list.length > 1 && <span className="block">{names}{more}。</span>}
            删除后不再监控{list.length > 1 ? "这些" : "这个"}直播间，已录制的文件不会删除。
          </AlertDialogDescription>
        </AlertDialogHeader>
        {error && (
          <Notice tone="error" title={`无法删除：${error.title}`}>
            {error.message}
          </Notice>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={deleting}>取消</AlertDialogCancel>
          <Button variant="destructive" onClick={() => void confirm()} disabled={deleting}>
            {deleting ? <Loader2 className="animate-spin" aria-hidden /> : <Trash2 aria-hidden />}
            删除
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
