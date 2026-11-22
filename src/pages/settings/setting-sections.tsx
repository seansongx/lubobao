// 有设置项的分组：录制、检测、网络、平台账号、通知。键名与默认值见 src-tauri/src/tasks/settings.rs。

import { useState } from "react";
import { Eye, EyeOff, FolderOpen, FolderSearch } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { api, type Platform, type Settings } from "@/lib/api";
import { cn } from "@/lib/utils";
import { FieldRow, NumberField, SelectField, SettingsCard, SwitchField, TextField, type SelectOption } from "@/pages/settings/fields";
import type { SettingKey, SettingsController } from "@/pages/settings/use-settings";

interface SectionProps {
  ctx: SettingsController;
}

/** 每个分组“恢复默认值”时恢复的键。 */
export const groupKeys = {
  record: [
    "live_save_path",
    "record_quality",
    "video_format",
    "segmented_recording_enabled",
    "video_segment_time",
    "recording_space_threshold",
    "convert_to_mp4",
    "delete_original",
    "folder_name_platform",
    "folder_name_author",
  ],
  check: ["loop_time_seconds", "platform_max_concurrent_requests", "default_live_source", "force_https_recording"],
  network: ["enable_proxy", "proxy_address", "default_platform_with_proxy"],
  accounts: ["platform_cookies"],
  notify: ["system_notification_enabled", "stream_start_notification_enabled", "stream_end_notification_enabled"],
} as const satisfies Record<string, readonly SettingKey[]>;

/** 常用的保存与校验接线。 */
function bind<K extends SettingKey>(ctx: SettingsController, key: K) {
  return {
    error: ctx.errors[key],
    onError: (message: string | null) => ctx.setError(key, message),
    save: (value: Settings[K]) => void ctx.save({ [key]: value } as Partial<Settings>),
  };
}

const qualityOptions: SelectOption[] = [
  { value: "OD", label: "原画" },
  { value: "UHD", label: "超清" },
  { value: "HD", label: "高清" },
  { value: "SD", label: "标清" },
  { value: "LD", label: "流畅" },
];

/** 录制引擎目前只输出 TS，其余格式列出但不可选。 */
const formatOptions = [
  {
    group: "视频",
    options: [
      { value: "TS", label: "TS（推荐，断流不损坏）" },
      ...["MP4", "FLV", "MKV", "MOV", "NUT"].map((value) => ({ value, label: `${value}（即将支持）`, disabled: true })),
    ],
  },
  {
    group: "仅音频",
    options: ["M4A", "AAC", "MP3", "WAV", "WMA"].map((value) => ({ value, label: `${value}（即将支持）`, disabled: true })),
  },
];

function formatDuration(seconds: number) {
  if (seconds % 3600 === 0) return `${seconds / 3600} 小时`;
  if (seconds % 60 === 0) return `${seconds / 60} 分钟`;
  return `${seconds} 秒`;
}

