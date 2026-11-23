import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"
import { cn } from "@/lib/utils"
import { Slot } from "radix-ui"

// 按钮层级随平台：default 为 Teal 主操作；outline 为次级按钮（Fluent Standard、macOS 推按钮、Adwaita 平涂）；
// ghost 为命令栏和行内的无框按钮；destructive 为次级按钮加红色文字。尺寸取自 tokens.css 的 --control-h。
const buttonVariants = cva(
  "group/button inline-flex shrink-0 items-center justify-center rounded-md border border-transparent bg-clip-padding text-(length:--text-ui) font-normal whitespace-nowrap transition-colors duration-(--duration) outline-none select-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background mac:focus-visible:ring-3 mac:focus-visible:ring-ring/50 mac:focus-visible:ring-offset-0 disabled:pointer-events-none disabled:opacity-45 aria-invalid:border-destructive linux:font-semibold [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
  {
    variants: {
      variant: {
        default:
          "bg-primary text-primary-foreground hover:bg-primary/90 active:bg-primary/80 win:border-white/10 win:border-b-black/25 mac:shadow-(--control-shadow)",
        outline:
          "border-control-border border-b-control-border-strong bg-control text-foreground shadow-(--control-shadow) hover:bg-control-hover active:bg-control-pressed active:text-muted-foreground aria-expanded:bg-control-hover",
        secondary:
          "bg-secondary text-secondary-foreground hover:bg-control-hover active:bg-control-pressed aria-expanded:bg-secondary",
        ghost:
          "hover:bg-accent hover:text-foreground active:bg-control-pressed aria-expanded:bg-accent",
        destructive:
          "border-control-border border-b-control-border-strong bg-control text-destructive shadow-(--control-shadow) hover:bg-control-hover active:bg-control-pressed",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default:
          "h-(--control-h) gap-1.5 px-3 mac:px-2.5 linux:px-3.5 has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2",
        xs: "h-6 gap-1 px-2 text-xs has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 [&_svg:not([class*='size-'])]:size-3",
        sm: "h-(--control-h-sm) gap-1 px-2.5 text-[13px] has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 [&_svg:not([class*='size-'])]:size-3.5",
        lg: "h-[calc(var(--control-h)+4px)] gap-1.5 px-3.5 has-data-[icon=inline-end]:pr-2 has-data-[icon=inline-start]:pl-2",
        icon: "size-(--control-h)",
        "icon-xs": "size-6 [&_svg:not([class*='size-'])]:size-3",
        "icon-sm": "size-(--control-h-sm)",
        "icon-lg": "size-[calc(var(--control-h)+4px)]",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

function Button({
  className,
  variant = "default",
  size = "default",
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean
  }) {
  const Comp = asChild ? Slot.Root : "button"

  return (
    <Comp
      data-slot="button"
      data-variant={variant}
      data-size={size}
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  )
}

export { Button, buttonVariants }
