// 直播间页的界面状态。由 App 持有，切换页面后筛选、排序、选中项保持不变。

import type { RowSelectionState, SortingState } from "@tanstack/react-table";

import type { DisplayStatus, TaskId, TaskSnapshot } from "@/lib/api";

export type StatusGroup = "all" | "recording" | "monitoring" | "attention" | "paused";

export interface RoomsView {
  group: StatusGroup;
  /** "all" 或平台标识。 */
  platform: string;
  query: string;
  sorting: SortingState;
  rowSelection: RowSelectionState;
  /** 右侧详情显示的任务。 */
  activeId: TaskId | null;
}

export const initialRoomsView: RoomsView = {
  group: "all",
  platform: "all",
  query: "",
  sorting: [{ id: "status", desc: false }],
  rowSelection: {},
  activeId: null,
};

export const statusGroups: { id: StatusGroup; label: string; statuses: DisplayStatus[] | null }[] = [
  { id: "all", label: "全部任务", statuses: null },
  { id: "recording", label: "录制中", statuses: ["recording"] },
  { id: "monitoring", label: "监控中", statuses: ["checking", "waiting", "notify"] },
  { id: "attention", label: "需关注", statuses: ["needsAction", "retrying"] },
  { id: "paused", label: "已暂停", statuses: ["paused"] },
];

export function inGroup(task: TaskSnapshot, group: StatusGroup): boolean {
  const statuses = statusGroups.find((item) => item.id === group)?.statuses;
  return !statuses || statuses.includes(task.display);
}

export function matchesQuery(task: TaskSnapshot, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [task.anchorName, task.roomId, task.note, task.url].some((value) => value.toLowerCase().includes(needle));
}
