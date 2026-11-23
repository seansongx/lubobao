// 设置页：左侧分组导航，右侧显示当前分组。修改后自动保存。

import { Component, useState, type ErrorInfo, type ReactNode } from "react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { AboutSection } from "@/pages/settings/about-section";
import { ComponentsSection } from "@/pages/settings/components-section";
import {
  AccountsSection,
  CheckSection,
  NetworkSection,
  NotifySection,
  RecordSection,
} from "@/pages/settings/setting-sections";
import { useSettings, type SettingsController } from "@/pages/settings/use-settings";

type GroupId = "record" | "check" | "network" | "accounts" | "notify" | "components" | "about";

const groups: { id: GroupId; label: string }[] = [
  { id: "record", label: "录制" },
  { id: "check", label: "检测" },
  { id: "network", label: "网络" },
  { id: "accounts", label: "平台账号" },
  { id: "notify", label: "托盘与通知" },
  { id: "components", label: "组件" },
  { id: "about", label: "关于" },
];

interface BoundaryProps {
  children: ReactNode;
  onRetry: () => void;
}

/** 分组渲染出错时只影响设置页的内容区，不让整个应用白屏。 */
class SettingsErrorBoundary extends Component<BoundaryProps, { error: Error | null }> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("设置页渲染失败", error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <div role="alert" className="flex max-w-[760px] items-center gap-3 rounded-lg border bg-card px-4 py-3">
        <span className="flex-1 text-destructive">这一组设置显示失败：{this.state.error.message}</span>
        {/* 重新读取成功后父组件换 key，错误边界随之重置。 */}
        <Button type="button" variant="outline" onClick={this.props.onRetry}>
          重新读取
        </Button>
      </div>
    );
  }
}

type SettingsGroupId = Exclude<GroupId, "components" | "about">;

function SettingsGroup({ group, ctx }: { group: SettingsGroupId; ctx: SettingsController }) {
  switch (group) {
    case "record":
      return <RecordSection ctx={ctx} />;
    case "check":
      return <CheckSection ctx={ctx} />;
    case "network":
      return <NetworkSection ctx={ctx} />;
    case "accounts":
      return <AccountsSection ctx={ctx} />;
    case "notify":
      return <NotifySection ctx={ctx} />;
  }
}

export function SettingsPage() {
  const [group, setGroup] = useState<GroupId>("record");
  const { controller, loadError, reload, loadId } = useSettings();

  let body;
  if (group === "components") {
    body = <ComponentsSection />;
  } else if (group === "about") {
    body = <AboutSection ctx={controller} />;
  } else if (controller) {
    body = <SettingsGroup key={controller.revision} group={group} ctx={controller} />;
  } else if (loadError) {
    body = (
      <div role="alert" className="flex max-w-[760px] items-center gap-3 rounded-lg border bg-card px-4 py-3">
        <span className="flex-1 text-destructive">读取设置失败：{loadError}</span>
        <Button type="button" variant="outline" onClick={reload}>
          重试
        </Button>
      </div>
    );
  } else {
    body = <p className="m-0 px-1 text-muted-foreground">正在读取设置…</p>;
  }

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col">
      <header className="flex items-center gap-2.5 px-5 pt-3.5">
        <h1 className="m-0 text-[17px] font-semibold">设置</h1>
        <span className="text-xs text-muted-foreground">修改后自动保存</span>
      </header>
      <div className="flex min-h-0 flex-1">
        <nav aria-label="设置分组" className="flex w-[160px] shrink-0 flex-col gap-0.5 py-3.5 pr-2.5 pl-5">
          {groups.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-current={group === item.id ? "page" : undefined}
              onClick={() => setGroup(item.id)}
              className={cn(
                "h-8 rounded-lg px-2.5 text-left text-[13px] text-muted-foreground outline-none transition-colors hover:bg-muted hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50",
                group === item.id && "bg-card font-medium text-foreground shadow-[0_0_0_1px_var(--border)]",
              )}
            >
              {item.label}
            </button>
          ))}
        </nav>
        <div className="min-w-0 flex-1 overflow-y-auto py-3.5 pr-5 pl-2.5">
          {/* 切换分组或重新读取设置后重置错误状态。 */}
          <SettingsErrorBoundary key={`${group}:${loadId}`} onRetry={reload}>
            {body}
          </SettingsErrorBoundary>
        </div>
      </div>
    </section>
  );
}
