import type { PageInfo } from "@/pages/pages";

interface PagePlaceholderProps {
  page: PageInfo;
}

export function PagePlaceholder({ page }: PagePlaceholderProps) {
  const Icon = page.icon;

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col">
      <header className="flex items-center gap-2.5 px-5 pt-3.5">
        <h1 className="m-0 text-title font-(--weight-title)">{page.label}</h1>
      </header>
      <div className="mx-5 mt-3.5 mb-4 flex flex-1 items-center justify-center rounded-xl border bg-card">
        <div className="flex max-w-sm flex-col items-center gap-2 px-6 py-14 text-center">
          <span className="mb-1 grid size-10 place-items-center rounded-full bg-muted text-muted-foreground">
            <Icon className="size-5" aria-hidden />
          </span>
          <p className="m-0 text-sm font-medium">{page.emptyTitle}</p>
          <p className="m-0 text-muted-foreground">{page.emptyDescription}</p>
        </div>
      </div>
    </section>
  );
}
