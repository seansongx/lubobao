import type { TaskSnapshot } from "@/lib/api";
import { cn } from "@/lib/utils";

type AnchorLike = Pick<TaskSnapshot, "anchorName">;

export function AnchorAvatar({ task, className }: { task: AnchorLike; className?: string }) {
  const initial = Array.from(task.anchorName.trim())[0] ?? "?";
  return (
    <span
      className={cn(
        "grid size-[26px] shrink-0 place-items-center rounded-full bg-primary/15 text-xs font-semibold text-primary",
        className,
      )}
      aria-hidden
    >
      {initial}
    </span>
  );
}

/** 平台标识：文字标签，不依赖平台品牌色。 */
export function PlatformTag({ task, className }: { task: Pick<TaskSnapshot, "platformName">; className?: string }) {
  return (
    <span
      className={cn(
        "inline-flex h-4 shrink-0 items-center rounded-sm border px-1 text-(length:--text-small) leading-none text-muted-foreground",
        className,
      )}
    >
      {task.platformName}
    </span>
  );
}
