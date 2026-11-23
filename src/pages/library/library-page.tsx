// 录像库：按目录浏览保存目录（设置 live_save_path）。
// 目录、文件、排序和越界检查都由 Rust（src-tauri/src/library）完成，这里只展示和收集操作。

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import {
  AlertTriangle,
  ArrowUp,
  FileVideo,
  Folder,
  FolderOpen,
  FolderSearch,
  Layers,
  Copy,
  Loader2,
  Play,
  RotateCw,
  Settings,
  X,
} from "lucide-react";
import { toast } from "sonner";

import { formatBytes, formatDateTime } from "@/components/format";
import type { Navigate } from "@/components/navigation";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useDiskSpace, useLibraryUsage } from "@/components/use-library-space";
import {
  api,
  isLibraryError,
  type LibraryEntry,
  type LibraryError,
  type LibraryListing,
  type UsageSnapshot,
} from "@/lib/api";
import { cn } from "@/lib/utils";

interface LibraryPageProps {
  onNavigate: Navigate;
}

type Load =
  | { state: "loading"; previous: LibraryListing | null }
  | { state: "ready"; listing: LibraryListing }
  | { state: "error"; error: LibraryError };

function toLibraryError(error: unknown): LibraryError {
  if (isLibraryError(error)) return error;
  return { kind: "io", message: String((error as { message?: unknown })?.message ?? error), root: "" };
}

function parentOf(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? "" : path.slice(0, index);
}

type Row =
  | { type: "entry"; entry: LibraryEntry; nested: boolean }
  | { type: "group"; key: string; name: string; count: number; size: number; latest: number | null };

/** 行高固定，虚拟滚动据此计算位置。 */
const ENTRY_HEIGHT = 44;
const GROUP_HEIGHT = 32;
/** 可视区上下多渲染的高度。 */
const OVERSCAN_PX = 400;

const rowHeight = (row: Row) => (row.type === "group" ? GROUP_HEIGHT : ENTRY_HEIGHT);

const collator = new Intl.Collator("zh-CN", { numeric: true });

/**
 * 同一次录制的多个分段前插入一行分组标题，分段缩进显示。线性复杂度：先一遍汇总各组，再一遍生成行。
 * 文件保持 Rust 给出的顺序（同组分段相邻）；目录改用中文排序规则（Rust 按码位比较，「快手」会排在「抖音」前）。
 */
function buildRows(listed: LibraryEntry[]): Row[] {
  const dirs = listed.filter((entry) => entry.kind === "dir").sort((a, b) => collator.compare(a.name, b.name));
  const files = listed.filter((entry) => entry.kind === "file");
  const groups = new Map<string, { count: number; size: number; latest: number }>();
  for (const file of files) {
    if (file.segment === null || !file.group) continue;
    const group = groups.get(file.group) ?? { count: 0, size: 0, latest: 0 };
    group.count += 1;
    group.size += file.size ?? 0;
    group.latest = Math.max(group.latest, file.modifiedAt ?? 0);
    groups.set(file.group, group);
  }
  const rows: Row[] = dirs.map((entry) => ({ type: "entry", entry, nested: false }));
  let currentGroup: string | null = null;
  for (const file of files) {
    const summary = file.group ? groups.get(file.group) : undefined;
    const group = summary && summary.count > 1 ? file.group : null;
    if (group && summary && group !== currentGroup) {
      rows.push({
        type: "group",
        key: `group:${group}`,
        name: group.replace(/\.[^.]+$/, ""),
        count: summary.count,
        size: summary.size,
        latest: summary.latest || null,
      });
    }
    currentGroup = group;
    rows.push({ type: "entry", entry: file, nested: group !== null });
  }
  return rows;
}

/** 根目录本身的问题：只显示根目录面包屑，提示去设置修改。 */
function isRootError(error: LibraryError, path: string): boolean {
  return error.kind === "root_missing" || error.kind === "root_not_directory" || (path === "" && error.kind !== "not_found");
}

