// 开发调试面板：切换平台、窗口材质、明暗、示例数据，触发少见状态。
// 只在开发构建里由 status-bar.tsx 动态加载（import.meta.env.DEV 分支），正式构建里没有这个模块。
// 入口在状态栏最右侧，快捷键 Ctrl+Shift+D（macOS 为 Cmd+Shift+D）。覆盖值存在网址参数里，见 overrides.ts。

import { useEffect, useState, type ReactNode } from "react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Bug, RotateCcw } from "lucide-react";
import { toast } from "sonner";

import { checkForUpdates } from "@/components/update-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { isMockBackend } from "@/lib/api";
import { hasModifier, isMac, platform as activePlatform, type Platform, type WindowMaterial } from "@/lib/platform";
import {
  materialNames,
  overrideLabels,
  platformNames,
  readOverrides,
  realShell,
  reloadWith,
  rememberTheme,
  resetAll,
  type Theme,
} from "@/dev/overrides";

const FOLLOW = "follow";
const desktop = isTauri();
const shortcut = isMac ? "⇧⌘D" : "Ctrl+Shift+D";

async function applyTheme(theme: Theme | null) {
  if (!desktop) return;
  try {
    // 需要 core:window:allow-set-theme，只在调试构建里由 lib.rs 放开。
    await getCurrentWindow().setTheme(theme);
  } catch (error) {
    toast.error(`切换明暗失败：${String(error)}`);
  }
}

/** 网址参数里记着强制的明暗：重新加载后再设一次（窗口主题本身在重新加载后也保留，这里只是保险）。 */
const initialTheme = readOverrides().theme;
if (initialTheme) void applyTheme(initialTheme);

function Section({ title, note, children }: { title: string; note?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label>{title}</Label>
      {children}
      {note && <p className="text-(length:--text-small) text-muted-foreground">{note}</p>}
    </div>
  );
}

function Choices<T extends string>({
  value,
  options,
  onChange,
  disabled,
  label,
}: {
  value: T | null;
  options: { value: T; label: string }[];
  onChange: (value: T | null) => void;
  disabled?: boolean;
  label: string;
}) {
  return (
    <ToggleGroup
      type="single"
      size="sm"
      spacing={0}
      variant="outline"
      value={value ?? FOLLOW}
      disabled={disabled}
      aria-label={label}
      onValueChange={(next) => {
        if (!next) return;
        onChange(next === FOLLOW ? null : (next as T));
      }}
    >
      <ToggleGroupItem value={FOLLOW}>跟随</ToggleGroupItem>
      {options.map((option) => (
        <ToggleGroupItem key={option.value} value={option.value}>
          {option.label}
        </ToggleGroupItem>
      ))}
    </ToggleGroup>
  );
}

const platformOptions = (["windows", "macos", "linux"] as const).map((value) => ({
  value,
  label: platformNames[value],
}));
const materialOptions = (["mica", "vibrancy", "none"] as const).map((value) => ({
  value,
  label: materialNames[value],
}));
const themeOptions: { value: Theme; label: string }[] = [
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
];

function triggerUpdateDialog() {
  const mock = (window as { __lubobaoMock?: { setUpdate(mode: string): void } }).__lubobaoMock;
  mock?.setUpdate("available");
  void checkForUpdates({ manual: true });
}

