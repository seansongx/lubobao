// 页面间跳转的约定：App 持有当前页面，页面通过 onNavigate 请求切换。
// 页面保活后，目标页可能早已挂载、停在旧状态，所以跳转带的定位信息都包装成带 seq 的请求：
// seq 每次跳转递增，目标页在 seq 变化时应用（重复跳到同一处也能再次触发）。
// - 带 taskId 跳到直播间：选中该任务。
// - 带 activityTaskId 跳到动态：只看该任务的动态。
// - 带 settingsGroup 跳到设置：打开该分组。

import type { TaskId } from "@/lib/api";
import type { PageId } from "@/pages/pages";

export type SettingsGroupId = "record" | "check" | "network" | "accounts" | "notify" | "components" | "about";

export interface NavigateOptions {
  /** 跳到直播间并选中该任务。 */
  taskId?: TaskId;
  /** 跳到动态并按该任务筛选。 */
  activityTaskId?: TaskId;
  /** 跳到设置的某个分组。 */
  settingsGroup?: SettingsGroupId;
}

export type Navigate = (page: PageId, options?: NavigateOptions) => void;

/** App 交给目标页面的定位请求。 */
export interface FocusRequest<T> {
  value: T;
  seq: number;
}

/** 兼容直播间页原有的写法。 */
export interface TaskFocusRequest {
  taskId: TaskId;
  seq: number;
}
