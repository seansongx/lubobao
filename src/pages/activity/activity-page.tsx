// 动态：按时间倒序列出开播、下播、录制、失败和用户操作，向下滚动分页加载，收到 activity 事件实时插入。
// 数据来自 Rust TaskManager::list_activity（src-tauri/src/tasks/activity.rs）。

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  CircleDot,
  CircleStop,
  Loader2,
  MousePointerClick,
  Radio,
  RotateCw,
  Square,
  type LucideIcon,
} from "lucide-react";

import { dayKey, formatClock, formatDateTime, formatDay, formatDuration } from "@/components/format";
import type { Navigate } from "@/components/navigation";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { api, type ActivityEntry, type ActivityKind, type TaskId, type TaskSnapshot } from "@/lib/api";
import { useTasks } from "@/lib/use-backend";
import { cn } from "@/lib/utils";

const PAGE_SIZE = 50;

interface ActivityPageProps {
  onNavigate: Navigate;
}

const KIND_FILTERS: { value: ActivityKind; label: string }[] = [
  { value: "live_start", label: "开播" },
  { value: "live_end", label: "下播" },
  { value: "record_start", label: "开始录制" },
  { value: "record_end", label: "录制结束" },
  { value: "failure", label: "失败" },
  { value: "user_action", label: "用户操作" },
];

type Tone = "recording" | "failure" | "retrying" | "notify" | "muted";

const TONE_CLASS: Record<Tone, string> = {
  recording: "bg-status-recording-soft text-status-recording",
  failure: "bg-status-recording-soft text-destructive",
  retrying: "bg-status-retrying/15 text-status-retrying",
  notify: "bg-primary/10 text-primary",
  muted: "bg-muted text-muted-foreground",
};

/** 检测失败的这几类会自动重试，用琥珀色；其余失败需要用户处理，用红色警告。与 derive_display 一致。 */
const RETRYABLE = new Set(["network", "rate_limited", "parse"]);

const FAILURE_KIND_LABEL: Record<string, string> = {
  network: "网络错误，会自动重试",
  rate_limited: "请求过于频繁，会自动重试",
  parse: "解析失败，会自动重试",
  auth_required: "需要登录，请在设置中填写 Cookie",
  invalid_url: "链接无效",
  config: "配置有误",
};

const QUALITY_LABEL: Record<string, string> = { OD: "原画", UHD: "超清", HD: "高清", SD: "标清", LD: "流畅" };

