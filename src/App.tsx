import { useCallback, useMemo, useState } from "react";

import { AppSidebar } from "@/components/app-sidebar";
import { useAppShortcuts } from "@/components/app-shortcuts";
import type { FocusRequest, Navigate, SettingsGroupId, TaskFocusRequest } from "@/components/navigation";
import { PageSlot } from "@/components/page-activity";
import { DesktopDialogs } from "@/components/desktop-dialogs";
import { SelectionMenu } from "@/components/selection-menu";
import { StatusBar } from "@/components/status-bar";
import { UpdateDialog } from "@/components/update-dialog";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useTaskList } from "@/lib/use-backend";
import { ActivityPage } from "@/pages/activity/activity-page";
import { LibraryPage } from "@/pages/library/library-page";
import { PagePlaceholder } from "@/pages/page-placeholder";
import { pages, type PageId } from "@/pages/pages";
import { isRecordingActive } from "@/pages/rooms/format";
import { RoomsPage } from "@/pages/rooms/rooms-page";
import { initialRoomsView, type RoomsView } from "@/pages/rooms/rooms-view";
import { SettingsPage } from "@/pages/settings/settings-page";

export default function App() {
  const [current, setCurrent] = useState<PageId>("rooms");
  // 直播间页的筛选、排序和选中项放在这里，切换页面后保留。
  const [roomsView, setRoomsView] = useState<RoomsView>(initialRoomsView);
  // 任务订阅放在 App 层，回到直播间时不必重新加载，也不会闪出空状态。
  const taskList = useTaskList();
  // 跳到直播间时要选中的任务；RoomsPage 在 seq 变化时选中 taskId。
  const [taskFocus, setTaskFocus] = useState<TaskFocusRequest | null>(null);
  // 跳到动态时的任务筛选、跳到设置时的分组；目标页保活，按 seq 应用。
  const [activityFocus, setActivityFocus] = useState<FocusRequest<number> | null>(null);
  const [settingsFocus, setSettingsFocus] = useState<FocusRequest<SettingsGroupId> | null>(null);
  // 打开过的页面一直挂载（页面保活），非当前页隐藏。
  const [visited, setVisited] = useState<ReadonlySet<PageId>>(() => new Set<PageId>(["rooms"]));

  const navigate = useCallback<Navigate>((target, options) => {
    if (options?.taskId !== undefined) {
      const taskId = options.taskId;
      setTaskFocus((previous) => ({ taskId, seq: (previous?.seq ?? 0) + 1 }));
    }
    if (options?.activityTaskId !== undefined) {
      const value = options.activityTaskId;
      setActivityFocus((previous) => ({ value, seq: (previous?.seq ?? 0) + 1 }));
    }
    if (options?.settingsGroup !== undefined) {
      const value = options.settingsGroup;
      setSettingsFocus((previous) => ({ value, seq: (previous?.seq ?? 0) + 1 }));
    }
    setVisited((previous) => (previous.has(target) ? previous : new Set(previous).add(target)));
    setCurrent(target);
  }, []);

  useAppShortcuts(navigate, current);

  const recordingCount = useMemo(
    () => (taskList.tasks ?? []).filter((task) => isRecordingActive(task.record)).length,
    [taskList.tasks],
  );

  const renderPage = (id: PageId) => {
    switch (id) {
      case "rooms":
        return (
          <RoomsPage
            taskList={taskList}
            view={roomsView}
            onViewChange={setRoomsView}
            focusTask={taskFocus}
            onNavigate={navigate}
          />
        );
      case "library":
        return <LibraryPage onNavigate={navigate} />;
      case "activity":
        return <ActivityPage onNavigate={navigate} focusTask={activityFocus} />;
      case "settings":
        return <SettingsPage focusGroup={settingsFocus} />;
      default: {
        const page = pages.find((item) => item.id === id) ?? pages[0];
        return <PagePlaceholder page={page} />;
      }
    }
  };

  return (
    <TooltipProvider delayDuration={400}>
      {/* 侧栏占满高度；右侧是内容层和状态栏。Windows 的内容层是 Mica 上的 Fluent Layer（左上圆角加描边）。 */}
      <div className="grid h-screen grid-cols-[var(--sidebar-width)_minmax(0,1fr)] bg-window">
        <AppSidebar current={current} onNavigate={navigate} recordingCount={recordingCount} />
        <div className="flex min-h-0 min-w-0 flex-col">
          <main className="relative flex min-h-0 min-w-0 flex-1 bg-content win:rounded-tl-xl win:border-t win:border-l win:border-content-border">
            {pages
              .filter((page) => visited.has(page.id))
              .map((page) => (
                <PageSlot key={page.id} active={page.id === current}>
                  {renderPage(page.id)}
                </PageSlot>
              ))}
          </main>
          <StatusBar />
        </div>
      </div>
      <SelectionMenu />
      <DesktopDialogs />
      <UpdateDialog />
      {/* 顶部居中，避免遮住详情面板底部的操作按钮和批量操作条。 */}
      <Toaster position="top-center" />
    </TooltipProvider>
  );
}
