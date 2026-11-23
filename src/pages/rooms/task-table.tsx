import { memo, useCallback, useMemo } from "react";
import {
  createColumnHelper,
  createSortedRowModel,
  metaHelper,
  rowSelectionFeature,
  rowSortingFeature,
  tableFeatures,
  useTable,
  type OnChangeFn,
  type RowSelectionState,
  type SortingState,
} from "@tanstack/react-table";
import { ArrowDown, ArrowUp, ArrowUpDown } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { TaskId, TaskSnapshot } from "@/lib/api";
import { cn } from "@/lib/utils";
import { AnchorAvatar, PlatformTag } from "@/pages/rooms/anchor";
import { describeTiming, formatBitrate, formatBytes, qualityLabels, statusRank } from "@/pages/rooms/format";
import { TaskStatus } from "@/pages/rooms/task-status";
import { useNow } from "@/pages/rooms/use-now";

interface ColumnMeta {
  /** 表头和单元格共用的宽度与响应式隐藏类。 */
  className?: string;
}

const features = tableFeatures({
  rowSortingFeature,
  rowSelectionFeature,
  sortedRowModel: createSortedRowModel(),
  columnMeta: metaHelper<ColumnMeta>(),
});

const helper = createColumnHelper<typeof features, TaskSnapshot>();

// 列定义只负责表头、排序取值和宽度；单元格由 TaskRow 渲染，便于按任务引用跳过重渲染。
// 窄窗口（详情面板打开、1024 宽）时按容器宽度隐藏次要列。
const columnClass = {
  select: "w-10 pl-3 pr-0",
  anchor: "min-w-0",
  status: "w-[136px]",
  quality: "hidden w-[64px] @[620px]:table-cell",
  timing: "w-[112px]",
  size: "hidden w-[150px] @[540px]:table-cell",
};

const columns = helper.columns([
  helper.display({ id: "select", meta: { className: columnClass.select } }),
  helper.accessor((task) => task.anchorName, {
    id: "anchor",
    header: "主播",
    meta: { className: columnClass.anchor },
    sortFn: (a, b) => a.original.anchorName.localeCompare(b.original.anchorName, "zh-CN"),
  }),
  helper.accessor((task) => statusRank[task.display], {
    id: "status",
    header: "状态",
    meta: { className: columnClass.status },
  }),
  helper.accessor((task) => task.quality, {
    id: "quality",
    header: "画质",
    enableSorting: false,
    meta: { className: columnClass.quality },
  }),
  helper.accessor(
    (task) =>
      task.record.state === "recording" || task.record.state === "finishing"
        ? -task.record.startedAt
        : Number.NEGATIVE_INFINITY,
    {
      id: "timing",
      header: "时长 / 检测",
      sortDescFirst: true,
      meta: { className: columnClass.timing },
    },
  ),
  helper.accessor((task) => (task.record.state === "recording" ? task.record.bytesWritten : -1), {
    id: "size",
    header: "大小 / 码率",
    sortDescFirst: true,
    meta: { className: columnClass.size },
  }),
]);

/** 只有这一格订阅时钟，每秒刷新时不牵动整行和整表。 */
function TimingCell({ task }: { task: TaskSnapshot }) {
  const now = useNow();
  const timing = describeTiming(task, now);
  const active = task.record.state === "recording" || task.record.state === "finishing";
  return (
    <span className={cn("tabular-nums", active ? "text-foreground" : "text-muted-foreground")} title={timing.hint}>
      {timing.text}
    </span>
  );
}

interface TaskRowProps {
  task: TaskSnapshot;
  selected: boolean;
  active: boolean;
  onSelect: (id: TaskId, value: boolean) => void;
  onActivate: (id: TaskId) => void;
}

/** 按 task 引用比较：进度事件只让对应的一行重渲染。 */
const TaskRow = memo(function TaskRow({ task, selected, active, onSelect, onActivate }: TaskRowProps) {
  const needsAction = task.display === "needsAction";
  return (
    <TableRow
      data-state={selected ? "selected" : undefined}
      data-task-id={task.id}
      aria-current={active ? "true" : undefined}
      onClick={() => onActivate(task.id)}
      className={cn(
        "h-task-row cursor-default",
        // 需处理行始终保留淡红底，勾选不覆盖；当前行为淡 Teal 底（各平台一致，状态色保持可读），
        // Windows 另在左侧画 Fluent 的 Teal 竖条。
        needsAction
          ? "bg-status-recording-soft hover:bg-status-recording-soft data-[state=selected]:bg-status-recording-soft"
          : active && "bg-selection hover:bg-selection data-[state=selected]:bg-selection",
        active &&
          "win:[&>td:first-child]:relative win:[&>td:first-child]:before:absolute win:[&>td:first-child]:before:top-1/2 win:[&>td:first-child]:before:left-0 win:[&>td:first-child]:before:h-4 win:[&>td:first-child]:before:w-[3px] win:[&>td:first-child]:before:-translate-y-1/2 win:[&>td:first-child]:before:rounded-full win:[&>td:first-child]:before:bg-primary",
        active && needsAction && "linux:shadow-[inset_3px_0_0_var(--primary)] mac:shadow-[inset_3px_0_0_var(--primary)]",
        task.display === "paused" && "[&>td:not(:first-child)]:opacity-55",
      )}
    >
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.select)}>
        <Checkbox
          aria-label={`选择 ${task.anchorName}`}
          checked={selected}
          onCheckedChange={(value) => onSelect(task.id, value === true)}
          onClick={(event) => event.stopPropagation()}
        />
      </TableCell>
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.anchor)}>
        <div className="flex min-w-0 items-center gap-2.5">
          <AnchorAvatar task={task} />
          <div className="min-w-0">
            <Button
              variant="link"
              className="h-auto max-w-full min-w-0 shrink justify-start p-0"
              onClick={(event) => {
                event.stopPropagation();
                onActivate(task.id);
              }}
              title={task.anchorName}
            >
              <span className="truncate">{task.anchorName}</span>
            </Button>
            <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-muted-foreground">
              <PlatformTag task={task} />
              <span className="truncate tabular-nums">{task.roomId}</span>
            </div>
          </div>
        </div>
      </TableCell>
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.status)}>
        <TaskStatus task={task} />
      </TableCell>
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.quality)}>
        <span className="text-muted-foreground">
          {qualityLabels[task.quality]}
          {task.notifyOnly && <span className="sr-only">，仅通知</span>}
        </span>
      </TableCell>
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.timing)}>
        <TimingCell task={task} />
      </TableCell>
      <TableCell className={cn("h-task-row px-2.5 py-0", columnClass.size)}>
        {task.record.state === "recording" ? (
          <span className="tabular-nums">
            {formatBytes(task.record.bytesWritten)}
            <span className="text-muted-foreground"> · {formatBitrate(task.record.bitrateKbps)}</span>
          </span>
        ) : (
          <span className="text-muted-foreground">—</span>
        )}
      </TableCell>
    </TableRow>
  );
});

