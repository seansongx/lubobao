import { useState } from "react";

import { AppSidebar } from "@/components/app-sidebar";
import { StatusBar } from "@/components/status-bar";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PagePlaceholder } from "@/pages/page-placeholder";
import { useTaskList } from "@/lib/use-backend";
import { pages, type PageId } from "@/pages/pages";
import { RoomsPage } from "@/pages/rooms/rooms-page";
import { initialRoomsView, type RoomsView } from "@/pages/rooms/rooms-view";

export default function App() {
  const [current, setCurrent] = useState<PageId>("rooms");
  // 直播间页的筛选、排序和选中项放在这里，切换页面后保留。
  const [roomsView, setRoomsView] = useState<RoomsView>(initialRoomsView);
  // 任务订阅放在 App 层，回到直播间时不必重新加载，也不会闪出空状态。
  const taskList = useTaskList();
  const page = pages.find((item) => item.id === current) ?? pages[0];

  return (
    <TooltipProvider delayDuration={400}>
      <div className="grid h-screen grid-cols-[200px_minmax(0,1fr)] grid-rows-[minmax(0,1fr)_auto]">
        <AppSidebar current={current} onNavigate={setCurrent} />
        <main className="flex min-h-0 min-w-0">
          {page.id === "rooms" ? (
            <RoomsPage taskList={taskList} view={roomsView} onViewChange={setRoomsView} />
          ) : (
            <PagePlaceholder key={page.id} page={page} />
          )}
        </main>
        <StatusBar />
      </div>
      {/* 顶部居中，避免遮住详情面板底部的操作按钮和批量操作条。 */}
      <Toaster position="top-center" />
    </TooltipProvider>
  );
}
