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
 * 页面标题行：三个平台都是 52px 的工具栏（标题和摘要上下排列，右侧是操作）。macOS 用 Overlay 标题栏，
 * 整条可拖动窗口（拖拽区只作用于带属性的这几层，按钮和输入框照常响应）；Windows 和 Linux 有系统标题栏，不设拖拽区。
 */
export function PageHeader({ title, summary, actions, className }: PageHeaderProps) {
  return (
    <header
      {...dragRegion}
      className={cn(
        "flex h-(--header-h) min-w-0 shrink-0 items-center gap-2 border-b pr-3.5 pl-(--page-pad)",
        className,
      )}
    >
      <div {...dragRegion} className="flex min-w-0 flex-col">
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