interface TaskTableProps {
  tasks: TaskSnapshot[];
  sorting: SortingState;
  onSortingChange: OnChangeFn<SortingState>;
  rowSelection: RowSelectionState;
  onRowSelectionChange: OnChangeFn<RowSelectionState>;
  activeId: TaskId | null;
  onActivate: (id: TaskId) => void;
}

export function TaskTable({
  tasks,
  sorting,
  onSortingChange,
  rowSelection,
  onRowSelectionChange,
  activeId,
  onActivate,
}: TaskTableProps) {
  const options = useMemo(
    () => ({
      getRowId: (task: TaskSnapshot) => String(task.id),
      enableSortingRemoval: false,
      // 工具栏只显示一个排序字段，Shift 点击表头也不叠加多列排序。
      enableMultiSort: false,
    }),
    [],
  );
  const table = useTable({
    features,
    columns,
    data: tasks,
    ...options,
    state: { sorting, rowSelection },
    onSortingChange,
    onRowSelectionChange,
  });

  const onSelect = useCallback(
    (id: TaskId, value: boolean) =>
      onRowSelectionChange((current) => {
        const next = { ...current };
        if (value) next[String(id)] = true;
        else delete next[String(id)];
        return next;
      }),
    [onRowSelectionChange],
  );

  const rows = table.getRowModel().rows;
  // 表头状态按“可见已选 / 可见总数”计算，不受被筛选掉的旧选择影响。
  const selectedCount = rows.reduce((count, row) => count + (rowSelection[row.id] ? 1 : 0), 0);
  const headerChecked = selectedCount === 0 ? false : selectedCount === rows.length ? true : "indeterminate";
  const toggleAllVisible = (value: boolean) =>
    onRowSelectionChange(() => (value ? Object.fromEntries(rows.map((row) => [row.id, true as const])) : {}));

  return (
    <table className="w-full table-fixed border-collapse text-(length:--text-dense)">
      <TableHeader className="sticky top-0 z-[1] bg-content backdrop-blur-xl linux:bg-card">
        {table.getHeaderGroups().map((group) => (
          <TableRow key={group.id} className="hover:bg-transparent">
            {group.headers.map((header) => {
              const meta = header.column.columnDef.meta;
              const sorted = header.column.getIsSorted();
              const isSelect = header.column.id === "select";
              const canSort = !isSelect && header.column.getCanSort();
              return (
                <TableHead
                  key={header.id}
                  className={cn("h-8 px-2.5 text-xs font-normal text-muted-foreground mac:h-7 mac:font-medium linux:font-semibold", meta?.className)}
                  aria-sort={sorted === "asc" ? "ascending" : sorted === "desc" ? "descending" : undefined}
                >
                  {isSelect ? (
                    <Checkbox
                      aria-label="选择全部显示的任务"
                      checked={headerChecked}
                      onCheckedChange={() => toggleAllVisible(headerChecked !== true)}
                    />
                  ) : canSort ? (
                    <Button
                      variant="ghost"
                      size="sm"
                      className="-ml-2.5"
                      onClick={header.column.getToggleSortingHandler()}
                      aria-label={`按${String(header.column.columnDef.header)}排序`}
                    >
                      <table.FlexRender header={header} />
                      {sorted === "asc" ? (
                        <ArrowUp aria-hidden />
                      ) : sorted === "desc" ? (
                        <ArrowDown aria-hidden />
                      ) : (
                        <ArrowUpDown aria-hidden />
                      )}
                    </Button>
                  ) : (
                    <table.FlexRender header={header} />
                  )}
                </TableHead>
              );
            })}
          </TableRow>
        ))}
      </TableHeader>
      <TableBody>
        {rows.map((row) => (
          <TaskRow
            key={row.id}
            task={row.original}
            selected={!!rowSelection[row.id]}
            active={row.original.id === activeId}
            onSelect={onSelect}
            onActivate={onActivate}
          />
        ))}
      </TableBody>
    </table>
  );
}
