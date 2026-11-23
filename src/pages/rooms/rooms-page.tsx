import { useCallback, useEffect, useMemo, useRef, useState, type Dispatch, type RefObject, type SetStateAction } from "react";
import type { RowSelectionState, SortingState, Updater } from "@tanstack/react-table";
import {
  AlertTriangle,
  ArrowDownWideNarrow,
  Loader2,
  ArrowUpNarrowWide,
  Pause,
  Play,
  Plus,
  Radio,
  Search,
  Trash2,
  X,
} from "lucide-react";

import type { Navigate, TaskFocusRequest } from "@/components/navigation";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { ButtonGroup } from "@/components/ui/button-group";
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput } from "@/components/ui/input-group";
import { Select, SelectContent, SelectGroup, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { TaskId, TaskSnapshot } from "@/lib/api";
import { useAppCommand } from "@/lib/app-commands";
import { withShortcut } from "@/lib/platform";
import type { TaskList } from "@/lib/use-backend";
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
import { TaskContextMenu } from "@/pages/rooms/task-context-menu";
import { TaskInspector } from "@/pages/rooms/task-inspector";
import { StatusMark } from "@/pages/rooms/task-status";
import { TaskTable } from "@/pages/rooms/task-table";
import { describeError } from "@/pages/rooms/format";

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

interface RoomsPageProps {
  /** 由 App 持有的任务订阅，切换页面不重新加载。 */
  taskList: TaskList;
  view: RoomsView;
  onViewChange: Dispatch<SetStateAction<RoomsView>>;
  focusTask?: TaskFocusRequest | null;
  /** 详情面板「全部动态」跳到动态页。 */
  onNavigate?: Navigate;
}

const NO_TASKS: TaskSnapshot[] = [];

/** 对话框状态：关闭时保留内容直到动画结束；key 每次打开递增，让表单重新挂载并取得新的初始值。 */
interface DialogState<T> {
  open: boolean;
  key: number;
  value: T;
}

export function RoomsPage({ taskList, view, onViewChange, focusTask, onNavigate }: RoomsPageProps) {
  const tasks = taskList.tasks ?? NO_TASKS;
  const [adding, setAdding] = useState<DialogState<null>>({ open: false, key: 0, value: null });
  const [editing, setEditing] = useState<DialogState<TaskId | null>>({ open: false, key: 0, value: null });
  const [deleting, setDeleting] = useState<DialogState<TaskSnapshot[]>>({ open: false, key: 0, value: [] });
  const [batchBusy, setBatchBusy] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);

  const patch = useCallback(
    (values: Partial<RoomsView>) => onViewChange((current) => ({ ...current, ...values })),
    [onViewChange],
  );
  const openAdd = () => setAdding((current) => ({ open: true, key: current.key + 1, value: null }));
  const openEdit = useCallback(
    (id: TaskId) => setEditing((current) => ({ open: true, key: current.key + 1, value: id })),
    [],
  );
  const openDelete = useCallback(
    (list: TaskSnapshot[]) => setDeleting((current) => ({ open: true, key: current.key + 1, value: list })),
    [],
  );

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

  // 筛选或任务变化后剔除不可见的勾选，批量操作和表头状态都只针对可见行。
  useEffect(() => {
    if (taskList.tasks === null) return;
    const visibleIds = new Set(visible.map((task) => String(task.id)));
    onViewChange((current) => {
      const stale = Object.keys(current.rowSelection).filter((id) => !visibleIds.has(id));
      if (stale.length === 0) return current;
      const rowSelection = { ...current.rowSelection };
      for (const id of stale) delete rowSelection[id];
      return { ...current, rowSelection };
    });
  }, [visible, taskList.tasks, onViewChange]);

  // 筛选按钮上的数字一次遍历算完。
  const counts = useMemo(() => {
    const result: Record<StatusGroup, number> = { all: 0, recording: 0, monitoring: 0, attention: 0, paused: 0 };
    for (const task of tasks) {
      result.all += 1;
      for (const group of statusGroups) if (group.id !== "all" && inGroup(task, group.id)) result[group.id] += 1;
    }
    return result;
  }, [tasks]);

  const selected = visible.filter((task) => view.rowSelection[String(task.id)]);
  const activeTask = tasks.find((task) => task.id === view.activeId) ?? null;
  const editingTask = tasks.find((task) => task.id === editing.value) ?? null;
  const sort = view.sorting[0] ?? { id: "status", desc: false };
  const filtered = view.group !== "all" || view.platform !== "all" || view.query.trim() !== "";

  const onSortingChange = useCallback(
    (updater: Updater<SortingState>) =>
      onViewChange((current) => ({ ...current, sorting: resolve(updater, current.sorting) })),
    [onViewChange],
  );
  const onRowSelectionChange = useCallback(
    (updater: Updater<RowSelectionState>) =>
      onViewChange((current) => ({ ...current, rowSelection: resolve(updater, current.rowSelection) })),
    [onViewChange],
  );
  const onActivate = useCallback((id: TaskId) => patch({ activeId: id }), [patch]);
  const closeInspector = useCallback(() => patch({ activeId: null }), [patch]);

  const clearSelection = () => patch({ rowSelection: {} });

  /** 批量操作成功后，启停和删除都只取消被处理的那些勾选。 */
  const unselect = useCallback(
    (ids: TaskId[]) =>
      onViewChange((current) => {
        const rowSelection = { ...current.rowSelection };
        for (const id of ids) delete rowSelection[String(id)];
        return { ...current, rowSelection };
      }),
    [onViewChange],
  );

  async function batchMonitoring(enabled: boolean) {
    const ids = selected.map((task) => task.id);
    setBatchBusy(true);
    const ok = await setMonitoring(ids, enabled);
    setBatchBusy(false);
    if (ok) unselect(ids);
  }

  const onDeleted = useCallback(
    (ids: TaskId[]) => {
      unselect(ids);
      onViewChange((current) =>
        current.activeId !== null && ids.includes(current.activeId) ? { ...current, activeId: null } : current,
      );
    },
    [onViewChange, unselect],
  );

  useAppCommand("add-task", openAdd);
  useAppCommand("delete-selection", () => {
    if (selected.length > 0) openDelete(selected);
    else if (activeTask) openDelete([activeTask]);
    else return false;
  });
  useAppCommand("dismiss", () => {
    if (view.activeId !== null) closeInspector();
    else if (selected.length > 0) clearSelection();
    else return false;
  });

  const onShowActivity = useCallback(
    (id: TaskId) => onNavigate?.("activity", { activityTaskId: id }),
    [onNavigate],
  );

  const showTask = (id: TaskId) => {
    // 定位到任务：清除会把它隐藏的筛选。
    onViewChange((current) => ({ ...current, activeId: id, group: "all", platform: "all", query: "" }));
  };

  // 其他页面请求定位任务：每个 seq 只处理一次，切回本页时不重复应用。
  useEffect(() => {
    if (!focusTask) return;
    onViewChange((current) =>
      focusTask.seq === current.handledFocusSeq
        ? current
        : {
            ...current,
            handledFocusSeq: focusTask.seq,
            activeId: focusTask.taskId,
            group: "all",
            platform: "all",
            query: "",
          },
    );
  }, [focusTask, onViewChange]);

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <section className="flex min-h-0 min-w-0 flex-1 flex-col">
        <PageHeader
          title="直播间"
          summary={taskList.tasks ? roomsSummary(tasks.length, counts.recording) : undefined}
          actions={
            <>
              {tasks.length > 0 && (
                <SearchBox inputRef={searchRef} value={view.query} onChange={(query) => patch({ query })} />
              )}
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button onClick={openAdd}>
                    <Plus aria-hidden />
                    添加直播间
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{withShortcut("添加直播间", "N")}</TooltipContent>
              </Tooltip>
            </>
          }
        />

        {taskList.tasks === null ? (
          taskList.error ? (
            <LoadError error={taskList.error} onRetry={taskList.reload} />
          ) : (
            <div className="flex flex-1 items-center justify-center gap-2 text-muted-foreground" role="status">
              <Loader2 className="size-4 animate-spin" aria-hidden />
              正在读取直播间…
            </div>
          )
        ) : tasks.length === 0 ? (
          <EmptyState onAdd={openAdd} />
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-2 px-(--page-pad) pt-2.5 pb-2">
              <StatusFilter value={view.group} counts={counts} onChange={(group) => patch({ group })} />
              <div className="ml-auto flex items-center gap-2">
                {filtered && (
                  <span className="text-xs whitespace-nowrap text-muted-foreground tabular-nums" aria-live="polite">
                    {visible.length} / {tasks.length} 项
                  </span>
                )}
                <Select value={view.platform} onValueChange={(platform) => patch({ platform })}>
                  <SelectTrigger className="w-24" aria-label="按平台筛选">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectGroup>
                      <SelectItem value="all">全部平台</SelectItem>
                      {platforms.map(([id, name]) => (
                        <SelectItem key={id} value={id}>
                          {name}
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  </SelectContent>
                </Select>
                <ButtonGroup>
                  <Select
                    value={sort.id}
                    onValueChange={(id) => patch({ sorting: [{ id, desc: sortOptions.find((item) => item.id === id)?.desc ?? false }] })}
                  >
                    <SelectTrigger className="w-24" aria-label="排序方式">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectGroup>
                        {sortOptions.map((option) => (
                          <SelectItem key={option.id} value={option.id}>
                            {option.label}
                          </SelectItem>
                        ))}
                      </SelectGroup>
                    </SelectContent>
                  </Select>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        variant="outline"
                        size="icon"
                        aria-label={sort.desc ? "当前降序，切换为升序" : "当前升序，切换为降序"}
                        onClick={() => patch({ sorting: [{ id: sort.id, desc: !sort.desc }] })}
                      >
                        {sort.desc ? <ArrowDownWideNarrow aria-hidden /> : <ArrowUpNarrowWide aria-hidden />}
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>{sort.desc ? "降序" : "升序"}</TooltipContent>
                  </Tooltip>
                </ButtonGroup>
              </div>
            </div>

            <div className="relative mx-2.5 mb-2 flex min-h-0 flex-1 flex-col">
              <div
                className={cn(
                  "@container min-h-0 flex-1 overflow-auto",
                  // 批量操作条浮在表格底部，留出空间让最后一行可以滚动出来。
                  selected.length > 0 && "pb-16",
                )}
              >
                {visible.length > 0 ? (
                  <TaskContextMenu tasks={visible} onActivate={onActivate} onEdit={openEdit} onDelete={openDelete}>
                    <TaskTable
                      tasks={visible}
                      sorting={view.sorting}
                      onSortingChange={onSortingChange}
                      rowSelection={view.rowSelection}
                      onRowSelectionChange={onRowSelectionChange}
                      activeId={view.activeId}
                      onActivate={onActivate}
                    />
                  </TaskContextMenu>
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
                <div className="pointer-events-none absolute inset-x-0 bottom-3 z-10 flex justify-center px-3">
                  <div
                    role="toolbar"
                    aria-label="批量操作"
                    className="pointer-events-auto flex items-center gap-1 rounded-xl bg-popover py-1 pr-1 pl-3.5 text-popover-foreground shadow-lg ring-1 ring-foreground/10"
                  >
                    <span className="mr-1.5 text-(length:--text-dense) whitespace-nowrap tabular-nums">已选 {selected.length} 项</span>
                    <Button variant="ghost" size="sm" onClick={() => void batchMonitoring(true)} disabled={batchBusy}>
                      <Play aria-hidden />
                      开始监控
                    </Button>
                    <Button variant="ghost" size="sm" onClick={() => void batchMonitoring(false)} disabled={batchBusy}>
                      <Pause aria-hidden />
                      暂停
                    </Button>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          variant="destructive"
                          size="sm"
                          onClick={() => openDelete(selected)}
                          disabled={batchBusy}
                        >
                          <Trash2 aria-hidden />
                          删除
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>{withShortcut("删除所选", "Delete", false)}</TooltipContent>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button variant="ghost" size="icon-sm" aria-label="取消选择" onClick={clearSelection}>
                          <X aria-hidden />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>{withShortcut("取消选择", "Escape", false)}</TooltipContent>
                    </Tooltip>
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
          onClose={closeInspector}
          onEdit={openEdit}
          onDelete={openDelete}
          onShowActivity={onShowActivity}
        />
      )}

      <AddTaskDialog
        key={"add-" + adding.key}
        open={adding.open}
        onOpenChange={(open) => setAdding((current) => ({ ...current, open }))}
        onShowTask={showTask}
      />
      {editingTask && (
        <EditTaskDialog
          key={"edit-" + editing.key}
          task={editingTask}
          open={editing.open}
          onOpenChange={(open) => setEditing((current) => ({ ...current, open }))}
        />
      )}
      <DeleteTasksDialog
        key={"delete-" + deleting.key}
        tasks={deleting.value}
        open={deleting.open}
        onOpenChange={(open) => setDeleting((current) => ({ ...current, open }))}
        onDeleted={onDeleted}
      />
    </div>
  );
}

function LoadError({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  const view = describeError(error);
  return (
    <div className="mx-(--page-pad) mt-3.5 mb-4 flex flex-1 items-center justify-center">
      <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center" role="alert">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-status-recording-soft text-status-recording">
          <AlertTriangle className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">无法读取直播间列表</p>
        <p className="m-0 break-words text-muted-foreground">{view.message || view.title}</p>
        <Button className="mt-2" onClick={onRetry}>
          重试
        </Button>
      </div>
    </div>
  );
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="mx-(--page-pad) mt-3.5 mb-4 flex flex-1 items-center justify-center">
      <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-muted text-muted-foreground">
          <Radio className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">还没有直播间</p>
        <p className="m-0 text-muted-foreground">
          粘贴抖音、快手、B站、虎牙的直播间链接，或抖音的主播主页、分享短链。录播宝会按检测间隔查看是否开播，开播后自动录制。
        </p>
        <Button className="mt-2" onClick={onAdd}>
          <Plus aria-hidden />
          添加第一个直播间
        </Button>
      </div>
    </div>
  );
}

function roomsSummary(total: number, recording: number): string {
  return recording > 0 ? `${total} 个直播间 · ${recording} 个录制中` : `${total} 个直播间`;
}

function SearchBox({
  inputRef,
  value,
  onChange,
}: {
  inputRef: RefObject<HTMLInputElement | null>;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <InputGroup className="w-[200px] min-w-[140px] shrink">
      <InputGroupAddon>
        <Search aria-hidden />
      </InputGroupAddon>
      <InputGroupInput
        ref={inputRef}
        data-search-input
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape" && value) {
            event.preventDefault();
            onChange("");
          }
        }}
        placeholder="搜索主播或房间号"
        aria-label="搜索主播"
        title={withShortcut("搜索", "F")}
        spellCheck={false}
        autoComplete="off"
      />
      {value && (
        <InputGroupAddon align="inline-end">
          <InputGroupButton size="icon-xs" aria-label="清除搜索" onClick={() => onChange("")}>
            <X aria-hidden />
          </InputGroupButton>
        </InputGroupAddon>
      )}
    </InputGroup>
  );
}

/** 按状态筛选：shadcn Tabs（line 样式），每项带状态标记和数字。只用作筛选，没有对应的 TabsContent。 */
function StatusFilter({
  value,
  counts,
  onChange,
}: {
  value: StatusGroup;
  counts: Record<StatusGroup, number>;
  onChange: (group: StatusGroup) => void;
}) {
  return (
    <Tabs value={value} onValueChange={(next) => onChange(next as StatusGroup)}>
      <TabsList variant="line" aria-label="按状态筛选">
        {statusGroups.map((group) => {
          const mark = groupMark[group.id];
          return (
            <TabsTrigger key={group.id} value={group.id}>
              {mark && <StatusMark status={mark} />}
              {group.id === "all" ? "全部" : group.label}
              <span className="tabular-nums">{counts[group.id]}</span>
            </TabsTrigger>
          );
        })}
      </TabsList>
    </Tabs>
  );
}
