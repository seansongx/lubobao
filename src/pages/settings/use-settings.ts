// 设置页的数据：读取一次快照，修改后立即保存（自动保存），后端拒绝时把原因挂到对应字段。

import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";

import { api, isCommandError, settingDefaults, type Settings } from "@/lib/api";

export type SettingKey = keyof Settings;
export type FieldErrors = Partial<Record<SettingKey, string>>;

export interface SettingsController {
  settings: Settings;
  errors: FieldErrors;
  /** 保存若干项。成功时返回 true 并给出轻量提示；失败时把原因挂到字段上。 */
  save(values: Partial<Settings>, notice?: string): Promise<boolean>;
  /** 前端校验的错误，传 null 清除。 */
  setError(key: SettingKey, message: string | null): void;
  /** 把若干项恢复为后端默认值。 */
  reset(keys: readonly SettingKey[]): Promise<boolean>;
  /** 每次恢复默认值后加一，用作分组的 key，丢弃输入框里未保存的草稿。 */
  revision: number;
}

const SAVED_TOAST = "settings-saved";

/** 后端错误信息里的键名换成界面上的名称。 */
const fieldLabels: Partial<Record<SettingKey, string>> = {
  live_save_path: "保存目录",
  record_quality: "默认画质",
  video_format: "录制格式",
  segmented_recording_enabled: "分段录制",
  video_segment_time: "分段时长",
  recording_space_threshold: "磁盘空间阈值",
  convert_to_mp4: "录制完成后转为 MP4",
  delete_original: "转换后删除原文件",
  folder_name_platform: "按平台建子目录",
  folder_name_author: "按主播建子目录",
  loop_time_seconds: "检测间隔",
  platform_max_concurrent_requests: "每平台并发数",
  default_live_source: "默认直播源",
  force_https_recording: "强制 HTTPS",
  enable_proxy: "启用代理",
  proxy_address: "代理地址",
  default_platform_with_proxy: "走代理的平台",
  platform_cookies: "平台 Cookie",
  system_notification_enabled: "系统通知",
  stream_start_notification_enabled: "开播通知",
  stream_end_notification_enabled: "下播通知",
};

/** 后端校验信息形如「设置项 loop_time_seconds 的值无效：0」，从中找出字段名。 */
function fieldOf(message: string, keys: SettingKey[]): SettingKey | null {
  return keys.find((key) => new RegExp(`(^|[^a-z_])${key}([^a-z_]|$)`).test(message)) ?? null;
}

export function useSettings() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [errors, setErrors] = useState<FieldErrors>({});
  const [revision, setRevision] = useState(0);
  // 保存按顺序进行，后发的保存不会被先发的结果覆盖。
  const queue = useRef<Promise<unknown>>(Promise.resolve());

  const load = useCallback(() => {
    setLoadError(null);
    api.getSettings().then(setSettings, (error: unknown) => {
      setLoadError(isCommandError(error) ? error.message : String(error));
    });
  }, []);

  useEffect(load, [load]);

  const setError = useCallback((key: SettingKey, message: string | null) => {
    setErrors((current) => {
      if ((current[key] ?? null) === message) return current;
      const next = { ...current };
      if (message === null) delete next[key];
      else next[key] = message;
      return next;
    });
  }, []);

  const save = useCallback((values: Partial<Settings>, notice = "已保存") => {
    const keys = Object.keys(values) as SettingKey[];
    const run = async () => {
      try {
        const next = await api.updateSettings(values);
        setSettings(next);
        setErrors((current) => {
          const rest = { ...current };
          for (const key of keys) delete rest[key];
          return rest;
        });
        toast.success(notice, { id: SAVED_TOAST, duration: 1600 });
        return true;
      } catch (error) {
        const message = isCommandError(error) ? error.message : String(error);
        const validation = isCommandError(error) && (error.kind === "config" || error.kind === "invalid_input");
        const key = validation ? (fieldOf(message, keys) ?? keys[0]) : null;
        if (key) {
          const label = fieldLabels[key];
          const shown = label ? message.replace(key, `「${label}」`) : message;
          setErrors((current) => ({ ...current, [key]: shown }));
        } else {
          toast.error(`保存失败：${message}`, { id: SAVED_TOAST });
        }
        return false;
      }
    };
    const result = queue.current.then(run, run);
    queue.current = result;
    return result;
  }, []);

  const reset = useCallback(
    async (keys: readonly SettingKey[]) => {
      const values: Partial<Settings> = {};
      for (const key of keys) {
        if (key === "live_save_path") values.live_save_path = await api.defaultSaveDir();
        else Object.assign(values, { [key]: settingDefaults[key] });
      }
      // 恢复的键先清掉前端校验错误，成功后重新挂载分组，输入框回到保存的值。
      for (const key of keys) setError(key, null);
      const saved = await save(values, "已恢复默认值");
      if (saved) setRevision((current) => current + 1);
      return saved;
    },
    [save, setError],
  );

  const controller: SettingsController | null = settings ? { settings, errors, save, setError, reset, revision } : null;
  return { controller, loadError, reload: load };
}
