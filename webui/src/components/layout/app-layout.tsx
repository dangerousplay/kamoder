import * as React from "react"
import { Outlet, useLocation } from "react-router"

import { AppSidebar } from "@/components/layout/app-sidebar"
import { NAV_ITEMS } from "@/components/layout/nav"
import { ThemeToggle } from "@/components/layout/theme-toggle"
import { UserMenu } from "@/components/layout/user-menu"
import { Separator } from "@/components/ui/separator"
import { Skeleton } from "@/components/ui/skeleton"
import {
  SidebarInset,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar"

function usePageTitle() {
  const { pathname } = useLocation()
  const match = NAV_ITEMS.find((i) =>
    i.to === "/" ? pathname === "/" : pathname.startsWith(i.to)
  )
  return match?.title ?? ""
}

export function AppLayout() {
  const title = usePageTitle()
  return (
    <SidebarProvider>
      <AppSidebar />
      <SidebarInset>
        <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-2 h-4" />
          <h1 className="text-sm font-medium">{title}</h1>
          <div className="ml-auto flex items-center gap-1">
            <ThemeToggle />
            <UserMenu />
          </div>
        </header>
        <main className="flex flex-1 flex-col gap-4 p-4">
          <React.Suspense fallback={<Skeleton className="h-32 w-full" />}>
            <Outlet />
          </React.Suspense>
        </main>
      </SidebarInset>
    </SidebarProvider>
  )
}