export function RecordSection({ ctx }: SectionProps) {
  const { settings } = ctx;
  const path = bind(ctx, "live_save_path");
  const segment = bind(ctx, "video_segment_time");
  const threshold = bind(ctx, "recording_space_threshold");
  const [pickError, setPickError] = useState<string | null>(null);

  const pick = async () => {
    setPickError(null);
    try {
      const picked = await api.pickDirectory({ title: "选择保存目录", defaultPath: settings.live_save_path });
      if (picked && picked !== settings.live_save_path) path.save(picked);
    } catch (error) {
      setPickError(`无法打开目录选择框：${String(error)}`);
    }
  };
  const open = async () => {
    setPickError(null);
    try {
      await api.openPath(settings.live_save_path);
    } catch (error) {
      setPickError(`无法打开目录，可能还没有创建：${String(error)}`);
    }
  };

  // 选择不在列表中的旧格式时也能显示当前值。
  const formatValue = settings.video_format;

  return (
    <SettingsCard title="录制" onReset={() => void ctx.reset(groupKeys.record)}>
      <FieldRow label="保存目录" description="录像文件保存的位置" error={path.error ?? pickError ?? undefined} stacked>
        <Input
          readOnly
          value={settings.live_save_path}
          aria-label="当前保存目录"
          title={settings.live_save_path}
          className="min-w-0 flex-1 font-mono text-xs"
        />
        <Button type="button" variant="outline" onClick={() => void pick()}>
          <FolderSearch aria-hidden />
          选择文件夹
        </Button>
        <Button type="button" variant="outline" onClick={() => void open()}>
          <FolderOpen aria-hidden />
          打开
        </Button>
      </FieldRow>
      <SelectField
        label="默认画质"
        description="新添加直播间的默认画质，直播间没有该画质时自动降一级"
        value={settings.record_quality}
        options={qualityOptions}
        error={ctx.errors.record_quality}
        onChange={(value) => bind(ctx, "record_quality").save(value as Settings["record_quality"])}
      />
      <SelectField
        label="录制格式"
        description="目前只支持 TS，其他格式即将支持"
        value={formatValue}
        options={formatOptions}
        error={ctx.errors.video_format}
        onChange={(value) => bind(ctx, "video_format").save(value as Settings["video_format"])}
      />
      <SwitchField
        label="分段录制"
        description="按时长切分文件，单个文件损坏时影响更小"
        checked={settings.segmented_recording_enabled}
        error={ctx.errors.segmented_recording_enabled}
        onChange={(value) => bind(ctx, "segmented_recording_enabled").save(value)}
      />
      <NumberField
        label="分段时长"
        description={`每段 ${formatDuration(settings.video_segment_time)}`}
        value={settings.video_segment_time}
        integer
        suffix="秒"
        disabled={!settings.segmented_recording_enabled}
        error={segment.error}
        onError={segment.onError}
        onCommit={segment.save}
      />
      <NumberField
        label="磁盘空间阈值"
        description="保存目录所在磁盘的剩余空间低于该值时停止录制"
        value={settings.recording_space_threshold}
        suffix="GB"
        error={threshold.error}
        onError={threshold.onError}
        onCommit={threshold.save}
      />
      <SwitchField
        label="录制完成后转为 MP4"
        description="转换功能即将接入，目前只保存此选项"
        checked={settings.convert_to_mp4}
        error={ctx.errors.convert_to_mp4}
        onChange={(value) => bind(ctx, "convert_to_mp4").save(value)}
      />
      <SwitchField
        label="转换后删除原文件"
        description="只在转换成功后删除 TS 原文件"
        checked={settings.delete_original}
        disabled={!settings.convert_to_mp4}
        error={ctx.errors.delete_original}
        onChange={(value) => bind(ctx, "delete_original").save(value)}
      />
      <SwitchField
        label="按平台建子目录"
        description="例如 录播宝/抖音/…"
        checked={settings.folder_name_platform}
        error={ctx.errors.folder_name_platform}
        onChange={(value) => bind(ctx, "folder_name_platform").save(value)}
      />
      <SwitchField
        label="按主播建子目录"
        description="每个主播的录像放在单独的文件夹"
        checked={settings.folder_name_author}
        error={ctx.errors.folder_name_author}
        onChange={(value) => bind(ctx, "folder_name_author").save(value)}
      />
    </SettingsCard>
  );
}