function str(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function visual(entry: ActivityEntry): { icon: LucideIcon; tone: Tone; label: string } {
  switch (entry.kind) {
    case "live_start":
      return { icon: Radio, tone: "notify", label: "开播" };
    case "live_end":
      return { icon: CircleStop, tone: "muted", label: "下播" };
    case "record_start":
      return { icon: CircleDot, tone: "recording", label: "开始录制" };
    case "record_end":
      return { icon: Square, tone: "muted", label: "录制结束" };
    case "failure": {
      const kind = str(entry.detail?.kind);
      return kind && RETRYABLE.has(kind)
        ? { icon: RotateCw, tone: "retrying", label: "检测失败，重试中" }
        : { icon: AlertTriangle, tone: "failure", label: "失败" };
    }
    case "user_action":
      return { icon: MousePointerClick, tone: "muted", label: "用户操作" };
  }
}

function basename(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

/** 从 detail 拼出一行补充说明。 */
function describe(entry: ActivityEntry): string | null {
  const detail = entry.detail ?? {};
  const parts: string[] = [];
  const title = str(detail.title);
  if (title) parts.push(`直播标题：${title}`);
  const quality = str(detail.quality);
  if (quality) parts.push(QUALITY_LABEL[quality] ?? quality);
  if (typeof detail.durationMs === "number") parts.push(`时长 ${formatDuration(detail.durationMs)}`);
  const filePath = str(detail.filePath);
  if (filePath) parts.push(basename(filePath));
  const failureKind = entry.kind === "failure" ? str(detail.kind) : null;
  if (failureKind && FAILURE_KIND_LABEL[failureKind]) parts.push(FAILURE_KIND_LABEL[failureKind]);
  const url = str(detail.url);
  if (url) parts.push(url);
  return parts.length ? parts.join(" · ") : null;
}

/** 按 id 去重并倒序合并。 */
function merge(current: ActivityEntry[], incoming: ActivityEntry[]): ActivityEntry[] {
  const seen = new Set(current.map((entry) => entry.id));
  const added = incoming.filter((entry) => !seen.has(entry.id));
  if (added.length === 0) return current;
  return [...current, ...added].sort((a, b) => b.id - a.id);
}

interface Filter {
  taskId: TaskId | null;
  kind: ActivityKind | null;
}

function matches(entry: ActivityEntry, filter: Filter): boolean {
  return (
    (filter.taskId === null || entry.taskId === filter.taskId) &&
    (filter.kind === null || entry.kind === filter.kind)
  );
}

type LoadState = "loading" | "idle" | "loadingMore" | "error";

export function ActivityPage({ onNavigate }: ActivityPageProps) {
  const tasks = useTasks();
  const [filter, setFilter] = useState<Filter>({ taskId: null, kind: null });
  const [items, setItems] = useState<ActivityEntry[]>([]);
  const [nextBefore, setNextBefore] = useState<number | null>(null);
  const [state, setState] = useState<LoadState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [freshIds, setFreshIds] = useState<Set<number>>(new Set());
  const request = useRef(0);
  const filterRef = useRef(filter);
  const scrollRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLDivElement>(null);

  const fetchPage = useCallback((target: Filter, before: number | null) => {
    const id = ++request.current;
    setState(before === null ? "loading" : "loadingMore");
    setError(null);
    api
      .listActivity({
        taskId: target.taskId ?? undefined,
        kinds: target.kind ? [target.kind] : undefined,
        before: before ?? undefined,
        limit: PAGE_SIZE,
      })
      .then(
        (page) => {
          if (id !== request.current) return;
          setItems((current) => merge(current, page.items));
          setNextBefore(page.nextBefore);
          setState("idle");
        },
        (failure: unknown) => {
          if (id !== request.current) return;
          setError(String((failure as { message?: unknown })?.message ?? failure));
          setState("error");
        },
      );
  }, []);

  // 筛选变化：清空后重新读第一页。
  useEffect(() => {
    filterRef.current = filter;
    setItems([]);
    setNextBefore(null);
    scrollRef.current?.scrollTo({ top: 0 });
    fetchPage(filter, null);
  }, [filter, fetchPage]);

  // 实时插入：只插入符合当前筛选的条目。
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void api
      .onActivity((entry) => {
        if (!matches(entry, filterRef.current)) return;
        setItems((current) => merge(current, [entry]));
        setFreshIds((current) => new Set(current).add(entry.id));
      })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 滚到底部附近时加载下一页。
  useEffect(() => {
    const sentinel = sentinelRef.current;
    if (!sentinel || nextBefore === null || state !== "idle") return;
    const observer = new IntersectionObserver(
      (records) => {
        if (records.some((record) => record.isIntersecting)) fetchPage(filterRef.current, nextBefore);
      },
      { root: scrollRef.current, rootMargin: "200px" },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [nextBefore, state, fetchPage, items.length]);

  const taskById = useMemo(() => new Map(tasks.map((task) => [task.id, task])), [tasks]);
  const taskOptions = useMemo(
    () => tasks.slice().sort((a, b) => a.anchorName.localeCompare(b.anchorName, "zh-CN")),
    [tasks],
  );

  const days = useMemo(() => {
    const groups: { day: number; entries: ActivityEntry[] }[] = [];
    for (const entry of items) {
      const day = dayKey(entry.at);
      const last = groups[groups.length - 1];
      if (last && last.day === day) last.entries.push(entry);
      else groups.push({ day, entries: [entry] });
    }
    return groups;
  }, [items]);

  const filtered = filter.taskId !== null || filter.kind !== null;

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 px-5 pt-3.5">
        <h1 className="m-0 text-[17px] font-semibold">动态</h1>
        <span className="flex-1" />
        <ToggleGroup
          type="single"
          size="sm"
          spacing={0}
          variant="outline"
          value={filter.kind ?? "all"}
          onValueChange={(value) =>
            value && setFilter((current) => ({ ...current, kind: value === "all" ? null : (value as ActivityKind) }))
          }
          aria-label="按类型筛选"
        >
          <ToggleGroupItem value="all" className="data-[state=on]:bg-primary/10 data-[state=on]:text-primary">
            全部
          </ToggleGroupItem>
          {KIND_FILTERS.map((option) => (
            <ToggleGroupItem
              key={option.value}
              value={option.value}
              className="data-[state=on]:bg-primary/10 data-[state=on]:text-primary"
            >
              {option.label}
            </ToggleGroupItem>
          ))}
        </ToggleGroup>
        <Select
          value={filter.taskId === null ? "all" : String(filter.taskId)}
          onValueChange={(value) =>
            setFilter((current) => ({ ...current, taskId: value === "all" ? null : Number(value) }))
          }
        >
          <SelectTrigger size="sm" className="w-40" aria-label="按任务筛选">
            <SelectValue />
          </SelectTrigger>
          <SelectContent position="popper" align="end">
            <SelectItem value="all">全部任务</SelectItem>
            {taskOptions.length > 0 && <SelectSeparator />}
            {taskOptions.map((task) => (
              <SelectItem key={task.id} value={String(task.id)}>
                {task.anchorName}
              </SelectItem>
            ))}
            {/* 筛选中的任务被删除后仍保留选项，避免显示空白。 */}
            {filter.taskId !== null && !taskById.has(filter.taskId) && (
              <SelectItem value={String(filter.taskId)}>已删除的任务</SelectItem>
            )}
          </SelectContent>
        </Select>
      </header>

      <div
        ref={scrollRef}
        className="mx-5 mt-3.5 mb-4 flex min-h-0 flex-1 flex-col overflow-y-auto rounded-xl border bg-card"
      >
        {state === "loading" && items.length === 0 ? (
          <CenterNote>
            <Loader2 className="size-4 animate-spin" aria-hidden />
            正在读取动态…
          </CenterNote>
        ) : state === "error" && items.length === 0 ? (
          <ErrorNote message={error} onRetry={() => fetchPage(filter, null)} />
        ) : items.length === 0 ? (
          <EmptyNote filtered={filtered} onClear={() => setFilter({ taskId: null, kind: null })} />
        ) : (
          <>
            {days.map((group) => (
              <div key={group.day}>
                <div className="sticky top-0 z-10 border-b bg-background px-3.5 py-2 text-xs font-semibold text-muted-foreground">
                  {formatDay(group.day)}
                </div>
                {group.entries.map((entry) => (
                  <ActivityRow
                    key={entry.id}
                    entry={entry}
                    task={entry.taskId === null ? undefined : taskById.get(entry.taskId)}
                    fresh={freshIds.has(entry.id)}
                    onNavigate={onNavigate}
                  />
                ))}
              </div>
            ))}
            <div ref={sentinelRef} className="flex items-center justify-center gap-2 py-3 text-xs text-muted-foreground">
              {state === "loadingMore" ? (
                <>
                  <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  正在加载更早的动态…
                </>
              ) : state === "error" ? (
                <>
                  <span className="text-destructive">加载失败：{error}</span>
                  <Button variant="ghost" size="xs" onClick={() => fetchPage(filter, nextBefore)}>
                    重试
                  </Button>
                </>
              ) : nextBefore === null ? (
                "没有更早的动态了"
              ) : (
                <Button variant="ghost" size="xs" onClick={() => fetchPage(filter, nextBefore)}>
                  加载更多
                </Button>
              )}
            </div>
          </>
        )}
      </div>
    </section>
  );
}

interface ActivityRowProps {
  entry: ActivityEntry;
  task: TaskSnapshot | undefined;
  fresh: boolean;
  onNavigate: Navigate;
}

function ActivityRow({ entry, task, fresh, onNavigate }: ActivityRowProps) {
  const { icon: Icon, tone, label } = visual(entry);
  const description = describe(entry);
  const open = task ? () => onNavigate("rooms", { taskId: task.id }) : undefined;
  const name = task?.anchorName ?? entry.anchorName;

  return (
    <div
      className={cn(
        "grid grid-cols-[44px_28px_minmax(0,1fr)_auto] items-center gap-2.5 border-b px-3.5 py-2.5 last:border-b-0",
        fresh && "animate-in fade-in slide-in-from-top-1 duration-500",
      )}
    >
      <Tooltip>
        <TooltipTrigger asChild>
          <time dateTime={new Date(entry.at).toISOString()} className="text-xs text-muted-foreground tabular-nums">
            {formatClock(entry.at)}
          </time>
        </TooltipTrigger>
        <TooltipContent>{formatDateTime(entry.at)}</TooltipContent>
      </Tooltip>
      <span className={cn("grid size-7 place-items-center rounded-full", TONE_CLASS[tone])} title={label}>
        <Icon className="size-3.5" aria-hidden />
        <span className="sr-only">{label}</span>
      </span>
      <div className="min-w-0">
        <div className="truncate">
          {open ? (
            <button
              type="button"
              onClick={open}
              className="rounded-sm font-medium hover:text-primary hover:underline focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:outline-none"
              title="在直播间中查看"
            >
              {name}
            </button>
          ) : (
            <span className="font-medium text-muted-foreground" title="任务已删除">
              {name || "已删除的任务"}
            </span>
          )}{" "}
          <span>{entry.message}</span>
        </div>
        {description && <div className="truncate text-xs text-muted-foreground" title={description}>{description}</div>}
      </div>
      {open && tone === "failure" ? (
        <Button variant="outline" size="xs" onClick={open}>
          处理
        </Button>
      ) : (
        <span />
      )}
    </div>
  );
}

function CenterNote({ children }: { children: ReactNode }) {
  return <div className="flex flex-1 items-center justify-center gap-2 text-muted-foreground">{children}</div>;
}

function EmptyNote({ filtered, onClear }: { filtered: boolean; onClear: () => void }) {
  return (
    <div className="flex flex-1 items-center justify-center">
      <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-muted text-muted-foreground">
          <Radio className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">{filtered ? "没有符合条件的动态" : "暂无动态"}</p>
        <p className="m-0 text-muted-foreground">
          {filtered
            ? "换一个任务或类型试试。"
            : "开播、开始录制、录制结束、失败和你的操作会按时间顺序显示在这里。"}
        </p>
        {filtered && (
          <Button variant="outline" size="sm" className="mt-2" onClick={onClear}>
            清除筛选
          </Button>
        )}
      </div>
    </div>
  );
}

function ErrorNote({ message, onRetry }: { message: string | null; onRetry: () => void }) {
  return (
    <div className="flex flex-1 items-center justify-center">
      <div role="alert" className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-status-recording-soft text-destructive">
          <AlertTriangle className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">无法读取动态</p>
        <p className="m-0 break-all text-muted-foreground">{message}</p>
        <Button variant="outline" size="sm" className="mt-2" onClick={onRetry}>
          <RotateCw aria-hidden />
          重试
        </Button>
      </div>
    </div>
  );
}
