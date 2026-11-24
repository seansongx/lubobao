// 直播间页的文字与数值格式化。只读快照，不推测快照里没有的数据。

import {
  isCommandError,
  type CommandErrorKind,
  type DisplayStatus,
  type LiveState,
  type Quality,
  type RecordState,
  type TaskSnapshot,
} from "@/lib/api";

export const qualityLabels: Record<Quality, string> = {
  OD: "原画",
  UHD: "超清",
  HD: "高清",
  SD: "标清",
  LD: "流畅",
};

export const qualityOrder: Quality[] = ["OD", "UHD", "HD", "SD", "LD"];

export const statusLabels: Record<DisplayStatus, string> = {
  recording: "录制中",
  checking: "检测中",
  waiting: "等待开播",
  retrying: "重试中",
  notify: "已开播 · 仅通知",
  needsAction: "需处理",
  paused: "已暂停",
};

/** 状态列排序：越需要关注越靠前。 */
export const statusRank: Record<DisplayStatus, number> = {
  needsAction: 0,
  recording: 1,
  retrying: 2,
  notify: 3,
  checking: 4,
  waiting: 5,
  paused: 6,
};

export function isRecordingActive(record: RecordState): boolean {
  return record.state === "recording" || record.state === "finishing";
}

export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return [h, m, s].map((value) => String(value).padStart(2, "0")).join(":");
}

export function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${bytes} B`;
}

export function formatBitrate(kbps: number): string {
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${Math.round(kbps)} kbps`;
}

/** 相对时间：刚刚、12 秒前、3 分钟前…… */
export function formatAgo(at: number, now: number): string {
  const seconds = Math.round((now - at) / 1000);
  if (seconds < 5) return "刚刚";
  if (seconds < 60) return `${seconds} 秒前`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}

export function formatIn(at: number, now: number): string {
  const seconds = Math.max(0, Math.round((at - now) / 1000));
  if (seconds < 60) return `${seconds} 秒后`;
  return `${Math.ceil(seconds / 60)} 分钟后`;
}

/** 今天的显示时分，更早的显示月日。 */
export function formatClock(at: number): string {
  const date = new Date(at);
  if (date.toDateString() !== new Date().toDateString()) {
    return `${date.getMonth() + 1}/${date.getDate()}`;
  }
  return date.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
}

export function formatDateTime(at: number): string {
  return new Date(at).toLocaleString("zh-CN", { hour12: false });
}

/** 时长列：录制中显示已录时长，否则显示检测相关的时间。 */
export function describeTiming(task: TaskSnapshot, now: number): { text: string; hint: string } {
  const { record, live } = task;
  if (record.state === "recording" || record.state === "finishing") {
    return { text: formatDuration(now - record.startedAt), hint: `开始于 ${formatDateTime(record.startedAt)}` };
  }
  if (task.monitoring === "paused") return { text: "—", hint: "监控已暂停" };
  switch (live.state) {
    case "checking":
      return { text: "检测中…", hint: "正在检测是否开播" };
    case "checkFailed":
      if (live.nextRetryAt !== null) {
        return { text: `${formatIn(live.nextRetryAt, now)}重试`, hint: `第 ${live.attempt} 次失败于 ${formatDateTime(live.failedAt)}` };
      }
      return { text: `${formatAgo(live.failedAt, now)}失败`, hint: formatDateTime(live.failedAt) };
    case "offline":
    case "live":
      return { text: `${formatAgo(live.checkedAt, now)}检测`, hint: `上次检测 ${formatDateTime(live.checkedAt)}` };
    default:
      return { text: "—", hint: "尚未检测" };
  }
}

export function describeLive(live: LiveState): { value: string; reason: string | null } {
  switch (live.state) {
    case "unknown":
      return { value: "未知", reason: "尚未检测" };
    case "checking":
      return { value: "检测中", reason: null };
    case "offline":
      return { value: "未开播", reason: null };
    case "live":
      return { value: "已开播", reason: live.title };
    case "checkFailed":
      return {
        value: "检测失败",
        reason: `${live.message}（第 ${live.attempt} 次${live.nextRetryAt === null ? "，不再自动重试" : ""}）`,
      };
  }
}

export function describeRecord(task: TaskSnapshot): { value: string; reason: string | null } {
  const { record } = task;
  switch (record.state) {
    case "idle":
      if (task.recordHeld) return { value: "已停止", reason: "手动停止，本场直播不再录制" };
      return { value: task.notifyOnly ? "仅通知" : "未录制", reason: task.notifyOnly ? "开播时只通知，不写文件" : null };
    case "recording":
      return { value: "写入中", reason: null };
    case "finishing":
      return { value: "收尾中", reason: "正在关闭文件" };
    case "failed":
      return { value: "已中断", reason: record.reason };
  }
}

export interface ErrorView {
  title: string;
  advice: string;
  message: string;
}

const errorAdvice: Record<CommandErrorKind, { title: string; advice: string }> = {
  invalid_url: { title: "链接无效", advice: "检查链接是否完整，或在直播间页面重新复制分享链接。" },
  invalid_input: { title: "输入有误", advice: "检查填写的内容后重试。" },
  unsupported_platform: { title: "暂不支持这个平台", advice: "目前支持抖音、快手、B站、虎牙、斗鱼、小红书、YY、网易CC、微博、AcFun、花椒、映客、酷狗、六间房、百度、知乎、猫耳FM的直播间链接，以及抖音的主页和分享短链、小红书的直播分享链接，更多平台陆续加入。" },
  auth_required: { title: "需要登录", advice: "该直播间需要登录才能观看，在设置的平台账号中填写 Cookie 后重试。" },
  network: { title: "网络连接失败", advice: "检查网络或代理设置后重试。" },
  rate_limited: { title: "请求过于频繁", advice: "平台暂时限制了访问，过几分钟再试。" },
  parse: { title: "无法识别直播间信息", advice: "平台返回的内容无法解析，稍后重试；持续出现请反馈。" },
  config: { title: "设置有误", advice: "检查代理、Cookie 等设置后重试。" },
  not_found: { title: "任务不存在", advice: "任务可能已被删除，刷新后重试。" },
  recording_active: { title: "正在录制", advice: "先停止录制，再进行这个操作。" },
  not_implemented: { title: "功能尚未接入", advice: "这个操作将在录制引擎接入后可用。" },
  storage: { title: "保存失败", advice: "无法写入本地数据库，检查磁盘空间后重试。" },
};

export function describeError(error: unknown): ErrorView {
  if (isCommandError(error)) {
    const view = errorAdvice[error.kind] ?? { title: "操作失败", advice: "" };
    return { ...view, message: error.message };
  }
  return { title: "操作失败", advice: "", message: error instanceof Error ? error.message : String(error) };
}

/** 显示给用户的一行错误：后端原因优先。 */
export function errorMessage(error: unknown): string {
  const view = describeError(error);
  return view.message || view.title;
}
