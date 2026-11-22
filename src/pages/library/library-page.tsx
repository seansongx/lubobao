// 录像库：按目录浏览保存目录（设置 live_save_path）。
// 目录、文件、排序和越界检查都由 Rust（src-tauri/src/library）完成，这里只展示和收集操作。

import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import {
  AlertTriangle,
  ArrowUp,
  FileVideo,
  Folder,
  FolderOpen,
  FolderSearch,
  Layers,
  Loader2,
  Play,
  RotateCw,
  Settings,
  X,
} from "lucide-react";

import { formatBytes, formatDateTime } from "@/components/format";
import type { Navigate } from "@/components/navigation";
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useLibraryStats } from "@/components/use-library-stats";
import { api, isLibraryError, type LibraryEntry, type LibraryError, type LibraryListing } from "@/lib/api";
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

const collator = new Intl.Collator("zh-CN", { numeric: true });

/**
 * 同一次录制的多个分段前插入一行分组标题，分段缩进显示。
 * 文件保持 Rust 给出的顺序；目录改用中文排序规则（Rust 按码位比较，「快手」会排在「抖音」前面）。
 */
function buildRows(listed: LibraryEntry[]): Row[] {
  const dirs = listed.filter((entry) => entry.kind === "dir").sort((a, b) => collator.compare(a.name, b.name));
  const entries = [...dirs, ...listed.filter((entry) => entry.kind === "file")];
  const counts = new Map<string, number>();
  for (const entry of entries) {
    if (entry.kind === "file" && entry.segment !== null && entry.group) {
      counts.set(entry.group, (counts.get(entry.group) ?? 0) + 1);
    }
  }
  const rows: Row[] = [];
  let currentGroup: string | null = null;
  for (const entry of entries) {
    const group = entry.kind === "file" && entry.group && (counts.get(entry.group) ?? 0) > 1 ? entry.group : null;
    if (group && group !== currentGroup) {
      const members = entries.filter((item) => item.kind === "file" && item.group === group);
      rows.push({
        type: "group",
        key: `group:${group}`,
        name: group.replace(/\.[^.]+$/, ""),
        count: members.length,
        size: members.reduce((sum, item) => sum + (item.size ?? 0), 0),
        latest: Math.max(...members.map((item) => item.modifiedAt ?? 0)) || null,
      });
    }
    currentGroup = group;
    rows.push({ type: "entry", entry, nested: group !== null });
  }
  return rows;
}

