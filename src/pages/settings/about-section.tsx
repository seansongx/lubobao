// 关于：应用名、版本号、检查更新、许可证、仓库链接和第三方声明。

import { useEffect, useState } from "react";
import { ExternalLink, Loader2Icon, RefreshCw } from "lucide-react";

import { checkForUpdates, updateBusy, useUpdateFlow } from "@/components/update-dialog";
import { Button } from "@/components/ui/button";
import { api, WEBSITE_URL } from "@/lib/api";
import { FieldRow, SettingsCard, SwitchField } from "@/pages/settings/fields";
import { Markdown } from "@/pages/settings/markdown";
import type { SettingsController } from "@/pages/settings/use-settings";

import notices from "../../../THIRD_PARTY_NOTICES.md?raw";

const REPO_URL = "https://github.com/seansongx/lubobao";
const LICENSE_URL = `${REPO_URL}/blob/main/LICENSE`;

/** ctx 为 null 时设置还没读到，「自动检查更新」开关暂不可用。 */
export function AboutSection({ ctx }: { ctx: SettingsController | null }) {
  const flow = useUpdateFlow();
  const busy = updateBusy(flow);
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    api.getAppVersion().then(
      (value) => active && setVersion(value),
      () => active && setVersion("未知"),
    );
    return () => {
      active = false;
    };
  }, []);

  return (
    <>
      <SettingsCard title="关于录播宝">
        <FieldRow label="应用">
          <span>录播宝 LuBoBao</span>
        </FieldRow>
        <FieldRow label="版本" description="新版本从 xiaofany.com 下载，安装前校验签名">
          <span className="tabular-nums">{version ?? "读取中…"}</span>
          <Button type="button" variant="outline" size="sm" disabled={busy} onClick={() => void checkForUpdates({ manual: true })}>
            {flow.step === "checking" ? <Loader2Icon className="animate-spin" aria-hidden /> : <RefreshCw aria-hidden />}
            {flow.step === "checking" ? "正在检查…" : flow.step === "downloading" ? "正在下载…" : "检查更新"}
          </Button>
        </FieldRow>
        <SwitchField
          label="自动检查更新"
          description="启动后和之后每天检查一次，有新版本时提示，不会自动安装。检查更新时会发送一个随机设备标识，用于分批推送"
          checked={ctx?.settings.auto_check_update ?? true}
          disabled={!ctx}
          error={ctx?.errors.auto_check_update}
          onChange={(value) => void ctx?.save({ auto_check_update: value })}
        />
        <FieldRow label="官网">
          <Button type="button" variant="link" onClick={() => void api.openUrl(WEBSITE_URL)}>
            lubobao.xiaofany.com
            <ExternalLink aria-hidden />
          </Button>
        </FieldRow>
        <FieldRow label="许可证" description="可自由使用、修改和分发，需保留版权声明">
          <Button type="button" variant="link" onClick={() => void api.openUrl(LICENSE_URL)}>
            MIT License
            <ExternalLink aria-hidden />
          </Button>
        </FieldRow>
        <FieldRow label="源代码">
          <Button type="button" variant="link" onClick={() => void api.openUrl(REPO_URL)}>
            github.com/seansongx/lubobao
            <ExternalLink aria-hidden />
          </Button>
        </FieldRow>
      </SettingsCard>
      <SettingsCard title="第三方声明">
        <div className="px-4 py-3">
          <Markdown source={notices} />
        </div>
      </SettingsCard>
    </>
  );
}
