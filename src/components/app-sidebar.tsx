import { dragRegion } from "@/lib/platform";
import { cn } from "@/lib/utils";
import { pages, type PageId } from "@/pages/pages";

interface AppSidebarProps {
  current: PageId;
  onNavigate: (page: PageId) => void;
  /** 正在录制的任务数，显示在「直播间」旁。 */
  recordingCount: number;
}

/**
 * 左侧导航。Windows 落在 Mica 上，选中项左侧有 Teal 竖条；macOS 透出侧栏 vibrancy，顶部留给红绿灯；
 * Linux 是平涂灰色侧栏。三个平台都不放品牌标识（标题栏或菜单栏已经显示应用名）。
 */
export function AppSidebar({ current, onNavigate, recordingCount }: AppSidebarProps) {
  return (
    <aside
      {...dragRegion}
      className={cn(
        "flex min-h-0 flex-col gap-0.5 bg-sidebar px-1.5 pt-1 pb-2 text-sidebar-foreground",
        "mac:border-r mac:border-sidebar-border mac:px-2.5 mac:pt-(--header-h)",
        "linux:border-r linux:border-sidebar-border linux:px-2 linux:pt-2",
      )}
    >
      <nav className="flex flex-col gap-0.5" aria-label="主导航">
        {pages.map((page) => {
          const Icon = page.icon;
          const active = page.id === current;
          return (
            <button
              key={page.id}
              type="button"
              aria-current={active ? "page" : undefined}
              onClick={() => onNavigate(page.id)}
              className={cn(
                "relative flex h-(--nav-item-h) items-center gap-3 rounded-md px-3 text-left text-(length:--text-ui) outline-none transition-colors",
                "hover:bg-sidebar-accent focus-visible:ring-2 focus-visible:ring-sidebar-ring focus-visible:ring-inset",
                "mac:gap-2 mac:px-2",
                active && "bg-sidebar-accent linux:font-semibold",
                active &&
                  "win:before:absolute win:before:top-1/2 win:before:left-0 win:before:h-4 win:before:w-[3px] win:before:-translate-y-1/2 win:before:rounded-full win:before:bg-primary",
              )}
            >
              <Icon className="size-4 shrink-0 mac:text-primary" aria-hidden />
              {page.label}
              {page.id === "rooms" && recordingCount > 0 && (
                <span
                  className="ml-auto flex items-center gap-1.5 text-(length:--text-small) text-status-recording tabular-nums"
                  title={`${recordingCount} 个直播间正在录制`}
                >
                  <span className="size-1.5 rounded-full bg-status-recording" aria-hidden />
                  {recordingCount}
                  <span className="sr-only">个正在录制</span>
                </span>
              )}
            </button>
          );
        })}
      </nav>
    </aside>
  );
}
