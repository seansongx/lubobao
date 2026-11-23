import { useState, type ReactNode } from "react";
import { Circle, Copy, ExternalLink, FolderOpen, Pause, Pencil, Play, Square, Trash2 } from "lucide-react";

import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuShortcut,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import type { TaskId, TaskSnapshot } from "@/lib/api";
import { shortcutLabel } from "@/lib/platform";
import { isRecordingActive } from "@/pages/rooms/format";
import {
  canRestartRecording,
  copyRoomLink,
  openFolder,
  openInBrowser,
  setMonitoring,
  startRecording,
  stopRecording,
} from "@/pages/rooms/task-actions";

interface TaskContextMenuProps {
  tasks: TaskSnapshot[];
  /** 右键的行先成为当前行，详情面板随之切换。 */
  onActivate: (id: TaskId) => void;
  onEdit: (id: TaskId) => void;
  onDelete: (tasks: TaskSnapshot[]) => void;
  children: ReactNode;
}

/**
 * 任务表的右键菜单：整张表共用一个菜单，按右键所在行（tr[data-task-id]）决定内容。
 * 菜单项与详情面板的操作一致。空白处右键不弹菜单。
 */
export function TaskContextMenu({ tasks, onActivate, onEdit, onDelete, children }: TaskContextMenuProps) {
  const [taskId, setTaskId] = useState<TaskId | null>(null);
  const task = tasks.find((item) => item.id === taskId) ?? null;

  function onContextMenu(event: React.MouseEvent) {
    const row = event.target instanceof Element ? event.target.closest<HTMLElement>("tr[data-task-id]") : null;
    if (!row) {
      // 阻止 Radix 打开菜单；浏览器菜单由全局行为屏蔽。
      event.preventDefault();
      return;
    }
    const id = Number(row.dataset.taskId) as TaskId;
    setTaskId(id);
    onActivate(id);
  }

  return (
    <ContextMenu onOpenChange={(open) => !open && setTaskId(null)}>
      <ContextMenuTrigger asChild onContextMenu={onContextMenu}>
        <div>{children}</div>
      </ContextMenuTrigger>
      <ContextMenuContent className="min-w-52">{task && <TaskMenuItems task={task} onEdit={onEdit} onDelete={onDelete} />}</ContextMenuContent>
    </ContextMenu>
  );
}

function TaskMenuItems({
  task,
  onEdit,
  onDelete,
}: {
  task: TaskSnapshot;
  onEdit: (id: TaskId) => void;
  onDelete: (tasks: TaskSnapshot[]) => void;
}) {
  const recording = isRecordingActive(task.record);
  const paused = task.monitoring === "paused";
  return (
    <>
      {recording && (
        <ContextMenuItem disabled={task.record.state === "finishing"} onSelect={() => void stopRecording(task)}>
          <Square aria-hidden />
          {task.record.state === "finishing" ? "正在收尾…" : "停止录制"}
        </ContextMenuItem>
      )}
      {canRestartRecording(task) && (
        <ContextMenuItem onSelect={() => void startRecording(task)}>
          <Circle aria-hidden />
          重新开始录制
        </ContextMenuItem>
      )}
      <ContextMenuItem onSelect={() => void setMonitoring([task.id], paused)}>
        {paused ? <Play aria-hidden /> : <Pause aria-hidden />}
        {paused ? "恢复监控" : "暂停监控"}
      </ContextMenuItem>
      <ContextMenuSeparator />
      <ContextMenuItem onSelect={() => onEdit(task.id)}>
        <Pencil aria-hidden />
        编辑…
      </ContextMenuItem>
      <ContextMenuItem onSelect={() => void openFolder(task)}>
        <FolderOpen aria-hidden />
        打开文件夹
      </ContextMenuItem>
      <ContextMenuItem onSelect={() => void openInBrowser(task)}>
        <ExternalLink aria-hidden />
        在浏览器打开直播间
      </ContextMenuItem>
      <ContextMenuItem onSelect={() => void copyRoomLink(task)}>
        <Copy aria-hidden />
        复制直播间链接
      </ContextMenuItem>
      <ContextMenuSeparator />
      <ContextMenuItem variant="destructive" onSelect={() => onDelete([task])}>
        <Trash2 aria-hidden />
        删除…
        <ContextMenuShortcut>{shortcutLabel("Delete")}</ContextMenuShortcut>
      </ContextMenuItem>
    </>
  );
}
