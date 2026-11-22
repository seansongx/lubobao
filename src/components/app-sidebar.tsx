import { Disc } from "lucide-react";

import { cn } from "@/lib/utils";
import { pages, type PageId } from "@/pages/pages";

interface AppSidebarProps {
  current: PageId;
  onNavigate: (page: PageId) => void;
}

export function AppSidebar({ current, onNavigate }: AppSidebarProps) {
  return (
    <aside className="flex flex-col gap-0.5 border-r border-sidebar-border bg-sidebar px-2.5 py-3.5 text-sidebar-foreground">
      <div className="flex items-center gap-2.5 px-2 pt-0.5 pb-4 text-sm font-semibold">
        <span className="grid size-[26px] place-items-center rounded-[7px] bg-sidebar-primary text-sidebar-primary-foreground">
          <Disc className="size-4" aria-hidden />
        </span>
        录播宝
      </div>
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
                "flex h-[34px] items-center gap-2.5 rounded-lg px-2.5 text-left text-muted-foreground outline-none transition-colors",
                "hover:bg-sidebar-accent hover:text-sidebar-accent-foreground",
                "focus-visible:ring-2 focus-visible:ring-sidebar-ring",
                active &&
                  "bg-sidebar-accent font-medium text-sidebar-accent-foreground shadow-[0_0_0_1px_var(--sidebar-border)]",
              )}
            >
              <Icon className={cn("size-[15px] shrink-0", active && "text-sidebar-primary")} aria-hidden />
              {page.label}
            </button>
          );
        })}
      </nav>
    </aside>
  );
}