export function LibraryPage({ onNavigate }: LibraryPageProps) {
  const [path, setPath] = useState("");
  const [load, setLoad] = useState<Load>({ state: "loading", previous: null });
  const [selected, setSelected] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const { space } = useDiskSpace();
  const { snapshot: usage, refresh: refreshUsage } = useLibraryUsage();
  const request = useRef(0);
  const listRef = useRef<HTMLDivElement>(null);
  const [scroll, setScroll] = useState({ top: 0, height: 0 });

  const fetchListing = useCallback((target: string) => {
    const id = ++request.current;
    setLoad((current) => ({
      state: "loading",
      previous: current.state === "ready" ? current.listing : current.state === "loading" ? current.previous : null,
    }));
    api.listLibrary(target).then(
      (listing) => {
        if (id === request.current) setLoad({ state: "ready", listing });
      },
      (error: unknown) => {
        if (id === request.current) setLoad({ state: "error", error: toLibraryError(error) });
      },
    );
  }, []);

  useEffect(() => {
    fetchListing(path);
  }, [path, fetchListing]);

  const go = (target: string) => {
    setNotice(null);
    setSelected(null);
    setPath(target);
  };

  const reload = () => {
    setNotice(null);
    fetchListing(path);
    refreshUsage();
  };

  const listing = load.state === "ready" ? load.listing : load.state === "loading" ? load.previous : null;
  const rows = useMemo(() => buildRows(listing?.entries ?? []), [listing]);
  // 每行的起始位置；entryRows 记录可选行在 rows 中的下标，供键盘导航。
  const layout = useMemo(() => {
    const offsets = new Array<number>(rows.length + 1);
    offsets[0] = 0;
    rows.forEach((row, index) => {
      offsets[index + 1] = offsets[index] + rowHeight(row);
    });
    const entryRows = rows.flatMap((row, index) => (row.type === "entry" ? [index] : []));
    return { offsets, entryRows };
  }, [rows]);

  // 切换目录时回到顶部；跟踪列表可视区的大小。
  useLayoutEffect(() => {
    const list = listRef.current;
    if (!list) return;
    list.scrollTop = 0;
    setScroll({ top: 0, height: list.clientHeight });
    const observer = new ResizeObserver(() => setScroll({ top: list.scrollTop, height: list.clientHeight }));
    observer.observe(list);
    return () => observer.disconnect();
  }, [listing?.path, rows.length > 0]);

  // 只渲染可视区附近的行（二分查找起点）。
  const visible = useMemo(() => {
    const { offsets } = layout;
    const from = Math.max(0, scroll.top - OVERSCAN_PX);
    const to = scroll.top + scroll.height + OVERSCAN_PX;
    let low = 0;
    let high = rows.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (offsets[mid + 1] <= from) low = mid + 1;
      else high = mid;
    }
    let end = low;
    while (end < rows.length && offsets[end] < to) end += 1;
    return { start: low, end };
  }, [layout, rows.length, scroll]);

  const runAction = (action: Promise<void>) => {
    action.then(
      () => setNotice(null),
      (error: unknown) => {
        const failure = toLibraryError(error);
        setNotice(failure.message);
        // 文件已被删除或目录失效时刷新列表。
        if (failure.kind === "not_found" || failure.kind === "root_missing") fetchListing(path);
      },
    );
  };

  const activate = (entry: LibraryEntry) => {
    if (entry.kind === "dir") go(entry.path);
    else runAction(api.openLibraryFile(entry.path));
  };

  const reveal = (target: string) => runAction(api.revealLibraryItem(target));
  const openFolder = (target: string) => runAction(api.openLibraryFolder(target));

  const [menuEntry, setMenuEntry] = useState<LibraryEntry | null>(null);
  // 整个列表共用一个右键菜单：按右键所在行（data-path）决定内容，空白处不弹。
  const onListContextMenu = (event: React.MouseEvent) => {
    const row = event.target instanceof Element ? event.target.closest<HTMLElement>("[data-path]") : null;
    const entry = row ? listing?.entries.find((item) => item.path === row.dataset.path) : undefined;
    if (!entry) {
      event.preventDefault();
      return;
    }
    setSelected(entry.path);
    setMenuEntry(entry);
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Backspace" && path) {
      event.preventDefault();
      go(parentOf(path));
      return;
    }
    const { entryRows, offsets } = layout;
    if (entryRows.length === 0) return;
    const entryAt = (position: number) => (rows[entryRows[position]] as Extract<Row, { type: "entry" }>).entry;
    const index = entryRows.findIndex((rowIndex) => (rows[rowIndex] as Extract<Row, { type: "entry" }>).entry.path === selected);
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const next =
        event.key === "ArrowDown" ? Math.min(entryRows.length - 1, index + 1) : Math.max(0, index < 0 ? 0 : index - 1);
      setSelected(entryAt(next).path);
      // 行可能还没渲染，按计算的位置滚动到可见。
      const list = listRef.current;
      const rowIndex = entryRows[next];
      if (list) {
        const top = offsets[rowIndex];
        const bottom = offsets[rowIndex + 1];
        if (top < list.scrollTop) list.scrollTop = top;
        else if (bottom > list.scrollTop + list.clientHeight) list.scrollTop = bottom - list.clientHeight;
      }
    } else if (event.key === "Enter" && index >= 0) {
      event.preventDefault();
      activate(entryAt(index));
    }
  };

  const root = listing?.root ?? (load.state === "error" ? load.error.root : space?.root) ?? "";
  const rootBroken = load.state === "error" && isRootError(load.error, path);
  // 根目录失效时只显示根目录，重新检查也从根目录开始。
  const parts = path && !rootBroken ? path.split("/") : [];

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col">
      <PageHeader
        title="录像库"
        summary={
          <LibrarySummary
            usage={usage?.rootExists ? usage : null}
            availableBytes={space?.availableBytes ?? null}
            totalBytes={space?.totalBytes ?? null}
          />
        }
        actions={
          <>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon" onClick={reload} aria-label="刷新">
                  <RotateCw className={cn(load.state === "loading" && "animate-spin")} aria-hidden />
                </Button>
              </TooltipTrigger>
              <TooltipContent>刷新</TooltipContent>
            </Tooltip>
            <Button variant="outline" onClick={() => openFolder(path)} disabled={rootBroken}>
              <FolderOpen aria-hidden />
              打开目录
            </Button>
          </>
        }
      />

      <div className="flex min-w-0 items-center gap-1.5 px-(--page-pad) pt-3 pb-2 mac:pt-2.5">
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={() => go(parentOf(path))}
              disabled={!path}
              aria-label="返回上级"
            >
              <ArrowUp aria-hidden />
            </Button>
          </TooltipTrigger>
          <TooltipContent>返回上级（Backspace）</TooltipContent>
        </Tooltip>
        <Breadcrumb className="min-w-0">
          <BreadcrumbList className="flex-nowrap text-[13px]">
            <BreadcrumbItem className="min-w-0">
              <CrumbButton current={parts.length === 0} title={root} onClick={() => go("")}>
                {root || "录像目录"}
              </CrumbButton>
            </BreadcrumbItem>
            {parts.map((part, index) => (
              <CrumbSegment
                key={parts.slice(0, index + 1).join("/")}
                label={part}
                current={index === parts.length - 1}
                onClick={() => go(parts.slice(0, index + 1).join("/"))}
              />
            ))}
          </BreadcrumbList>
        </Breadcrumb>
      </div>

      {notice && (
        <div
          role="alert"
          className="mx-(--page-pad) mb-2 flex items-start gap-2 rounded-lg border border-destructive/30 bg-status-recording-soft px-3 py-2 text-[13px] select-text"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-destructive" aria-hidden />
          <span className="min-w-0 flex-1 break-all">{notice}</span>
          <Button variant="ghost" size="icon-xs" onClick={() => setNotice(null)} aria-label="关闭提示">
            <X aria-hidden />
          </Button>
        </div>
      )}

      <div className="mx-3 mb-2 flex min-h-0 flex-1 flex-col overflow-hidden text-(length:--text-dense) mac:mx-2.5 linux:mx-(--page-pad) linux:mb-3 linux:rounded-xl linux:border linux:bg-card linux:shadow-xs">
        {load.state === "error" ? (
          <LibraryErrorView
            error={load.error}
            atRoot={rootBroken}
            onNavigate={onNavigate}
            onRetry={rootBroken && path ? () => go("") : reload}
            onRoot={() => go("")}
          />
        ) : !listing ? (
          <div className="flex flex-1 items-center justify-center gap-2 text-muted-foreground">
            <Loader2 className="size-4 animate-spin" aria-hidden />
            正在读取录像目录…
          </div>
        ) : listing.entries.length === 0 ? (
          <EmptyFolder atRoot={listing.path === ""} root={listing.root} />
        ) : (
          <>
            <div className="grid h-8 grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b px-3 text-xs text-muted-foreground mac:h-7 mac:font-medium linux:font-semibold">
              <span>名称</span>
              <span className="text-right">大小</span>
              <span>修改时间</span>
              <span className="sr-only">操作</span>
            </div>
            <ContextMenu onOpenChange={(open) => !open && setMenuEntry(null)}>
              <ContextMenuTrigger asChild onContextMenu={onListContextMenu}>
              <div
                ref={listRef}
                role="listbox"
                aria-label="录像文件"
                tabIndex={0}
                onKeyDown={onKeyDown}
                onScroll={(event) =>
                  setScroll({ top: event.currentTarget.scrollTop, height: event.currentTarget.clientHeight })
                }
                className="relative min-h-0 flex-1 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:ring-inset"
              >
                <div style={{ height: layout.offsets[rows.length] }}>
                  {rows.slice(visible.start, visible.end).map((row, offset) => {
                    const index = visible.start + offset;
                    const style = { top: layout.offsets[index] };
                    return row.type === "group" ? (
                      <GroupRow key={row.key} row={row} style={style} />
                    ) : (
                      <EntryRow
                        key={row.entry.path}
                        entry={row.entry}
                        nested={row.nested}
                        style={style}
                        selected={selected === row.entry.path}
                        onSelect={() => setSelected(row.entry.path)}
                        onActivate={() => activate(row.entry)}
                        onReveal={() => reveal(row.entry.path)}
                      />
                    );
                  })}
                </div>
              </div>
              </ContextMenuTrigger>
              <ContextMenuContent className="min-w-52">
                {menuEntry && (
                  <>
                    <ContextMenuItem onSelect={() => activate(menuEntry)}>
                      {menuEntry.kind === "dir" ? <FolderOpen aria-hidden /> : <Play aria-hidden />}
                      {menuEntry.kind === "dir" ? "进入文件夹" : "用系统播放器打开"}
                    </ContextMenuItem>
                    <ContextMenuItem onSelect={() => reveal(menuEntry.path)}>
                      <FolderSearch aria-hidden />
                      在文件夹中显示
                    </ContextMenuItem>
                    <ContextMenuSeparator />
                    <ContextMenuItem onSelect={() => void copyPath(root, menuEntry.path)}>
                      <Copy aria-hidden />
                      复制路径
                    </ContextMenuItem>
                  </>
                )}
              </ContextMenuContent>
            </ContextMenu>
          </>
        )}
      </div>
    </section>
  );
}

