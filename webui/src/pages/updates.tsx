import * as React from "react"
import {
  DownloadIcon,
  ExternalLinkIcon,
  Loader2Icon,
  PowerIcon,
  RefreshCwIcon,
  RotateCcwIcon,
  ShieldAlertIcon,
  TriangleAlertIcon,
  UploadIcon,
} from "lucide-react"
import { createPortal } from "react-dom"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/common/confirm-dialog"
import { RebootingScreen } from "@/components/settings/reboot-card"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Separator } from "@/components/ui/separator"
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import { usePolling } from "@/hooks/use-polling"
import {
  ApiError,
  api,
  errorMessage,
  type UpdateChannel,
  type UpdatePhase,
  type UpdateProgress,
  type UpdateSettings,
  type UpdateSlot,
  type UpdateStatus,
} from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { formatBytes, formatDuration } from "@/lib/format"
import { cn } from "@/lib/utils"

/** Health threshold the updater commits a trial slot at (design §6). */
const COMMIT_AFTER_S = 180
const FAST_MS = 2000
const SLOW_MS = 10000

const BUSY_PHASES: UpdatePhase[] = [
  "checking",
  "downloading",
  "verifying",
  "installing",
]

const PHASE_LABEL: Record<UpdatePhase, string> = {
  idle: "Idle",
  checking: "Checking for updates",
  downloading: "Downloading",
  verifying: "Verifying",
  installing: "Installing",
  awaiting_reboot: "Waiting for reboot",
  trial: "Trial boot",
  error: "Error",
}

/** Friendlier text for the codes the update service refuses with. */
const CODE_HINT: Record<string, string> = {
  busy: "Another check, download or install is running.",
  trial:
    "The running version is still on trial; installing now would overwrite the fallback slot. Wait until it commits.",
  clock_not_set: "The camera clock is not set yet (NTP); try again shortly.",
  no_trusted_key: "This firmware has no update signing key.",
  bad_signature: "The package signature is not valid.",
  unknown_key: "The package is signed with a key this firmware does not trust.",
  replay:
    "The package is not newer than the installed one (use force reinstall for the same release).",
  board: "The package is for a different board.",
  target: "The package is for a different target.",
  below_floor: "The package is older than the security floor.",
  updater_too_old:
    "The package needs a newer updater; install an intermediate release first.",
  bad_package: "The package is damaged or incomplete.",
  no_space: "Not enough free space on the SD card.",
  no_rollback_target: "The other slot is not bootable and committed.",
  rate_limited: "GitHub rate limit reached; try again later.",
  network: "Could not reach the update server.",
}

function updateErrorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === "update_unavailable") return "Update service not running"
    const hint = CODE_HINT[e.code]
    return hint && hint !== e.message ? `${hint} (${e.message})` : e.message
  }
  return errorMessage(e)
}

function formatTime(unix: number | null | undefined): string {
  if (unix == null) return "never"
  return new Date(unix * 1000).toLocaleString()
}

function progressLabel(p: UpdateProgress): string {
  const what = p.what.startsWith("install:")
    ? `Installing ${p.what.slice("install:".length)}`
    : p.what === "download"
      ? "Downloading"
      : p.what === "verify"
        ? "Verifying"
        : p.what
  const bytes =
    p.bytes_total != null
      ? `${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`
      : formatBytes(p.bytes_done)
  return `${what}: ${bytes}`
}

/** Re-renders every `ms` while `enabled`; answers the current time. */
function useNow(enabled: boolean, ms = 1000): number {
  const [now, setNow] = React.useState(() => Date.now())
  React.useEffect(() => {
    if (!enabled) return
    const t = window.setInterval(() => setNow(Date.now()), ms)
    return () => window.clearInterval(t)
  }, [enabled, ms])
  return now
}

function ProgressBar({
  value,
  max,
  label,
}: {
  value: number
  max: number | null
  label: string
}) {
  const pct = max ? Math.min(100, Math.round((value / max) * 100)) : null
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={pct ?? undefined}
      aria-valuetext={pct != null ? `${pct}%` : label}
      className="h-2 w-full overflow-hidden rounded-full bg-muted"
    >
      <div
        className={cn(
          "h-full rounded-full bg-primary transition-[width]",
          pct == null && "w-1/3 animate-pulse"
        )}
        style={pct != null ? { width: `${pct}%` } : undefined}
      />
    </div>
  )
}

