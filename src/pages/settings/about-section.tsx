// 关于：应用名、版本号、许可证、仓库链接和第三方声明。

import { useEffect, useState } from "react";
import { ExternalLink } from "lucide-react";

import { Button } from "@/components/ui/button";
import { api } from "@/lib/api";
import { FieldRow, SettingsCard } from "@/pages/settings/fields";
import { Markdown } from "@/pages/settings/markdown";

import notices from "../../../THIRD_PARTY_NOTICES.md?raw";

const REPO_URL = "https://github.com/seansongx/lubobao";
const LICENSE_URL = `${REPO_URL}/blob/main/LICENSE`;

export function AboutSection() {
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
        <FieldRow label="版本">
          <span className="tabular-nums">{version ?? "读取中…"}</span>
        </FieldRow>
        <FieldRow label="许可证" description="可自由使用、修改和分发，需保留版权声明">
          <Button type="button" variant="link" className="h-auto px-0" onClick={() => void api.openUrl(LICENSE_URL)}>
            MIT License
            <ExternalLink aria-hidden />
          </Button>
        </FieldRow>
        <FieldRow label="源代码">
          <Button type="button" variant="link" className="h-auto px-0" onClick={() => void api.openUrl(REPO_URL)}>
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