function LibrarySummary(props: {
  usage: UsageSnapshot | null;
  availableBytes: number | null;
  totalBytes: number | null;
}) {
  const { usage } = props;
  return (
    <span className="flex min-w-0 items-center gap-3 truncate text-xs text-muted-foreground tabular-nums">
      {usage && (
        <span
          title={
            usage.computedAt
              ? `统计于 ${formatDateTime(usage.computedAt)}${usage.computing ? "，正在重新统计" : ""}`
              : "正在统计"
          }
        >
          已占用{" "}
          {usage.usage ? (
            <>
              <span className="text-foreground">{formatBytes(usage.usage.usedBytes)}</span>
              {` · ${usage.usage.fileCount} 个录像`}
            </>
          ) : (
            "—"
          )}
          {usage.computing && (
            <span className="ml-1.5 inline-flex items-center gap-1">
              <Loader2 className="size-3 animate-spin" aria-hidden />
              统计中
            </span>
          )}
        </span>
      )}
      {props.availableBytes !== null && (
        <span>
          磁盘剩余 <span className="text-foreground">{formatBytes(props.availableBytes)}</span>
          {props.totalBytes !== null && ` / ${formatBytes(props.totalBytes)}`}
        </span>
      )}
    </span>
  );
}

function CrumbButton(props: { current: boolean; title?: string; onClick: () => void; children: string }) {
  if (props.current) {
    return (
      <BreadcrumbPage className="block max-w-[28rem] truncate px-1.5" title={props.title}>
        {props.children}
      </BreadcrumbPage>
    );
  }
  return (
    <button
      type="button"
      title={props.title}
      onClick={props.onClick}
      className="block max-w-[16rem] truncate rounded px-1.5 py-0.5 text-muted-foreground hover:bg-muted hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:outline-none"
    >
      {props.children}
    </button>
  );
}

