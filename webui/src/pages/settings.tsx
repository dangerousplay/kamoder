import { ConfigForm } from "@/components/settings/config-form"
import { OnvifCard } from "@/components/settings/onvif-card"
import { RebootCard } from "@/components/settings/reboot-card"
import { SshCard } from "@/components/settings/ssh-card"
import { WifiPanel } from "@/components/settings/wifi-panel"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePolling } from "@/hooks/use-polling"
import { api, errorMessage } from "@/lib/api"
import { useNavigate } from "react-router"
import { toast } from "sonner"

export default function SettingsPage() {
  const config = usePolling(api.config, null)
  const onvifUser = config.data?.values.onvif_user
  const navigate = useNavigate()

  async function rerunSetup() {
    try {
      await api.reopenSetup()
      navigate("/setup")
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <div className="flex flex-col gap-4">
        {config.data ? (
          <ConfigForm config={config.data} onSaved={config.refresh} />
        ) : config.error ? (
          <Card>
            <CardHeader className="text-sm text-destructive">
              Could not load camera.conf: {errorMessage(config.error)}
            </CardHeader>
            <CardContent>
              <Button variant="outline" size="sm" onClick={config.refresh}>
                Retry
              </Button>
            </CardContent>
          </Card>
        ) : (
          <Card>
            <CardContent className="flex flex-col gap-4">
              {Array.from({ length: 6 }, (_, i) => (
                <Skeleton key={i} className="h-10 w-full" />
              ))}
            </CardContent>
          </Card>
        )}
      </div>
      <div className="flex flex-col gap-4">
        <WifiPanel />
        {config.data && (
          <OnvifCard
            currentUser={typeof onvifUser === "string" ? onvifUser : undefined}
            passwordSet={config.data.secrets_set.includes("onvif_password")}
          />
        )}
        <SshCard />
        <Card>
          <CardHeader className="text-sm">
            First-run setup wizard: admin password, ONVIF and SSH in one go.
          </CardHeader>
          <CardContent>
            <Button variant="outline" size="sm" onClick={rerunSetup}>
              Run setup again
            </Button>
          </CardContent>
        </Card>
        <RebootCard />
      </div>
    </div>
  )
}