export function CheckSection({ ctx }: SectionProps) {
  const { settings } = ctx;
  const loop = bind(ctx, "loop_time_seconds");
  const concurrency = bind(ctx, "platform_max_concurrent_requests");
  return (
    <SettingsCard title="检测" onReset={() => void ctx.reset(groupKeys.check)}>
      <NumberField
        label="检测间隔"
        description="每隔多久检查一次是否开播"
        value={settings.loop_time_seconds}
        integer
        min={1}
        suffix="秒"
        error={loop.error}
        onError={loop.onError}
        onCommit={loop.save}
      />
      <NumberField
        label="每平台并发数"
        description="同一平台同时检测的直播间数量，过高可能触发平台风控"
        value={settings.platform_max_concurrent_requests}
        integer
        min={1}
        error={concurrency.error}
        onError={concurrency.onError}
        onCommit={concurrency.save}
      />
      <SelectField
        label="默认直播源"
        description="平台同时提供两种流时优先使用"
        value={settings.default_live_source}
        options={[
          { value: "FLV", label: "FLV" },
          { value: "HLS", label: "HLS（m3u8）" },
        ]}
        error={ctx.errors.default_live_source}
        onChange={(value) => bind(ctx, "default_live_source").save(value as Settings["default_live_source"])}
      />
      <SwitchField
        label="强制 HTTPS"
        description="把直播流地址中的 http 改为 https"
        checked={settings.force_https_recording}
        error={ctx.errors.force_https_recording}
        onChange={(value) => bind(ctx, "force_https_recording").save(value)}
      />
    </SettingsCard>
  );
}

const PROXY_PATTERN = /^((https?|socks5h?|socks4a?):\/\/)?[^\s/:@]+(:[^\s/@]*@[^\s/:@]+)?(:\d{1,5})?\/?$/i;

function validateProxy(value: string) {
  if (value === "") return null;
  if (/\s/.test(value)) return "代理地址不能包含空格";
  if (!PROXY_PATTERN.test(value)) return "格式应为 http://127.0.0.1:7890 或 socks5://127.0.0.1:1080";
  return null;
}

function splitPlatforms(value: string) {
  return value
    .split(/[,，]/)
    .map((item) => item.trim())
    .filter(Boolean);
}

/** 去掉空项和重复项，统一为「, 」分隔。 */
function normalizePlatforms(value: string) {
  return [...new Set(splitPlatforms(value))].join(", ");
}

const supportedPlatforms: { key: Platform; name: string }[] = [
  { key: "douyin", name: "抖音" },
  { key: "kuaishou", name: "快手" },
];

export function NetworkSection({ ctx }: SectionProps) {
  const { settings } = ctx;
  const address = bind(ctx, "proxy_address");
  const platforms = bind(ctx, "default_platform_with_proxy");
  const listed = splitPlatforms(settings.default_platform_with_proxy);

  const toggle = (key: Platform) => {
    const next = listed.includes(key) ? listed.filter((item) => item !== key) : [...listed, key];
    platforms.save(next.join(", "));
  };

  return (
    <SettingsCard
      title="网络"
      description="录制只支持 HTTP 代理；SOCKS 代理只对检测生效"
      onReset={() => void ctx.reset(groupKeys.network)}
    >
      <SwitchField
        label="启用代理"
        description="只对下方列出的平台生效"
        checked={settings.enable_proxy}
        error={ctx.errors.enable_proxy}
        onChange={(value) => bind(ctx, "enable_proxy").save(value)}
      />
      <TextField
        label="代理地址"
        description={
          /^socks/i.test(settings.proxy_address)
            ? "当前是 SOCKS 代理：只用于检测开播，录制时不走代理"
            : "例如 http://127.0.0.1:7890"
        }
        placeholder="http://127.0.0.1:7890"
        value={settings.proxy_address}
        disabled={!settings.enable_proxy}
        error={address.error}
        validate={validateProxy}
        onError={address.onError}
        onCommit={address.save}
      />
      <TextField
        label="走代理的平台"
        description="用逗号分隔的平台标识。抖音、快手是国内平台，通常不需要代理。"
        value={settings.default_platform_with_proxy}
        disabled={!settings.enable_proxy}
        error={platforms.error}
        stacked
        multiline
        className="font-mono text-xs"
        normalize={normalizePlatforms}
        onError={platforms.onError}
        onCommit={platforms.save}
      >
        <div className="flex shrink-0 flex-col gap-1.5">
          {supportedPlatforms.map((platform) => {
            const on = listed.includes(platform.key);
            return (
              <Button
                key={platform.key}
                type="button"
                variant="outline"
                size="sm"
                aria-pressed={on}
                disabled={!settings.enable_proxy}
                className={cn(on && "border-primary text-primary")}
                onClick={() => toggle(platform.key)}
              >
                {platform.name}
                {on ? "：走代理" : "：直连"}
              </Button>
            );
          })}
        </div>
      </TextField>
    </SettingsCard>
  );
}

