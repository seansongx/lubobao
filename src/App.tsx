import { useCallback, useState } from "react";

import { AppSidebar } from "@/components/app-sidebar";
import type { Navigate, TaskFocusRequest } from "@/components/navigation";
import { StatusBar } from "@/components/status-bar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ActivityPage } from "@/pages/activity/activity-page";
import { LibraryPage } from "@/pages/library/library-page";
import { PagePlaceholder } from "@/pages/page-placeholder";
import { pages, type PageId } from "@/pages/pages";

export default function App() {
  const [current, setCurrent] = useState<PageId>("rooms");
  // 跳到直播间时要选中的任务。直播间页面接入后以 focusTask 属性接收：
  //   <RoomsPage focusTask={taskFocus} />，在 taskFocus.seq 变化时选中 taskFocus.taskId。
  // 在此之前只写到根元素的 data-focus-task 上，便于调试跳转。
  const [taskFocus, setTaskFocus] = useState<TaskFocusRequest | null>(null);
  const page = pages.find((item) => item.id === current) ?? pages[0];

  const navigate = useCallback<Navigate>((target, options) => {
    if (options?.taskId !== undefined) {
      const taskId = options.taskId;
      setTaskFocus((previous) => ({ taskId, seq: (previous?.seq ?? 0) + 1 }));
    }
    setCurrent(target);
  }, []);

  const content = (() => {
    switch (page.id) {
      case "library":
        return <LibraryPage onNavigate={navigate} />;
      case "activity":
        return <ActivityPage onNavigate={navigate} />;
      default:
        return <PagePlaceholder key={page.id} page={page} />;
    }
  })();

  return (
    <TooltipProvider delayDuration={400}>
      <div
        className="grid h-screen grid-cols-[200px_minmax(0,1fr)] grid-rows-[minmax(0,1fr)_auto]"
        data-focus-task={taskFocus?.taskId}
      >
        <AppSidebar current={current} onNavigate={navigate} />
        <main className="flex min-h-0 min-w-0">{content}</main>
        <StatusBar />
      </div>
    </TooltipProvider>
  );
}
