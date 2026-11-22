import { useMemo, type ReactNode } from "react";
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

import { Checkbox } from "@/components/ui/checkbox";
import { TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { TaskId, TaskSnapshot } from "@/lib/api";
import { cn } from "@/lib/utils";
import {
  describeTiming,
  formatBitrate,
  formatBytes,
  qualityLabels,
  statusRank,
} from "@/pages/rooms/format";
import { TaskStatus } from "@/pages/rooms/task-status";
import { AnchorAvatar, PlatformTag } from "@/pages/rooms/anchor";

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

// 窄窗口（详情面板打开、1024 宽）时按容器宽度隐藏次要列。
const columns = helper.columns([
  helper.display({
    id: "select",
    meta: { className: "w-10 pl-3 pr-0" },
    header: ({ table }) => (
      <Checkbox
        aria-label="选择全部显示的任务"
        checked={table.getIsAllRowsSelected() ? true : table.getIsSomeRowsSelected() ? "indeterminate" : false}
        onCheckedChange={(value) => table.toggleAllRowsSelected(value === true)}
      />
    ),
    cell: ({ row }) => (
      <Checkbox
        aria-label={`选择 ${row.original.anchorName}`}
        checked={row.getIsSelected()}
        onCheckedChange={(value) => row.toggleSelected(value === true)}
        onClick={(event) => event.stopPropagation()}
      />
    ),
  }),
  helper.accessor((task) => task.anchorName, {
    id: "anchor",
    header: "主播",
    meta: { className: "min-w-0" },
    sortFn: (a, b) => a.original.anchorName.localeCompare(b.original.anchorName, "zh-CN"),
    cell: () => null,
  }),
  helper.accessor((task) => statusRank[task.display], {
    id: "status",
    header: "状态",
    meta: { className: "w-[136px]" },
    cell: () => null,
  }),
  helper.accessor((task) => task.quality, {
    id: "quality",
    header: "画质",
    enableSorting: false,
    meta: { className: "hidden w-[64px] @[620px]:table-cell" },
    cell: () => null,
  }),
  helper.accessor(
    (task) =>
      task.record.state === "recording" || task.record.state === "finishing" ? -task.record.startedAt : Number.NEGATIVE_INFINITY,
    {
      id: "timing",
      header: "时长 / 检测",
      sortDescFirst: true,
      meta: { className: "w-[112px]" },
      cell: () => null,
    },
  ),
  helper.accessor((task) => (task.record.state === "recording" ? task.record.bytesWritten : -1), {
    id: "size",
    header: "大小 / 码率",
    sortDescFirst: true,
    meta: { className: "hidden w-[150px] @[540px]:table-cell" },
    cell: () => null,
  }),
]);

interface TaskTableProps {
  tasks: TaskSnapshot[];
  now: number;
  sorting: SortingState;
  onSortingChange: OnChangeFn<SortingState>;
  rowSelection: RowSelectionState;
  onRowSelectionChange: OnChangeFn<RowSelectionState>;
  activeId: TaskId | null;
  onActivate: (id: TaskId) => void;
}

function renderCell(columnId: string, task: TaskSnapshot, now: number, onActivate: (id: TaskId) => void): ReactNode {
  switch (columnId) {
    case "anchor":
      return (
        <div className="flex min-w-0 items-center gap-2.5">
          <AnchorAvatar task={task} />
          <div className="min-w-0">
            <button
              type="button"
              className="block max-w-full truncate rounded-sm text-left font-medium outline-none hover:underline focus-visible:ring-2 focus-visible:ring-ring"
              onClick={(event) => {
                event.stopPropagation();
                onActivate(task.id);
              }}
              title={task.anchorName}
            >
              {task.anchorName}
            </button>
            <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-muted-foreground">
              <PlatformTag task={task} />
              <span className="truncate tabular-nums">{task.roomId}</span>
            </div>
          </div>
        </div>
      );
    case "status":
      return <TaskStatus task={task} />;
    case "quality":
      return (
        <span className="text-muted-foreground">
          {qualityLabels[task.quality]}
          {task.notifyOnly && <span className="sr-only">，仅通知</span>}
        </span>
      );
    case "timing": {
      const timing = describeTiming(task, now);
      const active = task.record.state === "recording" || task.record.state === "finishing";
      return (
        <span className={cn("tabular-nums", active ? "text-foreground" : "text-muted-foreground")} title={timing.hint}>
          {timing.text}
        </span>
      );
    }
    case "size":
      if (task.record.state === "recording") {
        return (
          <span className="tabular-nums">
            {formatBytes(task.record.bytesWritten)}
            <span className="text-muted-foreground"> · {formatBitrate(task.record.bitrateKbps)}</span>
          </span>
        );
      }
      return <span className="text-muted-foreground">—</span>;
    default:
      return null;
  }
}

export function TaskTable({
  tasks,
  now,
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

  return (
    <table className="w-full table-fixed border-collapse text-[13px]">
      <TableHeader className="sticky top-0 z-[1] bg-card">
        {table.getHeaderGroups().map((group) => (
          <TableRow key={group.id} className="hover:bg-transparent">
            {group.headers.map((header) => {
              const meta = header.column.columnDef.meta;
              const sorted = header.column.getIsSorted();
              const canSort = header.column.getCanSort() && header.column.id !== "select";
              return (
                <TableHead
                  key={header.id}
                  className={cn("h-[34px] px-2.5 text-xs font-medium text-muted-foreground", meta?.className)}
                  aria-sort={sorted === "asc" ? "ascending" : sorted === "desc" ? "descending" : undefined}
                >
                  {canSort ? (
                    <button
                      type="button"
                      className="-mx-1 inline-flex items-center gap-1 rounded-sm px-1 outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
                      onClick={header.column.getToggleSortingHandler()}
                      aria-label={`按${String(header.column.columnDef.header)}排序`}
                    >
                      <table.FlexRender header={header} />
                      {sorted === "asc" ? (
                        <ArrowUp className="size-3" aria-hidden />
                      ) : sorted === "desc" ? (
                        <ArrowDown className="size-3" aria-hidden />
                      ) : (
                        <ArrowUpDown className="size-3 opacity-40" aria-hidden />
                      )}
                    </button>
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
        {table.getRowModel().rows.map((row) => {
          const task = row.original;
          const active = task.id === activeId;
          return (
            <TableRow
              key={row.id}
              data-state={row.getIsSelected() ? "selected" : undefined}
              aria-selected={active}
              onClick={() => onActivate(task.id)}
              className={cn(
                "h-task-row cursor-default",
                task.display === "needsAction" && "bg-status-recording-soft hover:bg-status-recording-soft",
                active && "bg-accent hover:bg-accent shadow-[inset_2px_0_0_var(--primary)]",
                task.display === "paused" && "[&>td:not(:first-child)]:opacity-55",
              )}
            >
              {row.getAllCells().map((cell) => {
                const meta = cell.column.columnDef.meta;
                return (
                  <TableCell key={cell.id} className={cn("h-task-row px-2.5 py-0", meta?.className)}>
                    {cell.column.id === "select" ? (
                      <table.FlexRender cell={cell} />
                    ) : (
                      renderCell(cell.column.id, task, now, onActivate)
                    )}
                  </TableCell>
                );
              })}
            </TableRow>
          );
        })}
      </TableBody>
    </table>
  );
}
