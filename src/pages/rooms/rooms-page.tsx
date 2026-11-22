import { useEffect, useMemo, useState, type Dispatch, type SetStateAction } from "react";
import type { RowSelectionState, SortingState, Updater } from "@tanstack/react-table";
import {
  ArrowDownWideNarrow,
  ArrowUpNarrowWide,
  Pause,
  Play,
  Plus,
  Radio,
  Search,
  Trash2,
  X,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import type { TaskId, TaskSnapshot } from "@/lib/api";
import { useTasks } from "@/lib/use-backend";
import { cn } from "@/lib/utils";
import { AddTaskDialog } from "@/pages/rooms/add-task-dialog";
import { DeleteTasksDialog } from "@/pages/rooms/delete-tasks-dialog";
import { EditTaskDialog } from "@/pages/rooms/edit-task-dialog";
import {
  inGroup,
  matchesQuery,
  statusGroups,
  type RoomsView,
  type StatusGroup,
} from "@/pages/rooms/rooms-view";
import { setMonitoring } from "@/pages/rooms/task-actions";
import { TaskInspector } from "@/pages/rooms/task-inspector";
import { StatusMark } from "@/pages/rooms/task-status";
import { TaskTable } from "@/pages/rooms/task-table";
import { useNow } from "@/pages/rooms/use-now";

function resolve<T>(updater: Updater<T>, current: T): T {
  return typeof updater === "function" ? (updater as (old: T) => T)(current) : updater;
}

const groupMark: Partial<Record<StatusGroup, Parameters<typeof StatusMark>[0]["status"]>> = {
  recording: "recording",
  monitoring: "waiting",
  attention: "needsAction",
  paused: "paused",
};

/** 排序字段与默认方向；表头点击与这里共用同一份排序状态。 */
const sortOptions = [
  { id: "status", label: "按状态", desc: false },
  { id: "anchor", label: "按主播名", desc: false },
  { id: "timing", label: "按已录时长", desc: true },
  { id: "size", label: "按文件大小", desc: true },
];

/** 其他页面请求定位到某个任务；seq 变化即重新定位。与 src/components/navigation.ts 的约定一致，合并时统一。 */
export interface TaskFocusRequest {
  taskId: TaskId;
  seq: number;
}

interface RoomsPageProps {
  view: RoomsView;
  onViewChange: Dispatch<SetStateAction<RoomsView>>;
  focusTask?: TaskFocusRequest | null;
}

export function RoomsPage({ view, onViewChange, focusTask }: RoomsPageProps) {
  const tasks = useTasks();
  const now = useNow();
  const [adding, setAdding] = useState(false);
  const [editingId, setEditingId] = useState<TaskId | null>(null);
  const [deleting, setDeleting] = useState<TaskSnapshot[] | null>(null);
  const [batchBusy, setBatchBusy] = useState(false);

  const patch = (values: Partial<RoomsView>) => onViewChange((current) => ({ ...current, ...values }));

  const platforms = useMemo(() => {
    const map = new Map<string, string>();
    for (const task of tasks) map.set(task.platform, task.platformName);
    return [...map.entries()];
  }, [tasks]);

  const visible = useMemo(
    () =>
      tasks.filter(
        (task) =>
          inGroup(task, view.group) &&
          (view.platform === "all" || task.platform === view.platform) &&
          matchesQuery(task, view.query),
      ),
    [tasks, view.group, view.platform, view.query],
  );

  // 批量操作只作用于当前显示且被勾选的任务。
  const selected = visible.filter((task) => view.rowSelection[String(task.id)]);
  const activeTask = tasks.find((task) => task.id === view.activeId) ?? null;
  const editingTask = tasks.find((task) => task.id === editingId) ?? null;
  const sort = view.sorting[0] ?? { id: "status", desc: false };
  const filtered = view.group !== "all" || view.platform !== "all" || view.query.trim() !== "";

  const onSortingChange = (updater: Updater<SortingState>) =>
    onViewChange((current) => ({ ...current, sorting: resolve(updater, current.sorting) }));
  const onRowSelectionChange = (updater: Updater<RowSelectionState>) =>
    onViewChange((current) => ({ ...current, rowSelection: resolve(updater, current.rowSelection) }));

  const clearSelection = () => patch({ rowSelection: {} });

  async function batchMonitoring(enabled: boolean) {
    setBatchBusy(true);
    const ok = await setMonitoring(
      selected.map((task) => task.id),
      enabled,
    );
    setBatchBusy(false);
    if (ok) clearSelection();
  }

  function onDeleted(ids: TaskId[]) {
    onViewChange((current) => {
      const rowSelection = { ...current.rowSelection };
      for (const id of ids) delete rowSelection[String(id)];
      return {
        ...current,
        rowSelection,
        activeId: current.activeId !== null && ids.includes(current.activeId) ? null : current.activeId,
      };
    });
  }

  const showTask = (id: TaskId) => {
    // 定位到任务：清除会把它隐藏的筛选。
    onViewChange((current) => ({ ...current, activeId: id, group: "all", platform: "all", query: "" }));
  };

  const focusSeq = focusTask?.seq;
  useEffect(() => {
    if (focusTask) showTask(focusTask.taskId);
  }, [focusSeq]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <section className="flex min-h-0 min-w-0 flex-1 flex-col">
        <header className="flex items-center gap-2.5 px-5 pt-3.5">
          <h1 className="m-0 text-[17px] font-semibold">直播间</h1>
          <span className="flex-1" />
          <Button onClick={() => setAdding(true)}>
            <Plus aria-hidden />
            添加直播间
          </Button>
        </header>

        {tasks.length === 0 ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : (
          <>
            <div className="grid grid-cols-5 gap-2 px-5 pt-3.5 xl:gap-2.5" role="group" aria-label="按状态筛选">
              {statusGroups.map((group) => {
                const count = tasks.filter((task) => inGroup(task, group.id)).length;
                const on = view.group === group.id;
                const mark = groupMark[group.id];
                return (
                  <button
                    key={group.id}
                    type="button"
                    aria-pressed={on}
                    onClick={() => patch({ group: group.id })}
                    className={cn(
                      "flex min-w-0 flex-col gap-0.5 rounded-lg border bg-card px-3 py-2 text-left outline-none transition-colors",
                      "hover:border-muted-foreground/40 focus-visible:ring-3 focus-visible:ring-ring/50",
                      on && "border-primary shadow-[0_0_0_1px_var(--primary)] hover:border-primary",
                    )}
                  >
                    <span className="flex min-w-0 items-center gap-1.5 text-xs text-muted-foreground">
                      {mark && <StatusMark status={mark} />}
                      <span className="truncate">{group.label}</span>
                    </span>
                    <span className="text-lg font-semibold tabular-nums xl:text-xl">{count}</span>
                  </button>
                );
              })}
            </div>

            <div className="flex flex-wrap items-center gap-2 px-5 py-3">
              <div className="relative max-w-[260px] min-w-[150px] flex-1">
                <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" aria-hidden />
                <Input
                  value={view.query}
                  onChange={(event) => patch({ query: event.target.value })}
                  placeholder="搜索主播或房间号"
                  aria-label="搜索主播"
                  className="pr-7 pl-8"
                />
                {view.query && (
                  <button
                    type="button"
                    aria-label="清除搜索"
                    onClick={() => patch({ query: "" })}
                    className="absolute top-1/2 right-1.5 grid size-5 -translate-y-1/2 place-items-center rounded-sm text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <X className="size-3.5" aria-hidden />
                  </button>
                )}
              </div>
              <Select value={view.platform} onValueChange={(platform) => patch({ platform })}>
                <SelectTrigger className="w-[108px]" aria-label="按平台筛选">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">全部平台</SelectItem>
                  {platforms.map(([id, name]) => (
                    <SelectItem key={id} value={id}>
                      {name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <div className="flex items-center">
                <Select
                  value={sort.id}
                  onValueChange={(id) => patch({ sorting: [{ id, desc: sortOptions.find((item) => item.id === id)?.desc ?? false }] })}
                >
                  <SelectTrigger className="w-[116px] rounded-r-none" aria-label="排序方式">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {sortOptions.map((option) => (
                      <SelectItem key={option.id} value={option.id}>
                        {option.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  variant="outline"
                  size="icon"
                  className="-ml-px rounded-l-none"
                  aria-label={sort.desc ? "当前降序，切换为升序" : "当前升序，切换为降序"}
                  title={sort.desc ? "降序" : "升序"}
                  onClick={() => patch({ sorting: [{ id: sort.id, desc: !sort.desc }] })}
                >
                  {sort.desc ? <ArrowDownWideNarrow aria-hidden /> : <ArrowUpNarrowWide aria-hidden />}
                </Button>
              </div>
              <span className="ml-auto text-xs text-muted-foreground tabular-nums">
                {visible.length === tasks.length ? `${tasks.length} 项` : `${visible.length} / ${tasks.length} 项`}
              </span>
            </div>

            <div className="relative mx-5 mb-4 flex min-h-0 flex-1 flex-col">
            <div
                className={cn(
                  "@container min-h-0 flex-1 overflow-auto rounded-lg border bg-card",
                  // 批量操作条浮在表格底部，留出空间让最后一行可以滚动出来。
                  selected.length > 0 && "pb-16",
                )}
              >
              {visible.length > 0 ? (
                <TaskTable
                  tasks={visible}
                  now={now}
                  sorting={view.sorting}
                  onSortingChange={onSortingChange}
                  rowSelection={view.rowSelection}
                  onRowSelectionChange={onRowSelectionChange}
                  activeId={view.activeId}
                  onActivate={(id) => patch({ activeId: id })}
                />
              ) : (
                <div className="flex flex-col items-center gap-3 px-5 py-14 text-center text-muted-foreground">
                  没有符合条件的直播间
                  {filtered && (
                    <Button variant="outline" size="sm" onClick={() => patch({ group: "all", platform: "all", query: "" })}>
                      清除筛选
                    </Button>
                  )}
                </div>
              )}
            </div>

            {selected.length > 0 && (
              <div className="pointer-events-none absolute inset-x-0 bottom-3.5 z-10 flex justify-center px-3">
                <div
                  role="toolbar"
                  aria-label="批量操作"
                  className="pointer-events-auto flex items-center gap-1.5 rounded-xl bg-foreground py-1.5 pr-1.5 pl-3.5 text-background shadow-lg"
                >
                  <span className="mr-1 text-[13px] whitespace-nowrap tabular-nums">已选 {selected.length} 项</span>
                  <BatchButton onClick={() => void batchMonitoring(true)} disabled={batchBusy}>
                    <Play aria-hidden />
                    开始监控
                  </BatchButton>
                  <BatchButton onClick={() => void batchMonitoring(false)} disabled={batchBusy}>
                    <Pause aria-hidden />
                    暂停
                  </BatchButton>
                  <BatchButton onClick={() => setDeleting(selected)} disabled={batchBusy}>
                    <Trash2 aria-hidden />
                    删除
                  </BatchButton>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    aria-label="取消选择"
                    title="取消选择"
                    onClick={clearSelection}
                    className="text-background hover:bg-background/15 hover:text-background"
                  >
                    <X aria-hidden />
                  </Button>
                </div>
              </div>
            )}
            </div>
          </>
        )}
      </section>

      {activeTask && (
        <TaskInspector
          key={activeTask.id}
          task={activeTask}
          now={now}
          onClose={() => patch({ activeId: null })}
          onEdit={() => setEditingId(activeTask.id)}
          onDelete={() => setDeleting([activeTask])}
        />
      )}

      <AddTaskDialog open={adding} onOpenChange={setAdding} onShowTask={showTask} />
      <EditTaskDialog task={editingTask} onOpenChange={(open) => !open && setEditingId(null)} />
      <DeleteTasksDialog tasks={deleting} onOpenChange={(open) => !open && setDeleting(null)} onDeleted={onDeleted} />
    </div>
  );
}

function BatchButton({ className, ...props }: React.ComponentProps<typeof Button>) {
  return (
    <Button
      variant="outline"
      size="sm"
      className={cn(
        "border-background/25 bg-transparent text-background hover:bg-background/15 hover:text-background dark:border-background/25 dark:bg-transparent dark:hover:bg-background/15",
        className,
      )}
      {...props}
    />
  );
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="mx-5 mt-3.5 mb-4 flex flex-1 items-center justify-center rounded-xl border bg-card">
      <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-muted text-muted-foreground">
          <Radio className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">还没有直播间</p>
        <p className="m-0 text-muted-foreground">
          粘贴抖音直播间链接、主播主页或分享短链。录播宝会按检测间隔查看是否开播，开播后自动录制。
        </p>
        <Button className="mt-2" onClick={onAdd}>
          <Plus aria-hidden />
          添加第一个直播间
        </Button>
      </div>
    </div>
  );
}
