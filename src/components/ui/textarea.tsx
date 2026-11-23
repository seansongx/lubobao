import * as React from "react"
import { cn } from "@/lib/utils"

function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "flex field-sizing-content min-h-16 w-full rounded-md border border-control-border border-b-field-underline bg-field px-2.5 py-2 text-(length:--text-ui) transition-colors outline-none placeholder:text-muted-foreground focus-visible:border-b-2 focus-visible:border-b-primary mac:focus-visible:border-b mac:focus-visible:border-control-border mac:focus-visible:ring-3 mac:focus-visible:ring-ring/45 linux:focus-visible:border-b-0 linux:focus-visible:ring-2 linux:focus-visible:ring-ring/55 disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-2 aria-invalid:ring-destructive/20",
        className
      )}
      {...props}
    />
  )
}

export { Textarea }
