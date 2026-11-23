import type { ReactNode } from "react";

import { dragRegion } from "@/lib/platform";
import { cn } from "@/lib/utils";

interface PageHeaderProps {
  title: string;
  /** 标题旁的次要信息（数量、占用空间等）。 */
  summary?: ReactNode;
  /** 右侧操作：搜索框、主操作按钮等。 */
  actions?: ReactNode;
  className?: string;
}

/**
 * 页面标题行。Windows 和 Linux 是内容区顶部的大标题；macOS 是 52px 的统一工具栏，
 * 整条可拖动窗口（拖拽区只作用于带属性的这几层，按钮和输入框照常响应）。
 */
export function PageHeader({ title, summary, actions, className }: PageHeaderProps) {
  return (
    <header
      {...dragRegion}
      className={cn(
        "flex min-w-0 shrink-0 items-center gap-3 px-(--page-pad) pt-5 linux:pt-4",
        "mac:h-(--header-h) mac:gap-2 mac:border-b mac:pt-0 mac:pr-3.5",
        className,
      )}
    >
      <div {...dragRegion} className="flex min-w-0 items-baseline gap-3 mac:flex-col mac:gap-0">
        <h1 {...dragRegion} className="m-0 shrink-0 text-(length:--text-title) leading-tight font-(--weight-title)">
          {title}
        </h1>
        {summary && (
          <div {...dragRegion} className="min-w-0 truncate text-(length:--text-small) text-muted-foreground">
            {summary}
          </div>
        )}
      </div>
      <span {...dragRegion} className="flex-1 self-stretch" />
      {actions}
    </header>
  );
}