function CrumbSegment(props: { label: string; current: boolean; onClick: () => void }) {
  return (
    <>
      <BreadcrumbSeparator />
      <BreadcrumbItem className="min-w-0">
        <CrumbButton current={props.current} onClick={props.onClick}>
          {props.label}
        </CrumbButton>
      </BreadcrumbItem>
    </>
  );
}

function GroupRow({ row, style }: { row: Extract<Row, { type: "group" }>; style: CSSProperties }) {
  return (
    <div
      style={style}
      className="absolute inset-x-0 grid h-8 grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b bg-muted/60 px-3 text-xs text-muted-foreground"
    >
      <span className="flex min-w-0 items-center gap-2">
        <Layers className="size-3.5 shrink-0" aria-hidden />
        <span className="truncate" title={row.name}>
          {row.name}
        </span>
        <span className="shrink-0">· {row.count} 段</span>
      </span>
      <span className="text-right tabular-nums">{formatBytes(row.size)}</span>
      <span className="tabular-nums">{formatDateTime(row.latest)}</span>
      <span />
    </div>
  );
}

interface EntryRowProps {
  entry: LibraryEntry;
  nested: boolean;
  style: CSSProperties;
  selected: boolean;
  onSelect: () => void;
  onActivate: () => void;
  onReveal: () => void;
}

