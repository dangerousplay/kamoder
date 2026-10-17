import * as React from "react"
import { createBrowserRouter, RouterProvider } from "react-router"

import { AppLayout } from "@/components/layout/app-layout"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { AuthProvider, RequireAdmin, RequireAuth } from "@/lib/auth"
import LoginPage from "@/pages/login"

// Pages are split into their own chunks; the camera serves them on demand.
const DashboardPage = React.lazy(() => import("@/pages/dashboard"))
const CamerasPage = React.lazy(() => import("@/pages/cameras"))
const ServicesPage = React.lazy(() => import("@/pages/services"))
const LogsPage = React.lazy(() => import("@/pages/logs"))
const PtzPage = React.lazy(() => import("@/pages/ptz"))
const SettingsPage = React.lazy(() => import("@/pages/settings"))
const UpdatesPage = React.lazy(() => import("@/pages/updates"))
const UsersPage = React.lazy(() => import("@/pages/users"))
const SetupPage = React.lazy(() => import("@/pages/setup"))
const NotFoundPage = React.lazy(() => import("@/pages/not-found"))

const router = createBrowserRouter([
  { path: "/login", element: <LoginPage /> },
  {
    element: (
      <RequireAuth>
        <AppLayout />
      </RequireAuth>
    ),
    children: [
      { index: true, element: <DashboardPage /> },
      { path: "cameras", element: <CamerasPage /> },
      { path: "services", element: <ServicesPage /> },
      { path: "logs", element: <LogsPage /> },
      { path: "ptz", element: <PtzPage /> },
      { path: "updates", element: <UpdatesPage /> },
      {
        path: "settings",
        element: (
          <RequireAdmin>
            <SettingsPage />
          </RequireAdmin>
        ),
      },
      {
        path: "setup",
        element: (
          <RequireAdmin>
            <SetupPage />
          </RequireAdmin>
        ),
      },
      {
        path: "users",
        element: (
          <RequireAdmin>
            <UsersPage />
          </RequireAdmin>
        ),
      },
      { path: "*", element: <NotFoundPage /> },
    ],
  },
])

export function App() {
  return (
    <AuthProvider>
      <TooltipProvider>
        <RouterProvider router={router} />
        <Toaster richColors position="top-center" />
      </TooltipProvider>
    </AuthProvider>
  )
}

export default App
