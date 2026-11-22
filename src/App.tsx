import { useState } from "react";

import { AppSidebar } from "@/components/app-sidebar";
import { StatusBar } from "@/components/status-bar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PagePlaceholder } from "@/pages/page-placeholder";
import { pages, type PageId } from "@/pages/pages";

export default function App() {
  const [current, setCurrent] = useState<PageId>("rooms");
  const page = pages.find((item) => item.id === current) ?? pages[0];

  return (
    <TooltipProvider delayDuration={400}>
      <div className="grid h-screen grid-cols-[200px_minmax(0,1fr)] grid-rows-[minmax(0,1fr)_auto]">
        <AppSidebar current={current} onNavigate={setCurrent} />
        <main className="flex min-h-0 min-w-0">
          <PagePlaceholder key={page.id} page={page} />
        </main>
        <StatusBar />
      </div>
    </TooltipProvider>
  );
}