function CookieField({
  platform,
  value,
  error,
  onCommit,
}: {
  platform: { key: Platform; name: string };
  value: string;
  error?: string;
  onCommit: (value: string) => void;
}) {
  const [visible, setVisible] = useState(false);
  const [draft, setDraft] = useState(value);
  const [synced, setSynced] = useState(value);
  if (value !== synced) {
    setSynced(value);
    setDraft(value);
  }
  const id = `cookie-${platform.key}`;
  const commit = () => {
    const next = draft.trim();
    if (next !== value) onCommit(next);
  };
  return (
    <FieldRow
      label={`${platform.name} Cookie`}
      description={value ? "已填写" : "未填写，部分直播间可能需要登录后才能检测"}
      htmlFor={id}
      error={error}
      errorId={`${id}-error`}
      stacked
    >
      <Input
        id={id}
        type={visible ? "text" : "password"}
        value={draft}
        placeholder="从浏览器开发者工具复制整段 Cookie"
        className="min-w-0 flex-1 font-mono text-xs"
        spellCheck={false}
        autoComplete="off"
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? `${id}-error` : undefined}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            commit();
          } else if (event.key === "Escape") {
            setDraft(value);
          }
        }}
      />
      <Button
        type="button"
        variant="outline"
        size="icon"
        aria-label={visible ? `遮挡${platform.name} Cookie` : `显示${platform.name} Cookie`}
        aria-pressed={visible}
        onClick={() => setVisible((current) => !current)}
      >
        {visible ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
      </Button>
    </FieldRow>
  );
}

export function AccountsSection({ ctx }: SectionProps) {
  const cookies = ctx.settings.platform_cookies ?? {};
  const save = (key: Platform, value: string) => {
    const next = { ...cookies };
    if (value) next[key] = value;
    else delete next[key];
    void ctx.save({ platform_cookies: next });
  };
  return (
    <SettingsCard
      title="平台账号"
      description="Cookie 只保存在本机，只发送给对应平台的接口。"
      onReset={() => void ctx.reset(groupKeys.accounts)}
    >
      {supportedPlatforms.map((platform) => (
        <CookieField
          key={platform.key}
          platform={platform}
          value={cookies[platform.key] ?? ""}
          error={ctx.errors.platform_cookies}
          onCommit={(value) => save(platform.key, value)}
        />
      ))}
    </SettingsCard>
  );
}

export function NotifySection({ ctx }: SectionProps) {
  const { settings } = ctx;
  const off = !settings.system_notification_enabled;
  return (
    <SettingsCard
      title="通知"
      description="通知发送功能即将接入，目前只保存这些选项"
      onReset={() => void ctx.reset(groupKeys.notify)}
    >
      <SwitchField
        label="系统通知"
        description="关闭后不发送任何系统通知"
        checked={settings.system_notification_enabled}
        error={ctx.errors.system_notification_enabled}
        onChange={(value) => bind(ctx, "system_notification_enabled").save(value)}
      />
      <SwitchField
        label="开播通知"
        description="主播开播时通知"
        checked={settings.stream_start_notification_enabled}
        disabled={off}
        error={ctx.errors.stream_start_notification_enabled}
        onChange={(value) => bind(ctx, "stream_start_notification_enabled").save(value)}
      />
      <SwitchField
        label="下播通知"
        description="主播下播时通知"
        checked={settings.stream_end_notification_enabled}
        disabled={off}
        error={ctx.errors.stream_end_notification_enabled}
        onChange={(value) => bind(ctx, "stream_end_notification_enabled").save(value)}
      />
    </SettingsCard>
  );
}
