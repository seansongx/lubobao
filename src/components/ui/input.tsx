import * as React from "react"
import { cn } from "@/lib/utils"

function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-slot="input"
      className={cn(
        "h-(--control-h) w-full min-w-0 rounded-md border border-control-border border-b-field-underline bg-field px-2.5 py-1 text-(length:--text-ui) text-foreground shadow-none transition-colors outline-none mac:shadow-[inset_0_0.5px_1px_rgb(0_0_0/0.06)] file:inline-flex file:h-6 file:border-0 file:bg-transparent file:text-sm file:font-medium file:text-foreground placeholder:text-muted-foreground focus-visible:border-b-2 focus-visible:border-b-primary mac:focus-visible:border-b mac:focus-visible:border-control-border mac:focus-visible:ring-3 mac:focus-visible:ring-ring/45 linux:focus-visible:border-b-0 linux:focus-visible:ring-2 linux:focus-visible:ring-ring/55 disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-2 aria-invalid:ring-destructive/20",
        className
      )}
      {...props}
    />
  )
}

export { Input }
