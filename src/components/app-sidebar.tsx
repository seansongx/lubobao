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
 * 左侧导航，三个平台样子相同（2026-11-24 起统一为原 Windows 的样子）：侧栏与窗口同底、不画分隔线，
 * 当前项淡灰底加左侧 Teal 竖条，图标随文字色。材质生效时侧栏透明（Windows 11 落在 Mica 上，macOS 透出 vibrancy）。
 * macOS 用 Overlay 标题栏，顶部留出工具栏高度给红绿灯；Windows 和 Linux 有系统标题栏，不留。
 * 三个平台都不放品牌标识（标题栏或菜单栏已经显示应用名）。
 */
export function AppSidebar({ current, onNavigate, recordingCount }: AppSidebarProps) {
  return (
    <aside
      {...dragRegion}
      className={cn(
        "flex min-h-0 flex-col gap-0.5 bg-sidebar px-1.5 pt-1 pb-2 text-sidebar-foreground",
        // 外壳差异：macOS 的红绿灯落在侧栏顶部
        "mac:pt-(--header-h)",
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
                "relative flex h-(--nav-item-h) items-center gap-3 rounded-md px-3 text-left text-nav outline-none transition-colors",
                "hover:bg-sidebar-accent focus-visible:ring-2 focus-visible:ring-ring",
                active && "bg-sidebar-accent selection-bar",
              )}
            >
              <Icon className="size-4 shrink-0" aria-hidden />
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
