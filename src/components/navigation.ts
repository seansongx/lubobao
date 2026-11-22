// 页面间跳转的约定：App 持有当前页面，页面通过 onNavigate 请求切换。
// 带 taskId 跳到直播间时，App 把它作为 focusTaskId 交给直播间页面，由直播间页面选中该任务。

import type { TaskId } from "@/lib/api";
import type { PageId } from "@/pages/pages";

export interface NavigateOptions {
  /** 跳到直播间并选中该任务。 */
  taskId?: TaskId;
}

export type Navigate = (page: PageId, options?: NavigateOptions) => void;

/** App 交给直播间页面的选中请求。seq 每次跳转递增，重复点同一任务也能再次触发。 */
export interface TaskFocusRequest {
  taskId: TaskId;
  seq: number;
}
