import { useState } from "react";

import { AppSidebar } from "@/components/app-sidebar";
import { StatusBar } from "@/components/status-bar";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PagePlaceholder } from "@/pages/page-placeholder";
import { pages, type PageId } from "@/pages/pages";
import { SettingsPage } from "@/pages/settings/settings-page";

export default function App() {
  const [current, setCurrent] = useState<PageId>("rooms");
  const page = pages.find((item) => item.id === current) ?? pages[0];

  return (
    <TooltipProvider delayDuration={400}>
      <div className="grid h-screen grid-cols-[200px_minmax(0,1fr)] grid-rows-[minmax(0,1fr)_auto]">
        <AppSidebar current={current} onNavigate={setCurrent} />
        <main className="flex min-h-0 min-w-0">
          {page.id === "settings" ? <SettingsPage /> : <PagePlaceholder key={page.id} page={page} />}
        </main>
        <StatusBar />
      </div>
      <Toaster position="bottom-right" offset={{ bottom: 44, right: 16 }} />
    </TooltipProvider>
  );
}