function Row({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="flex items-baseline justify-between gap-4 text-sm">
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="text-right font-medium">{children}</dd>
    </div>
  )
}

// ------------------------------------------------------------ cards ----

function CurrentCard({
  s,
  fetchedAt,
  rebootPending,
  isAdmin,
  onReboot,
}: {
  s: UpdateStatus
  fetchedAt: number
  rebootPending: boolean
  isAdmin: boolean
  onReboot: () => void
}) {
  const now = useNow(s.health != null)
  const elapsed = Math.max(0, Math.floor((now - fetchedAt) / 1000))
  const deadline = s.health ? Math.max(0, s.health.deadline_s - elapsed) : 0

  return (
    <Card>
      <CardHeader>
        <CardTitle>Current system</CardTitle>
        <CardDescription>The firmware this camera is running</CardDescription>
        <CardAction>
          {s.trial ? (
            <Badge className="bg-amber-500/15 text-amber-800 dark:text-amber-300">
              Trial
            </Badge>
          ) : s.committed ? (
            <Badge className="bg-emerald-500/15 text-emerald-800 dark:text-emerald-300">
              Committed
            </Badge>
          ) : (
            <Badge variant="outline">Not committed</Badge>
          )}
        </CardAction>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">
        <dl className="flex flex-col gap-2">
          <Row label="Version">
            <span className="font-mono">{s.version}</span>
          </Row>
          <Row label="Slot">{s.slot.toUpperCase()}</Row>
          <Row label="Status">{PHASE_LABEL[s.phase]}</Row>
          <Row label="Last check">{formatTime(s.last_check)}</Row>
        </dl>

        {s.health && (
          <div className="flex flex-col gap-2 rounded-lg border border-amber-600/40 p-3 text-sm">
            <p className="font-medium">
              Health check: {Math.min(s.health.ok_for_s, COMMIT_AFTER_S)} s /{" "}
              {COMMIT_AFTER_S} s healthy
            </p>
            <ProgressBar
              value={s.health.ok_for_s}
              max={COMMIT_AFTER_S}
              label="Trial health check"
            />
            {s.health.waiting_on.length > 0 && (
              <div>
                <p className="text-muted-foreground">Waiting on:</p>
                <ul className="list-inside list-disc font-mono text-xs">
                  {s.health.waiting_on.map((w) => (
                    <li key={w}>{w}</li>
                  ))}
                </ul>
              </div>
            )}
            <p className="text-muted-foreground tabular-nums">
              {deadline > 0
                ? `Automatic rollback to the previous slot in ${formatDuration(deadline)} unless healthy.`
                : "Rollback deadline passed; the boot guard reboots into the previous slot."}
            </p>
          </div>
        )}

        <div aria-live="polite" className="flex flex-col gap-2">
          {s.progress && (
            <>
              <p className="text-sm tabular-nums">
                {progressLabel(s.progress)}
              </p>
              <ProgressBar
                value={s.progress.bytes_done}
                max={s.progress.bytes_total}
                label={progressLabel(s.progress)}
              />
            </>
          )}
          {rebootPending && (
            <p className="flex items-center gap-2 text-sm">
              <Loader2Icon className="size-4 animate-spin" />
              Installing; the camera reboots into the new version when done.
            </p>
          )}
        </div>

        {s.phase === "awaiting_reboot" && !rebootPending && (
          <Alert>
            <PowerIcon />
            <AlertTitle>Reboot to finish</AlertTitle>
            <AlertDescription>
              The other slot boots next. Reboot to start it.
              {isAdmin && (
                <Button className="mt-2" size="sm" onClick={onReboot}>
                  <PowerIcon />
                  Reboot now
                </Button>
              )}
            </AlertDescription>
          </Alert>
        )}

        {s.last_error && (
          <Alert variant="destructive">
            <TriangleAlertIcon />
            <AlertTitle>
              Last error: <code>{s.last_error.code}</code>
              {s.last_error.at != null && (
                <span className="font-normal text-muted-foreground">
                  {" "}
                  ({formatTime(s.last_error.at)})
                </span>
              )}
            </AlertTitle>
            <AlertDescription>
              {CODE_HINT[s.last_error.code] && (
                <p>{CODE_HINT[s.last_error.code]}</p>
              )}
              <p>{s.last_error.message}</p>
            </AlertDescription>
          </Alert>
        )}
      </CardContent>
    </Card>
  )
}

