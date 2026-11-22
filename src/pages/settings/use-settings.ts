// 设置页的数据：读取一次快照，修改后立即保存（自动保存），后端拒绝时把原因挂到对应字段。
//
// 保存按顺序排队执行。要保存的值可以是一个更新函数，轮到它执行时才用“最新已保存的设置”
// 计算，这样连续的修改（例如先改平台列表再点切换、两个平台的 Cookie 先后提交）不会用渲染
// 当时的旧快照互相覆盖。

import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";

import { api, isCommandError, type Settings } from "@/lib/api";

export type SettingKey = keyof Settings;
export type FieldErrors = Partial<Record<SettingKey, string>>;

/** 要保存的部分设置，或根据最新设置计算它的函数。返回 null 表示不用保存。 */
export type SettingsUpdate = Partial<Settings> | ((latest: Settings) => Partial<Settings> | null);

export interface SaveOptions {
  /** 成功后的提示，默认“已保存”。 */
  notice?: string;
  /** 为 false 时校验错误不挂到字段上，只通过返回值交给调用方（例如按平台显示的 Cookie）。 */
  attachErrors?: boolean;
}

export type SaveResult = { ok: true } | { ok: false; message: string };

export interface SettingsController {
  settings: Settings;
  errors: FieldErrors;
  save(update: SettingsUpdate, options?: SaveOptions): Promise<SaveResult>;
  /** 前端校验的错误，传 null 清除。 */
  setError(key: SettingKey, message: string | null): void;
  /** 把若干项恢复为后端默认值。 */
  reset(keys: readonly SettingKey[]): Promise<SaveResult>;
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
  close_window_action: "关闭窗口时",
};

/** 后端校验信息形如「设置项 loop_time_seconds 的值无效：0」，从中找出字段名。 */
function fieldOf(message: string, keys: SettingKey[]): SettingKey | null {
  return keys.find((key) => new RegExp(`(^|[^a-z_])${key}([^a-z_]|$)`).test(message)) ?? null;
}

function errorMessage(error: unknown) {
  return isCommandError(error) ? error.message : String(error);
}

/**
 * 后端少了某个键（例如旧版本数据、或后端还没加上的新键）时用默认值补上，
 * 页面读取任何键都不会拿到 undefined。
 */
function withDefaults(loaded: Settings, defaults: Settings | null): Settings {
  return defaults ? { ...defaults, ...loaded } : loaded;
}

export function useSettings() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [errors, setErrors] = useState<FieldErrors>({});
  const [revision, setRevision] = useState(0);
  // 每次从后端重新读取成功后加一。设置页的错误边界以它为 key，重新读取后才重置。
  const [loadId, setLoadId] = useState(0);
  // 排队中的保存读取这里的最新值，而不是渲染时的快照。
  const latest = useRef<Settings | null>(null);
  const defaults = useRef<Settings | null>(null);
  const queue = useRef<Promise<unknown>>(Promise.resolve());

  const apply = useCallback((next: Settings) => {
    const merged = withDefaults(next, defaults.current);
    latest.current = merged;
    setSettings(merged);
  }, []);

  const load = useCallback(() => {
    setLoadError(null);
    // 默认值读不到不影响显示，只是“恢复默认值”时再试一次。
    const defaultsLoaded = api.getSettingDefaults().then(
      (value) => {
        defaults.current = value;
      },
      () => undefined,
    );
    Promise.all([api.getSettings(), defaultsLoaded]).then(
      ([loaded]) => {
        apply(loaded);
        setLoadId((current) => current + 1);
      },
      (error: unknown) => setLoadError(errorMessage(error)),
    );
  }, [apply]);

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

  const save = useCallback(
    (update: SettingsUpdate, options: SaveOptions = {}): Promise<SaveResult> => {
      const { notice = "已保存", attachErrors = true } = options;
      const run = async (): Promise<SaveResult> => {
        const base = latest.current;
        if (!base) return { ok: false, message: "设置尚未读取" };
        const values = typeof update === "function" ? update(base) : update;
        if (!values || Object.keys(values).length === 0) return { ok: true };
        const keys = Object.keys(values) as SettingKey[];
        try {
          apply(await api.updateSettings(values));
          if (attachErrors) {
            setErrors((current) => {
              const rest = { ...current };
              for (const key of keys) delete rest[key];
              return rest;
            });
          }
          toast.success(notice, { id: SAVED_TOAST, duration: 1600 });
          return { ok: true };
        } catch (error) {
          const message = errorMessage(error);
          const validation = isCommandError(error) && (error.kind === "config" || error.kind === "invalid_input");
          const key = validation ? (fieldOf(message, keys) ?? keys[0]) : null;
          const label = key ? fieldLabels[key] : undefined;
          const shown = key && label ? message.replace(key, `「${label}」`) : message;
          if (key && attachErrors) {
            setErrors((current) => ({ ...current, [key]: shown }));
          } else if (!key) {
            toast.error(`保存失败：${message}`, { id: SAVED_TOAST });
          }
          return { ok: false, message: shown };
        }
      };
      const result = queue.current.then(run, run);
      queue.current = result;
      return result;
    },
    [apply],
  );

  const reset = useCallback(
    async (keys: readonly SettingKey[]): Promise<SaveResult> => {
      if (!defaults.current) {
        try {
          defaults.current = await api.getSettingDefaults();
        } catch (error) {
          const message = `读取默认值失败：${errorMessage(error)}`;
          toast.error(message, { id: SAVED_TOAST });
          return { ok: false, message };
        }
      }
      const source = defaults.current;
      const values: Partial<Settings> = {};
      for (const key of keys) Object.assign(values, { [key]: source[key] });
      // 恢复的键先清掉前端校验错误，成功后重新挂载分组，输入框回到保存的值。
      for (const key of keys) setError(key, null);
      const result = await save(values, { notice: "已恢复默认值" });
      if (result.ok) setRevision((current) => current + 1);
      return result;
    },
    [save, setError],
  );

  const controller: SettingsController | null = settings ? { settings, errors, save, setError, reset, revision } : null;
  return { controller, loadError, reload: load, loadId };
}