export default function DebugPanel() {
  const [open, setOpen] = useState(false);
  const [overrides, setOverrides] = useState(readOverrides);
  const real = realShell();
  const labels = overrideLabels(overrides);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (hasModifier(event) && event.shiftKey && !event.altKey && event.key.toLowerCase() === "d") {
        event.preventDefault();
        setOpen((value) => !value);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const setTheme = (theme: Theme | null) => {
    void applyTheme(theme);
    rememberTheme(theme);
    setOverrides((current) => ({ ...current, theme }));
  };

  const reset = () => {
    // 先恢复窗口主题，再去掉参数重新加载。
    if (overrides.theme) void applyTheme(null).finally(resetAll);
    else resetAll();
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <Tooltip>
        <TooltipTrigger asChild>
          <PopoverTrigger asChild>
            <Button variant="ghost" size="xs" className="shrink-0">
              <Bug aria-hidden />
              调试
              {isMockBackend && desktop && <Badge>示例数据</Badge>}
              {labels.length > 0 && <Badge variant="secondary">已覆盖 {labels.length} 项</Badge>}
            </Button>
          </PopoverTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">
          调试面板（{shortcut}），仅开发环境
          {labels.length > 0 && `：${labels.join("、")}`}
        </TooltipContent>
      </Tooltip>
      <PopoverContent side="top" align="end" className="w-[22rem]">
        <div className="flex items-center justify-between gap-2">
          <span className="font-semibold">调试面板</span>
          <Badge variant="outline">仅开发环境</Badge>
        </div>

        <div className="flex items-center justify-between gap-2">
          <span className="flex items-center gap-2">
            当前数据
            {isMockBackend ? <Badge>示例数据</Badge> : <Badge variant="secondary">真实数据</Badge>}
          </span>
          {desktop && (
            <span className="flex items-center gap-2">
              <Label htmlFor="dev-mock">使用示例数据</Label>
              <Switch
                id="dev-mock"
                checked={overrides.mock}
                onCheckedChange={(checked) => reloadWith({ mock: checked, tasks: null })}
              />
            </span>
          )}
        </div>
        <p className="text-(length:--text-small) text-muted-foreground">
          {desktop
            ? isMockBackend
              ? "界面显示 mock-api.ts 的示例数据，操作不读写数据库、不联网。后台的真实直播间仍在照常检测和录制；退出、关窗和托盘提示仍走真实后端。"
              : "界面显示真实数据库里的直播间。切换后重新加载页面。"
            : "浏览器预览没有真实后端，始终是示例数据。"}
        </p>

        <Separator />

        <Section
          title="平台"
          note={`跟随：${platformNames[real.platform]}。当前 ${platformNames[activePlatform]}，切换后重新加载；影响字体族、macOS 侧栏顶部留白、滚动条和快捷键。桌面端的标题栏和窗口外壳不变。`}
        >
          <Choices
            label="平台"
            value={overrides.platform}
            options={platformOptions}
            onChange={(value: Platform | null) => reloadWith({ platform: value })}
          />
        </Section>

        <Section
          title="窗口材质"
          note={
            desktop
              ? `跟随：${materialNames[real.material]}。只改网页底色，窗口本身的材质不变：窗口没有材质时强制 Mica 或 Vibrancy 会透出窗口的默认底色，有材质时强制「不透明」会盖住材质。`
              : "浏览器里没有真实材质，Mica 和 Vibrancy 只是把窗口底、侧栏（Mica 另有状态栏）变透明，透出页面背后的白底或黑底。"
          }
        >
          <Choices
            label="窗口材质"
            value={overrides.material}
            options={materialOptions}
            onChange={(value: WindowMaterial | null) => reloadWith({ material: value })}
          />
        </Section>

        <Section
          title="明暗"
          note={
            desktop
              ? "设置窗口主题，prefers-color-scheme 随之变化，立即生效。"
              : "浏览器预览改不了 prefers-color-scheme，请改系统设置或用开发者工具的「渲染 → 模拟 CSS 媒体功能」。"
          }
        >
          <Choices label="明暗" value={overrides.theme} options={themeOptions} onChange={setTheme} disabled={!desktop} />
        </Section>

        <Separator />

        <Section
          title="少见状态"
          note={isMockBackend ? "空列表和加载失败会重新加载页面；加载失败后点「重试」恢复。" : "只在示例数据下可用。"}
        >
          <div className="flex flex-wrap gap-1.5">
            <Button variant="outline" size="sm"               disabled={!isMockBackend}
              onClick={() => {
                setOpen(false);
                triggerUpdateDialog();
              }}
            >
              发现新版本
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={!isMockBackend || overrides.tasks === "empty"}
              onClick={() => reloadWith({ tasks: "empty" })}
            >
              直播间空列表
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={!isMockBackend || overrides.tasks === "fail"}
              onClick={() => reloadWith({ tasks: "fail" })}
            >
              加载失败
            </Button>
            {overrides.tasks && (
              <Button variant="ghost" size="sm" onClick={() => reloadWith({ tasks: null })}>
                恢复列表
              </Button>
            )}
          </div>
        </Section>

        <Separator />

        <div className="flex items-center justify-between gap-2">
          <span className="min-w-0 truncate text-(length:--text-small) text-muted-foreground">
            {labels.length > 0 ? `已覆盖：${labels.join("、")}` : "没有覆盖"}
          </span>
          <Button variant="outline" size="sm" className="shrink-0" disabled={labels.length === 0} onClick={reset}>
            <RotateCcw aria-hidden />
            全部恢复默认
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}
