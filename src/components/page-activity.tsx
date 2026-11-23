// 页面保活：App 把打开过的页面一直挂载，非当前页隐藏。页面里的组件用这里的 hook 判断自己是否在当前页，
// 隐藏时暂停周期性工作，回到当前页时刷新一次。

import { createContext, useContext, useEffect, useRef, type ReactNode } from "react";

import { cn } from "@/lib/utils";

const PageActiveContext = createContext(true);

/** 所在页面是否为当前页。页面以外（侧栏、状态栏、全局对话框）始终为 true。 */
export function usePageActive(): boolean {
  return useContext(PageActiveContext);
}

/** 页面从隐藏回到当前页时调用 callback（首次挂载不调用）。 */
export function useOnPageActivate(callback: () => void): void {
  const active = usePageActive();
  const ref = useRef(callback);
  const wasActive = useRef(active);
  useEffect(() => {
    ref.current = callback;
  });
  useEffect(() => {
    if (active && !wasActive.current) ref.current();
    wasActive.current = active;
  }, [active]);
}

/**
 * 一个页面的容器。当前页正常参与布局；非当前页叠在下面、不可见、不可交互（inert：不能获得焦点，
 * 读屏也读不到）。用 visibility 而不是 display:none，切回来时页面里各滚动区域的位置保持不变。
 */
export function PageSlot({ active, children }: { active: boolean; children: ReactNode }) {
  return (
    <PageActiveContext.Provider value={active}>
      <div
        data-page-active={active || undefined}
        data-page-hidden={!active || undefined}
        inert={!active}
        className={cn("flex min-h-0 min-w-0 flex-1", !active && "pointer-events-none invisible absolute inset-0")}
      >
        {children}
      </div>
    </PageActiveContext.Provider>
  );
}
