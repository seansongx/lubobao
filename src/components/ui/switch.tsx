"use client"

import * as React from "react"
import { cn } from "@/lib/utils"
import { Switch as SwitchPrimitive } from "radix-ui"

function Switch({
  className,
  size = "default",
  ...props
}: React.ComponentProps<typeof SwitchPrimitive.Root> & {
  size?: "sm" | "default"
}) {
  return (
    <SwitchPrimitive.Root
      data-slot="switch"
      data-size={size}
      className={cn(
        "peer group/switch relative inline-flex shrink-0 items-center rounded-full border border-transparent transition-all outline-none group-has-[:focus-visible]/field-label:border-transparent group-has-[:focus-visible]/field-label:ring-0 after:absolute after:-inset-x-3 after:-inset-y-2 focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 data-[size=default]:h-[18.4px] data-[size=default]:w-[32px] win:data-[size=default]:h-5 win:data-[size=default]:w-10 win:data-unchecked:border-foreground/60 win:data-unchecked:bg-transparent linux:data-[size=default]:h-[26px] linux:data-[size=default]:w-[46px] data-[size=sm]:h-[14px] data-[size=sm]:w-[24px] dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40 data-checked:bg-primary data-unchecked:bg-control-pressed mac:data-unchecked:bg-black/10 mac:dark:data-unchecked:bg-white/15 linux:data-unchecked:bg-control data-disabled:cursor-not-allowed data-disabled:opacity-50",
        className
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb
        data-slot="switch-thumb"
        className="pointer-events-none block rounded-full bg-white shadow-sm ring-0 transition-transform duration-(--duration) group-data-[size=default]/switch:size-4 win:group-data-[size=default]/switch:size-3 win:shadow-none win:data-unchecked:bg-foreground/60 win:group-data-[size=default]/switch:data-unchecked:translate-x-[3px] win:group-data-[size=default]/switch:data-checked:translate-x-[23px] win:data-checked:bg-primary-foreground linux:group-data-[size=default]/switch:size-5 linux:group-data-[size=default]/switch:data-unchecked:translate-x-[2px] linux:group-data-[size=default]/switch:data-checked:translate-x-[22px] group-data-[size=sm]/switch:size-3 group-data-[size=default]/switch:data-checked:translate-x-[calc(100%-2px)] group-data-[size=sm]/switch:data-checked:translate-x-[calc(100%-2px)] group-data-[size=default]/switch:data-unchecked:translate-x-0 group-data-[size=sm]/switch:data-unchecked:translate-x-0"
      />
    </SwitchPrimitive.Root>
  )
}

export { Switch }
