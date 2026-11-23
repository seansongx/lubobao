// 直播间页的命令调用：成功和失败都用提示反馈，失败时显示后端返回的原因。

import { openUrl, revealItemInDir } from "@tauri-apps/plugin-opener";
import { toast } from "sonner";

import { api, isMockBackend, type TaskId, type TaskSnapshot } from "@/lib/api";
import { describeError } from "@/pages/rooms/format";

function reportError(action: string, error: unknown) {
  const view = describeError(error);
  toast.error(`${action}失败：${view.title}`, { description: view.message || view.advice });
}

export async function setMonitoring(ids: TaskId[], enabled: boolean): Promise<boolean> {
  const action = enabled ? "开始监控" : "暂停监控";
  try {
    const result = await api.setMonitoring(ids, enabled);
    toast.success(`已${action} ${result.length} 个任务`);
    return true;
  } catch (error) {
    reportError(action, error);
    return false;
  }
}

export async function stopRecording(task: TaskSnapshot): Promise<void> {
  try {
    await api.stopRecording([task.id]);
    toast.success(`已停止录制 ${task.anchorName}`, { description: "手动停止后保持停止，直到下次开播。" });
  } catch (error) {
    reportError("停止录制", error);
  }
}

export async function startRecording(task: TaskSnapshot): Promise<void> {
  try {
    const result = await api.startRecording([task.id]);
    if (result.length === 0) {
      toast.info(`${task.anchorName} 现在无法开始录制`, { description: "请先恢复监控，或等当前录制结束。" });
      return;
    }
    toast.success(`正在检测 ${task.anchorName}`, { description: "仍在直播就会立即开始录制。" });
  } catch (error) {
    reportError("开始录制", error);
  }
}

/** 录制文件所在位置；没有文件时为保存目录。 */
export async function folderFor(task: TaskSnapshot): Promise<{ path: string; isFile: boolean }> {
  const { record } = task;
  if (record.state === "recording" || record.state === "finishing") return { path: record.filePath, isFile: true };
  if (record.state === "failed" && record.filePath) return { path: record.filePath, isFile: true };
  const settings = await api.getSettings();
  return { path: settings.live_save_path, isFile: false };
}

export async function openFolder(task: TaskSnapshot): Promise<void> {
  let target: { path: string; isFile: boolean };
  try {
    target = await folderFor(task);
  } catch (error) {
    reportError("打开文件夹", error);
    return;
  }
  if (isMockBackend) {
    toast.info("浏览器预览无法打开文件夹", { description: target.path });
    return;
  }
  try {
    // opener:default 只允许在文件管理器中定位；目录会在其上级目录中选中。
    await revealItemInDir(target.path);
  } catch (error) {
    toast.error("打开文件夹失败", { description: `${target.path}：${String(error)}` });
  }
}

export async function openInBrowser(task: TaskSnapshot): Promise<void> {
  const raw = (task.liveUrl ?? task.url).trim();
  // 后端接受不带协议的输入（如 v.douyin.com/xxx），opener 只允许 http/https。
  const url = raw.includes("://") ? raw : `https://${raw}`;
  if (isMockBackend) {
    window.open(url, "_blank", "noopener");
    return;
  }
  try {
    await openUrl(url);
  } catch (error) {
    toast.error("无法打开链接", { description: String(error) });
  }
}