export function LibraryPage({ onNavigate }: LibraryPageProps) {
  const [path, setPath] = useState("");
  const [load, setLoad] = useState<Load>({ state: "loading", previous: null });
  const [selected, setSelected] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const { stats, refresh: refreshStats } = useLibraryStats();
  const request = useRef(0);
  const listRef = useRef<HTMLDivElement>(null);

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
    refreshStats();
  };

  const listing = load.state === "ready" ? load.listing : load.state === "loading" ? load.previous : null;
  const rows = useMemo(() => buildRows(listing?.entries ?? []), [listing]);
  const entries = useMemo(
    () => rows.flatMap((row) => (row.type === "entry" ? [row.entry] : [])),
    [rows],
  );

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

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Backspace" && path) {
      event.preventDefault();
      go(parentOf(path));
      return;
    }
    if (entries.length === 0) return;
    const index = entries.findIndex((entry) => entry.path === selected);
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const next =
        event.key === "ArrowDown" ? Math.min(entries.length - 1, index + 1) : Math.max(0, index < 0 ? 0 : index - 1);
      setSelected(entries[next].path);
      listRef.current
        ?.querySelector(`[data-path="${CSS.escape(entries[next].path)}"]`)
        ?.scrollIntoView({ block: "nearest" });
    } else if (event.key === "Enter" && index >= 0) {
      event.preventDefault();
      activate(entries[index]);
    }
  };

  const root = listing?.root ?? (load.state === "error" ? load.error.root : stats?.root) ?? "";
  const rootBroken = load.state === "error" && load.error.kind !== "not_found";
  // 根目录失效时只显示根目录，重新检查也从根目录开始。
  const parts = path && !rootBroken ? path.split("/") : [];

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col">
      <header className="flex items-center gap-3 px-5 pt-3.5">
        <h1 className="m-0 text-[17px] font-semibold">录像库</h1>
        <LibrarySummary
          usedBytes={stats?.rootExists ? stats.usedBytes : null}
          fileCount={stats?.rootExists ? stats.fileCount : null}
          availableBytes={stats?.availableBytes ?? null}
          totalBytes={stats?.totalBytes ?? null}
        />
        <span className="flex-1" />
        <Tooltip>
          <TooltipTrigger asChild>
            <Button variant="ghost" size="icon-sm" onClick={reload} aria-label="刷新">
              <RotateCw className={cn(load.state === "loading" && "animate-spin")} aria-hidden />
            </Button>
          </TooltipTrigger>
          <TooltipContent>刷新</TooltipContent>
        </Tooltip>
        <Button variant="outline" size="sm" onClick={() => openFolder(path)} disabled={rootBroken}>
          <FolderOpen aria-hidden />
          打开目录
        </Button>
      </header>

      <div className="flex min-w-0 items-center gap-1.5 px-5 pt-3 pb-2">
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
          className="mx-5 mb-2 flex items-start gap-2 rounded-lg border border-destructive/30 bg-status-recording-soft px-3 py-2 text-[13px]"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-destructive" aria-hidden />
          <span className="min-w-0 flex-1 break-all">{notice}</span>
          <Button variant="ghost" size="icon-xs" onClick={() => setNotice(null)} aria-label="关闭提示">
            <X aria-hidden />
          </Button>
        </div>
      )}

      <div className="mx-5 mb-4 flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border bg-card">
        {load.state === "error" ? (
          <LibraryErrorView error={load.error} onNavigate={onNavigate} onRetry={rootBroken && path ? () => go("") : reload} onRoot={() => go("")} />
        ) : !listing ? (
          <div className="flex flex-1 items-center justify-center gap-2 text-muted-foreground">
            <Loader2 className="size-4 animate-spin" aria-hidden />
            正在读取录像目录…
          </div>
        ) : listing.entries.length === 0 ? (
          <EmptyFolder atRoot={listing.path === ""} root={listing.root} />
        ) : (
          <>
            <div className="grid grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b px-3 py-1.5 text-xs text-muted-foreground">
              <span>名称</span>
              <span className="text-right">大小</span>
              <span>修改时间</span>
              <span className="sr-only">操作</span>
            </div>
            <div
              ref={listRef}
              role="listbox"
              aria-label="录像文件"
              tabIndex={0}
              onKeyDown={onKeyDown}
              className="min-h-0 flex-1 overflow-y-auto outline-none focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:ring-inset"
            >
              {rows.map((row) =>
                row.type === "group" ? (
                  <GroupRow key={row.key} row={row} />
                ) : (
                  <EntryRow
                    key={row.entry.path}
                    entry={row.entry}
                    nested={row.nested}
                    selected={selected === row.entry.path}
                    onSelect={() => setSelected(row.entry.path)}
                    onActivate={() => activate(row.entry)}
                    onReveal={() => reveal(row.entry.path)}
                  />
                ),
              )}
            </div>
          </>
        )}
      </div>
    </section>
  );
}

function LibrarySummary(props: {
  usedBytes: number | null;
  fileCount: number | null;
  availableBytes: number | null;
  totalBytes: number | null;
}) {
  return (
    <span className="flex min-w-0 items-center gap-3 truncate text-xs text-muted-foreground tabular-nums">
      {props.usedBytes !== null && (
        <span>
          已占用 <span className="text-foreground">{formatBytes(props.usedBytes)}</span>
          {props.fileCount !== null && ` · ${props.fileCount} 个录像`}
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

function GroupRow({ row }: { row: Extract<Row, { type: "group" }> }) {
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground">
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
  selected: boolean;
  onSelect: () => void;
  onActivate: () => void;
  onReveal: () => void;
}

function EntryRow({ entry, nested, selected, onSelect, onActivate, onReveal }: EntryRowProps) {
  const isDir = entry.kind === "dir";
  return (
    <div
      role="option"
      aria-selected={selected}
      data-path={entry.path}
      onClick={onSelect}
      onDoubleClick={onActivate}
      title={isDir ? "双击进入文件夹" : "双击用系统播放器打开"}
      className={cn(
        "group grid h-(--task-row-height) cursor-default grid-cols-[minmax(0,1fr)_88px_112px_64px] items-center gap-3 border-b px-3 select-none last:border-b-0 hover:bg-accent",
        selected && "bg-accent",
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

function RowAction(props: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon-xs"
          aria-label={props.label}
          onClick={(event) => {
            event.stopPropagation();
            props.onClick();
          }}
          onDoubleClick={(event) => event.stopPropagation()}
        >
          {props.children}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{props.label}</TooltipContent>
    </Tooltip>
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
  onNavigate: Navigate;
  onRetry: () => void;
  onRoot: () => void;
}

function LibraryErrorView({ error, onNavigate, onRetry, onRoot }: LibraryErrorViewProps) {
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
        return {
          title: "没有权限读取",
          description: `${error.message}。请检查文件夹权限，或到设置中换一个保存目录。`,
          settings: true,
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