function EntryRow({ entry, nested, style, selected, onSelect, onActivate, onReveal }: EntryRowProps) {
  const isDir = entry.kind === "dir";
  return (
    <div
      role="option"
      aria-selected={selected}
      data-path={entry.path}
      style={style}
      onClick={onSelect}
      onDoubleClick={onActivate}
      title={isDir ? "双击进入文件夹" : "双击用系统播放器打开"}
      className={cn(
        "group absolute inset-x-0 grid h-11 cursor-default grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b px-3 select-none hover:bg-accent",
        selected && "bg-selection hover:bg-selection",
      )}
    >
      <span className={cn("flex min-w-0 items-center gap-2.5", nested && "pl-5")}>
        {isDir ? (
          <Folder className="size-4 shrink-0 text-primary" aria-hidden />
        ) : (
          <FileVideo className="size-4 shrink-0 text-muted-foreground" aria-hidden />
        )}
        <span className="truncate">{entry.name}</span>
        {entry.segment !== null && nested && (
          <span className="shrink-0 rounded bg-muted px-1.5 text-[11px] text-muted-foreground tabular-nums">
            第 {entry.segment + 1} 段
          </span>
        )}
      </span>
      <span className="text-right text-muted-foreground tabular-nums">
        {isDir ? (entry.childCount === null ? "—" : `${entry.childCount} 项`) : formatBytes(entry.size)}
      </span>
      <span className="text-muted-foreground tabular-nums">{formatDateTime(entry.modifiedAt)}</span>
      <span
        className={cn(
          "flex justify-end gap-0.5 opacity-0 group-hover:opacity-100 focus-within:opacity-100",
          selected && "opacity-100",
        )}
      >
        <RowAction label={isDir ? "进入文件夹" : "用系统播放器打开"} onClick={onActivate}>
          {isDir ? <FolderOpen aria-hidden /> : <Play aria-hidden />}
        </RowAction>
        <RowAction label="在文件夹中显示" onClick={onReveal}>
          <FolderSearch aria-hidden />
        </RowAction>
      </span>
    </div>
  );
}