function SlotBox({ slot, booted }: { slot: UpdateSlot; booted: boolean }) {
  return (
    <div
      className={cn(
        "flex flex-col gap-2 rounded-lg border p-3",
        booted && "border-primary"
      )}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="font-medium">Slot {slot.slot.toUpperCase()}</span>
        {booted && <Badge>Booted</Badge>}
      </div>
      <dl className="flex flex-col gap-1">
        <Row label="Version">
          <span className="font-mono">{slot.version ?? "unknown"}</span>
        </Row>
        <Row label="Bootable">{slot.bootable ? "yes" : "no"}</Row>
        <Row label="Successful">{slot.successful ? "yes" : "no"}</Row>
        <Row label="Tries left">{slot.tries_left}</Row>
        {!slot.dir_present && (
          <p className="text-xs text-muted-foreground">
            Empty (no code on card)
          </p>
        )}
      </dl>
    </div>
  )
}

function SlotsCard({
  s,
  isAdmin,
  busy,
  onRollback,
}: {
  s: UpdateStatus
  isAdmin: boolean
  busy: boolean
  onRollback: () => Promise<void>
}) {
  const [confirm, setConfirm] = React.useState(false)
  const other = s.slots.find((x) => x.slot !== s.slot)
  const canRollback = !!other && other.bootable && other.successful

  return (
    <Card>
      <CardHeader>
        <CardTitle>Slots</CardTitle>
        <CardDescription>
          Updates install into the slot that is not running; the old one stays
          as a fallback.
        </CardDescription>
        {isAdmin && (
          <CardAction>
            <Button
              variant="outline"
              size="sm"
              disabled={!canRollback || busy}
              onClick={() => setConfirm(true)}
              title={
                canRollback
                  ? undefined
                  : "The other slot is not bootable and committed"
              }
            >
              <RotateCcwIcon />
              Roll back
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="grid gap-3 sm:grid-cols-2">
        {s.slots.map((slot) => (
          <SlotBox key={slot.slot} slot={slot} booted={slot.slot === s.slot} />
        ))}
      </CardContent>
      {other && (
        <ConfirmDialog
          open={confirm}
          onOpenChange={setConfirm}
          title={`Roll back to ${other.version ?? `slot ${other.slot.toUpperCase()}`}?`}
          description="The camera switches to the other slot and reboots right away. Video, ONVIF and SSH are offline for about a minute."
          confirmLabel="Roll back and reboot"
          destructive
          onConfirm={onRollback}
        />
      )}
    </Card>
  )
}

function AvailableCard({
  s,
  isAdmin,
  busy,
  locked,
  pending,
  onCheck,
  onDownload,
  onInstall,
}: {
  s: UpdateStatus
  isAdmin: boolean
  busy: boolean
  locked: boolean
  pending: string | null
  onCheck: () => void
  onDownload: () => void
  onInstall: () => Promise<void>
}) {
  const [confirm, setConfirm] = React.useState(false)
  const a = s.available
  const disabled = busy || locked

  return (
    <Card>
      <CardHeader>
        <CardTitle>Available update</CardTitle>
        <CardDescription>
          Channel <span className="font-medium">{s.settings.channel}</span> from{" "}
          <span className="font-mono">{s.settings.repo}</span>
        </CardDescription>
        {isAdmin && (
          <CardAction>
            <Button
              variant="outline"
              size="sm"
              disabled={disabled}
              onClick={onCheck}
            >
              {pending === "check" || s.phase === "checking" ? (
                <Loader2Icon className="animate-spin" />
              ) : (
                <RefreshCwIcon />
              )}
              Check now
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {a ? (
          <>
            <dl className="flex flex-col gap-2">
              <Row label="Version">
                <span className="font-mono">{a.version}</span>
              </Row>
              <Row label="Channel">{a.channel}</Row>
              {a.size != null && <Row label="Size">{formatBytes(a.size)}</Row>}
              <Row label="Downloaded">
                {a.downloaded ? (
                  <Badge className="bg-emerald-500/15 text-emerald-800 dark:text-emerald-300">
                    Downloaded and verified
                  </Badge>
                ) : (
                  "not yet"
                )}
              </Row>
            </dl>
            {a.downgrade && (
              <Alert className="border-amber-600/40 text-amber-800 dark:text-amber-300">
                <TriangleAlertIcon />
                <AlertTitle>Downgrade</AlertTitle>
                <AlertDescription>
                  Version {a.version} is older than the running {s.version}. It
                  was published as a deliberate rollback release.
                </AlertDescription>
              </Alert>
            )}
            {a.notes_url && (
              <a
                href={a.notes_url}
                target="_blank"
                rel="noreferrer noopener"
                className="inline-flex items-center gap-1 text-sm text-primary underline-offset-4 hover:underline"
              >
                Release notes
                <ExternalLinkIcon className="size-3.5" />
              </a>
            )}
          </>
        ) : (
          <p className="text-sm text-muted-foreground">
            {s.last_check
              ? "The camera is up to date."
              : "No check has run yet."}
          </p>
        )}
      </CardContent>
      {isAdmin && a && (
        <CardFooter className="flex flex-wrap gap-2">
          {!a.downloaded && (
            <Button variant="outline" disabled={disabled} onClick={onDownload}>
              {pending === "download" || s.phase === "downloading" ? (
                <Loader2Icon className="animate-spin" />
              ) : (
                <DownloadIcon />
              )}
              Download
            </Button>
          )}
          <Button
            disabled={disabled || !a.downloaded}
            onClick={() => setConfirm(true)}
          >
            <PowerIcon />
            Install &amp; reboot
          </Button>
        </CardFooter>
      )}
      {a && (
        <ConfirmDialog
          open={confirm}
          onOpenChange={setConfirm}
          title={`Install ${a.version} and reboot?`}
          description={
            <>
              The update is written to the inactive slot; the running version
              stays untouched as a fallback. The camera then reboots into{" "}
              {a.version} on trial. If it is not healthy within about 10
              minutes, it reboots back into {s.version} automatically.
              {a.downgrade && " This is a downgrade."}
            </>
          }
          confirmLabel="Install & reboot"
          onConfirm={onInstall}
        />
      )}
    </Card>
  )
}

function UploadCard({
  locked,
  busy,
  onUploaded,
}: {
  locked: boolean
  busy: boolean
  onUploaded: (reboot: boolean) => void
}) {
  const [file, setFile] = React.useState<File | null>(null)
  const [force, setForce] = React.useState(false)
  const [reboot, setReboot] = React.useState(true)
  const [progress, setProgress] = React.useState<{
    done: number
    total: number
  } | null>(null)
  const inputRef = React.useRef<HTMLInputElement>(null)
  const uploading = progress !== null

  async function upload(e: React.FormEvent) {
    e.preventDefault()
    if (!file) return
    setProgress({ done: 0, total: file.size })
    try {
      await api.updateUpload(file, { force, reboot }, (done, total) =>
        setProgress({ done, total })
      )
      toast.success(
        reboot
          ? "Package accepted; installing, then rebooting"
          : "Package accepted; installing"
      )
      setFile(null)
      if (inputRef.current) inputRef.current.value = ""
      onUploaded(reboot)
    } catch (err) {
      toast.error(`Upload failed: ${updateErrorMessage(err)}`)
    } finally {
      setProgress(null)
    }
  }

  const disabled = locked || busy || uploading

  return (
    <Card>
      <CardHeader>
        <CardTitle>Manual upload</CardTitle>
        <CardDescription>
          Install a signed <code>.kup</code> package from this computer, e.g.
          for a camera without internet access. It is verified the same way as a
          download.
        </CardDescription>
      </CardHeader>
      <form onSubmit={upload}>
        <CardContent>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="update-file">Package file</FieldLabel>
              <Input
                ref={inputRef}
                id="update-file"
                type="file"
                accept=".kup"
                disabled={disabled}
                onChange={(e) => setFile(e.target.files?.[0] ?? null)}
              />
              {file && (
                <FieldDescription>
                  {file.name}, {formatBytes(file.size)}
                </FieldDescription>
              )}
            </Field>
            <Field orientation="horizontal">
              <FieldContent>
                <FieldLabel htmlFor="update-force">Force reinstall</FieldLabel>
                <FieldDescription>
                  Accept a package of the same release that is already installed
                  (to repair a slot). Older packages and ones below the security
                  floor are always refused.
                </FieldDescription>
              </FieldContent>
              <Switch
                id="update-force"
                checked={force}
                onCheckedChange={setForce}
                disabled={disabled}
              />
            </Field>
            <Field orientation="horizontal">
              <FieldContent>
                <FieldLabel htmlFor="update-reboot">
                  Reboot when installed
                </FieldLabel>
                <FieldDescription>
                  Otherwise the new version starts on the next reboot.
                </FieldDescription>
              </FieldContent>
              <Switch
                id="update-reboot"
                checked={reboot}
                onCheckedChange={setReboot}
                disabled={disabled}
              />
            </Field>
            <div aria-live="polite">
              {progress && (
                <div className="flex flex-col gap-2">
                  <p className="text-sm tabular-nums">
                    Uploading: {formatBytes(progress.done)} /{" "}
                    {formatBytes(progress.total)}
                    {progress.total > 0 && progress.done >= progress.total
                      ? " (verifying…)"
                      : ""}
                  </p>
                  <ProgressBar
                    value={progress.done}
                    max={progress.total}
                    label="Upload progress"
                  />
                </div>
              )}
            </div>
          </FieldGroup>
        </CardContent>
        <CardFooter className="mt-4">
          <Button type="submit" disabled={disabled || !file}>
            {uploading ? (
              <Loader2Icon className="animate-spin" />
            ) : (
              <UploadIcon />
            )}
            Upload &amp; install
          </Button>
        </CardFooter>
      </form>
    </Card>
  )
}

const HOURS = Array.from({ length: 24 }, (_, h) => h)
const hourLabel = (h: number) => `${String(h).padStart(2, "0")}:00`

function HourSelect({
  id,
  value,
  onChange,
  disabled,
}: {
  id: string
  value: number
  onChange: (h: number) => void
  disabled: boolean
}) {
  return (
    <Select
      value={String(value)}
      onValueChange={(v) => onChange(Number(v))}
      disabled={disabled}
    >
      <SelectTrigger id={id} className="w-28">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {HOURS.map((h) => (
          <SelectItem key={h} value={String(h)}>
            {hourLabel(h)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

function SettingsCard({
  settings,
  onSaved,
}: {
  settings: UpdateSettings
  onSaved: (s: UpdateStatus) => void
}) {
  const [draft, setDraft] = React.useState(settings)
  const [pending, setPending] = React.useState(false)
  const set = (patch: Partial<UpdateSettings>) =>
    setDraft((d) => ({ ...d, ...patch }))
  const dirty = JSON.stringify(draft) !== JSON.stringify(settings)

  async function save(e: React.FormEvent) {
    e.preventDefault()
    setPending(true)
    try {
      onSaved(await api.updateSettings({ ...draft, repo: draft.repo.trim() }))
      toast.success("Update settings saved")
    } catch (err) {
      toast.error(`Save failed: ${updateErrorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Update settings</CardTitle>
      </CardHeader>
      <form onSubmit={save}>
        <CardContent>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="update-channel">Channel</FieldLabel>
              <Select
                value={draft.channel}
                onValueChange={(v) => set({ channel: v as UpdateChannel })}
                disabled={pending}
              >
                <SelectTrigger id="update-channel" className="w-full sm:w-48">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="stable">Stable</SelectItem>
                  <SelectItem value="beta">Beta (pre-releases)</SelectItem>
                </SelectContent>
              </Select>
            </Field>
            <Field orientation="horizontal">
              <FieldContent>
                <FieldLabel htmlFor="update-auto-check">
                  Check automatically
                </FieldLabel>
                <FieldDescription>
                  Look for new releases periodically and download them in the
                  background.
                </FieldDescription>
              </FieldContent>
              <Switch
                id="update-auto-check"
                checked={draft.auto_check}
                onCheckedChange={(v) => set({ auto_check: v })}
                disabled={pending}
              />
            </Field>
            <Field orientation="horizontal">
              <FieldContent>
                <FieldLabel htmlFor="update-auto-install">
                  Install automatically
                </FieldLabel>
                <FieldDescription>
                  Install downloaded updates and reboot without asking.
                </FieldDescription>
              </FieldContent>
              <Switch
                id="update-auto-install"
                checked={draft.auto_install}
                onCheckedChange={(v) => set({ auto_install: v })}
                disabled={pending}
              />
            </Field>
            {draft.auto_install && (
              <>
                <Field orientation="horizontal">
                  <FieldContent>
                    <FieldLabel htmlFor="update-window">
                      Only in a maintenance window
                    </FieldLabel>
                    <FieldDescription>
                      Camera local time; a window like 22:00 → 04:00 spans
                      midnight. Off means any time.
                    </FieldDescription>
                  </FieldContent>
                  <Switch
                    id="update-window"
                    checked={draft.window !== null}
                    onCheckedChange={(v) =>
                      set({
                        window: v ? { start_hour: 3, end_hour: 5 } : null,
                      })
                    }
                    disabled={pending}
                  />
                </Field>
                {draft.window && (
                  <div className="flex flex-wrap items-end gap-4">
                    <Field className="w-auto">
                      <FieldLabel htmlFor="update-window-start">
                        From
                      </FieldLabel>
                      <HourSelect
                        id="update-window-start"
                        value={draft.window.start_hour}
                        onChange={(h) =>
                          set({
                            window: { ...draft.window!, start_hour: h },
                          })
                        }
                        disabled={pending}
                      />
                    </Field>
                    <Field className="w-auto">
                      <FieldLabel htmlFor="update-window-end">Until</FieldLabel>
                      <HourSelect
                        id="update-window-end"
                        value={draft.window.end_hour}
                        onChange={(h) =>
                          set({ window: { ...draft.window!, end_hour: h } })
                        }
                        disabled={pending}
                      />
                    </Field>
                  </div>
                )}
              </>
            )}
            <details className="text-sm">
              <summary className="cursor-pointer text-muted-foreground">
                Advanced
              </summary>
              <Field className="mt-3">
                <FieldLabel htmlFor="update-repo">GitHub repository</FieldLabel>
                <Input
                  id="update-repo"
                  value={draft.repo}
                  onChange={(e) => set({ repo: e.target.value })}
                  placeholder="owner/name"
                  spellCheck={false}
                  autoComplete="off"
                  disabled={pending}
                />
                <FieldDescription>
                  Releases are taken from this repository. Packages must still
                  be signed with the key built into this firmware.
                </FieldDescription>
              </Field>
            </details>
          </FieldGroup>
        </CardContent>
        <CardFooter className="mt-4 gap-2">
          <Button type="submit" disabled={pending || !dirty}>
            {pending && <Loader2Icon className="animate-spin" />}
            Save
          </Button>
          {dirty && (
            <Button
              type="button"
              variant="ghost"
              disabled={pending}
              onClick={() => setDraft(settings)}
            >
              Discard
            </Button>
          )}
        </CardFooter>
      </form>
    </Card>
  )
}

// ------------------------------------------------------------- page ----

interface Fetched {
  s: UpdateStatus
  /** Browser time the status arrived (for local countdowns). */
  at: number
}

const fetchStatus = async (signal: AbortSignal): Promise<Fetched> => ({
  s: await api.updateStatus(signal),
  at: Date.now(),
})

export default function UpdatesPage() {
  const { isAdmin } = useAuth()
  const [pending, setPending] = React.useState<string | null>(null)
  /**
   * `install`: an install with reboot runs; the camera goes down when done.
   * `down`: it is rebooting; the reconnect screen takes over.
   */
  const [reboot, setReboot] = React.useState<{
    stage: "install" | "down"
    since: number
  } | null>(null)

  const [fast, setFast] = React.useState(false)
  const status = usePolling(fetchStatus, fast ? FAST_MS : SLOW_MS)
  const s = status.data?.s

  const phaseBusy = !!s && BUSY_PHASES.includes(s.phase)
  const wantFast =
    pending !== null ||
    reboot !== null ||
    (!!s && s.phase !== "idle" && s.phase !== "trial" && s.phase !== "error")
  if (wantFast !== fast) setFast(wantFast)

  // While an install-with-reboot runs, losing the API means the reboot
  // started; a failed job (fresh error status) means it never will.
  if (reboot?.stage === "install") {
    if (status.error) {
      setReboot({ stage: "down", since: reboot.since })
    } else if (
      status.data &&
      status.data.at > reboot.since &&
      status.data.s.phase === "error"
    ) {
      setReboot(null)
    }
  }

  const locked = !!s && !s.trusted_key
  const busy = phaseBusy || pending !== null || reboot !== null

  async function run<T>(what: string, fn: () => Promise<T>): Promise<T> {
    setPending(what)
    try {
      return await fn()
    } catch (e) {
      toast.error(updateErrorMessage(e))
      throw e
    } finally {
      setPending(null)
      status.refresh()
    }
  }

  const check = () =>
    run("check", api.updateCheck)
      .then((next) => {
        status.setData({ s: next, at: Date.now() })
        toast.success(
          next.available
            ? `Update available: ${next.available.version}`
            : "The camera is up to date"
        )
      })
      .catch(() => {})

  const download = () =>
    run("download", api.updateDownload)
      .then(() => toast.success("Download started"))
      .catch(() => {})

  async function install() {
    const since = Date.now()
    await run("install", () =>
      api.updateInstall({ force: false, reboot: true })
    )
    setReboot({ stage: "install", since })
  }

  async function rollback() {
    await run("rollback", () => api.updateRollback({ reboot: true }))
    setReboot({ stage: "down", since: Date.now() })
  }

  const rebootNow = () =>
    run("reboot", api.updateReboot)
      .then(() => setReboot({ stage: "down", since: Date.now() }))
      .catch(() => {})

  if (!s) {
    if (status.error) {
      const unavailable =
        status.error instanceof ApiError && status.error.isUnavailable
      return (
        <Card>
          <CardHeader>
            <CardTitle>
              {unavailable
                ? "Update service not running"
                : "Could not load update status"}
            </CardTitle>
            <CardDescription>
              {unavailable
                ? "The update service is stopped or still starting. Check it on the Services page."
                : errorMessage(status.error)}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button variant="outline" size="sm" onClick={status.refresh}>
              Retry
            </Button>
          </CardContent>
        </Card>
      )
    }
    return (
      <div className="grid gap-4 lg:grid-cols-2">
        {Array.from({ length: 4 }, (_, i) => (
          <Skeleton key={i} className="h-56 w-full" />
        ))}
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-4">
      {locked && (
        <Alert variant="destructive">
          <ShieldAlertIcon />
          <AlertTitle>No update signing key</AlertTitle>
          <AlertDescription>
            No update signing key built into this firmware; updates cannot be
            verified. Checking, installing and uploading are disabled.
          </AlertDescription>
        </Alert>
      )}
      {status.error != null && reboot === null && (
        <Alert variant="destructive" aria-live="polite">
          <TriangleAlertIcon />
          <AlertTitle>
            {status.error instanceof ApiError && status.error.isUnavailable
              ? "Update service not running"
              : "Status is stale"}
          </AlertTitle>
          <AlertDescription>{errorMessage(status.error)}</AlertDescription>
        </Alert>
      )}
      <div className="grid items-start gap-4 lg:grid-cols-2">
        <div className="flex flex-col gap-4">
          <CurrentCard
            s={s}
            fetchedAt={status.data!.at}
            rebootPending={reboot?.stage === "install"}
            isAdmin={isAdmin}
            onReboot={rebootNow}
          />
          <SlotsCard
            s={s}
            isAdmin={isAdmin}
            busy={busy}
            onRollback={rollback}
          />
        </div>
        <div className="flex flex-col gap-4">
          <AvailableCard
            s={s}
            isAdmin={isAdmin}
            busy={busy}
            locked={locked}
            pending={pending}
            onCheck={check}
            onDownload={download}
            onInstall={install}
          />
          {isAdmin && (
            <UploadCard
              locked={locked}
              busy={busy}
              onUploaded={(r) => {
                status.refresh()
                if (r) setReboot({ stage: "install", since: Date.now() })
              }}
            />
          )}
          {isAdmin && (
            <SettingsCard
              key={JSON.stringify(s.settings)}
              settings={s.settings}
              onSaved={(next) => status.setData({ s: next, at: Date.now() })}
            />
          )}
        </div>
      </div>
      {!isAdmin && (
        <>
          <Separator />
          <p className="text-sm text-muted-foreground">
            Only administrators can install updates or change update settings.
          </p>
        </>
      )}
      {reboot?.stage === "down" &&
        createPortal(<RebootingScreen />, document.body)}
    </div>
  )
}