/** 行内按钮用原生 title 提示：列表可能有几千行，不给每行挂两个 Radix tooltip。 */
function RowAction(props: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <Button
      variant="ghost"
      size="icon-xs"
      aria-label={props.label}
      title={props.label}
      onClick={(event) => {
        event.stopPropagation();
        props.onClick();
      }}
      onDoubleClick={(event) => event.stopPropagation()}
    >
      {props.children}
    </Button>
  );
}

function EmptyFolder({ atRoot, root }: { atRoot: boolean; root: string }) {
  return (
    <div className="flex flex-1 items-center justify-center">
      <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-muted text-muted-foreground">
          <FolderOpen className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">{atRoot ? "还没有录像" : "这个文件夹是空的"}</p>
        <p className="m-0 break-all text-muted-foreground">
          {atRoot
            ? `录制完成的文件会按“平台/主播”保存在 ${root}，并显示在这里。`
            : "这里没有子文件夹，也没有录像文件。"}
        </p>
      </div>
    </div>
  );
}

interface LibraryErrorViewProps {
  error: LibraryError;
  /** 错误出在根目录本身；只有这时才建议到设置中换保存目录。 */
  atRoot: boolean;
  onNavigate: Navigate;
  onRetry: () => void;
  onRoot: () => void;
}

function LibraryErrorView({ error, atRoot, onNavigate, onRetry, onRoot }: LibraryErrorViewProps) {
  const view = (() => {
    switch (error.kind) {
      case "root_missing":
        return {
          title: "找不到录像目录",
          description: `保存目录 ${error.root} 不存在。可能是移动硬盘没有连接，或者目录被移动、删除。请到设置中修改保存目录。`,
          settings: true,
        };
      case "root_not_directory":
        return {
          title: "保存目录不是文件夹",
          description: `${error.root} 是一个文件，不能用来保存录像。请到设置中修改保存目录。`,
          settings: true,
        };
      case "permission_denied":
        return atRoot
          ? {
              title: "没有权限读取录像目录",
              description: `${error.message}。请检查文件夹权限，或到设置中换一个保存目录。`,
              settings: true,
            }
          : {
              title: "没有权限读取这个文件夹",
              description: `${error.message}。请检查这个文件夹的权限。`,
              root: true,
            };
      case "not_found":
        return { title: "这个文件夹已不存在", description: error.message, root: true };
      default:
        return { title: "无法读取录像目录", description: error.message };
    }
  })();

  return (
    <div className="flex flex-1 items-center justify-center">
      <div role="alert" className="flex max-w-md flex-col items-center gap-2 px-6 py-14 text-center">
        <span className="mb-1 grid size-10 place-items-center rounded-full bg-status-recording-soft text-destructive">
          <AlertTriangle className="size-5" aria-hidden />
        </span>
        <p className="m-0 text-sm font-medium">{view.title}</p>
        <p className="m-0 break-all text-muted-foreground">{view.description}</p>
        <div className="mt-2 flex gap-2">
          {"settings" in view && view.settings && (
            <Button size="sm" onClick={() => onNavigate("settings")}>
              <Settings aria-hidden />
              前往设置
            </Button>
          )}
          {"root" in view && view.root ? (
            <Button size="sm" onClick={onRoot}>
              返回录像目录
            </Button>
          ) : (
            <Button variant="outline" size="sm" onClick={onRetry}>
              <RotateCw aria-hidden />
              重新检查
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

/** 复制录像的完整路径（保存目录 + 相对路径，分隔符跟随保存目录）。 */
async function copyPath(root: string, relative: string): Promise<void> {
  const separator = root.includes("\\") ? "\\" : "/";
  const full = root ? `${root.replace(/[\\/]+$/, "")}${separator}${relative.split("/").join(separator)}` : relative;
  try {
    await navigator.clipboard.writeText(full);
    toast.success("已复制路径", { description: full });
  } catch (error) {
    toast.error("复制失败", { description: String(error) });
  }
}
