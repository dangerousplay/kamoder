import * as React from "react"
import { CheckCircle2Icon, Loader2Icon, TriangleAlertIcon } from "lucide-react"
import { toast } from "sonner"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import {
  Card,
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
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSeparator,
  FieldSet,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import {
  api,
  errorMessage,
  type ConfigResponse,
  type ConfigValue,
} from "@/lib/api"
import { encodeBool, isTruthy } from "@/lib/format"

type Kind = "bool" | "text" | "secret" | "level"

interface FieldDef {
  key: string
  label: string
  kind: Kind
  description?: React.ReactNode
  placeholder?: string
}

const SECTIONS: { title: string; fields: FieldDef[] }[] = [
  {
    title: "Stack",
    fields: [
      {
        key: "run_rust",
        label: "Run the Rust stack",
        kind: "bool",
        description:
          "Off: Factory/config.sh falls back to anyka_happy on the next boot.",
      },
      {
        key: "start_telnetd",
        label: "Start telnetd (port 23)",
        kind: "bool",
        description:
          "Unauthenticated root shell. Only keep it on until SSH has been proven on this camera.",
      },
      {
        key: "stop_vendor_ipc",
        label: "Stop the vendor IPC",
        kind: "bool",
        description:
          "Stops /ipc/ipc and its watchers before the Rust stack starts, freeing the ISP, ADC and /dev/watchdog (wdt_feeder takes over the watchdog). Off: the vendor stack keeps running alongside and may hold the camera hardware.",
      },
      {
        key: "log_level",
        label: "Log level",
        kind: "level",
        description: "tracing filter for every Rust binary.",
      },
      {
        key: "metrics",
        label: "Metrics",
        kind: "text",
        placeholder: "textfile:/mnt/disc1/anyka_rust/metrics",
        description: "Empty disables Prometheus metrics.",
      },
    ],
  },
  {
    title: "Wi-Fi",
    fields: [
      {
        key: "wifi_ssid",
        label: "SSID",
        kind: "text",
        placeholder: "(keep the current network)",
        description: "Empty keeps the camera on its current network.",
      },
      {
        key: "wifi_password",
        label: "Password",
        kind: "secret",
        description: "8–63 characters. Leave empty to keep it.",
      },
    ],
  },
  {
    title: "SSH (port 2222)",
    fields: [
      { key: "ssh_user", label: "User", kind: "text" },
      {
        key: "ssh_password",
        label: "Password",
        kind: "secret",
        description: "Leave empty to keep it.",
      },
    ],
  },
]

const FIELDS = SECTIONS.flatMap((s) => s.fields)
const LEVELS = ["error", "warn", "info", "debug", "trace"]

interface Change {
  key: string
  label: string
  from?: string
  to?: string
  secret: boolean
}

function show(v: ConfigValue | undefined, kind: Kind): string {
  if (kind === "bool") return isTruthy(v) ? "on" : "off"
  return v === undefined || v === "" ? "(empty)" : String(v)
}

function validate(key: string, value: string): string | null {
  if (
    key === "wifi_password" &&
    value &&
    (value.length < 8 || value.length > 63)
  ) {
    return "Wi-Fi password must be 8–63 characters"
  }
  return null
}

export function ConfigForm({
  config,
  onSaved,
}: {
  config: ConfigResponse
  onSaved: () => void
}) {
  const original = config.values
  const [draft, setDraft] = React.useState<Record<string, ConfigValue>>(() => {
    const d: Record<string, ConfigValue> = {}
    for (const f of FIELDS) {
      if (f.kind === "bool") d[f.key] = isTruthy(original[f.key])
      else if (f.kind !== "secret") d[f.key] = String(original[f.key] ?? "")
    }
    return d
  })
  const [secrets, setSecrets] = React.useState<Record<string, string>>({})
  const [pending, setPending] = React.useState(false)
  const [result, setResult] = React.useState<{
    changes: Change[]
    restart: string[]
  } | null>(null)

  const changes: Change[] = []
  for (const f of FIELDS) {
    if (f.kind === "secret") {
      if (secrets[f.key]) {
        changes.push({ key: f.key, label: f.label, secret: true })
      }
      continue
    }
    const before = original[f.key]
    const after = draft[f.key]
    const differs =
      f.kind === "bool"
        ? isTruthy(before) !== after
        : String(before ?? "") !== String(after)
    if (differs) {
      changes.push({
        key: f.key,
        label: f.label,
        from: show(before, f.kind),
        to: show(after, f.kind),
        secret: false,
      })
    }
  }

  const errors = Object.fromEntries(
    FIELDS.map((f) => [f.key, validate(f.key, secrets[f.key] ?? "")])
  )
  const invalid = Object.values(errors).some(Boolean)
  const ssidChanged = changes.some((c) => c.key === "wifi_ssid")

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault()
    if (changes.length === 0 || invalid) return
    const values: Record<string, ConfigValue> = {}
    for (const c of changes) {
      const f = FIELDS.find((x) => x.key === c.key)!
      if (f.kind === "secret") values[c.key] = secrets[c.key]
      else if (f.kind === "bool") {
        values[c.key] = encodeBool(original[c.key], draft[c.key] === true)
      } else values[c.key] = String(draft[c.key]).trim()
    }
    setPending(true)
    try {
      const res = await api.updateConfig({ values })
      const changed = res?.changed
      setResult({
        changes: changed
          ? changes.filter((c) => changed.includes(c.key))
          : changes,
        restart: res?.restart ?? [],
      })
      setSecrets({})
      toast.success("Settings saved")
      onSaved()
    } catch (err) {
      toast.error(`Save failed: ${errorMessage(err)}`)
    } finally {
      setPending(false)
    }
  }

  function renderField(f: FieldDef) {
    const id = `cfg-${f.key}`
    const unknownToServer = !(f.key in original) && f.kind !== "secret"
    if (f.kind === "bool") {
      return (
        <Field key={f.key} orientation="horizontal">
          <FieldContent>
            <FieldLabel htmlFor={id}>{f.label}</FieldLabel>
            {f.description && (
              <FieldDescription>{f.description}</FieldDescription>
            )}
          </FieldContent>
          <Switch
            id={id}
            checked={draft[f.key] === true}
            onCheckedChange={(v) => setDraft((d) => ({ ...d, [f.key]: v }))}
            disabled={pending}
          />
        </Field>
      )
    }
    if (f.kind === "level") {
      const value = String(draft[f.key] ?? "")
      return (
        <Field key={f.key}>
          <FieldLabel htmlFor={id}>{f.label}</FieldLabel>
          <Select
            value={value}
            onValueChange={(v) => setDraft((d) => ({ ...d, [f.key]: v }))}
            disabled={pending}
          >
            <SelectTrigger id={id} className="w-full sm:w-64">
              <SelectValue placeholder="info" />
            </SelectTrigger>
            <SelectContent>
              {value && !LEVELS.includes(value) && (
                <SelectItem value={value}>{value}</SelectItem>
              )}
              {LEVELS.map((l) => (
                <SelectItem key={l} value={l}>
                  {l}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {f.description && (
            <FieldDescription>{f.description}</FieldDescription>
          )}
        </Field>
      )
    }
    if (f.kind === "secret") {
      const isSet = config.secrets_set.includes(f.key)
      return (
        <Field key={f.key} data-invalid={!!errors[f.key] || undefined}>
          <FieldLabel htmlFor={id}>{f.label}</FieldLabel>
          <Input
            id={id}
            type="password"
            autoComplete="new-password"
            placeholder={isSet ? "set" : "not set"}
            value={secrets[f.key] ?? ""}
            onChange={(e) =>
              setSecrets((s) => ({ ...s, [f.key]: e.target.value }))
            }
            aria-invalid={!!errors[f.key] || undefined}
            disabled={pending}
          />
          {errors[f.key] ? (
            <FieldError>{errors[f.key]}</FieldError>
          ) : (
            f.description && (
              <FieldDescription>{f.description}</FieldDescription>
            )
          )}
        </Field>
      )
    }
    return (
      <Field key={f.key}>
        <FieldLabel htmlFor={id}>{f.label}</FieldLabel>
        <Input
          id={id}
          value={String(draft[f.key] ?? "")}
          placeholder={f.placeholder}
          autoComplete="off"
          autoCapitalize="none"
          spellCheck={false}
          onChange={(e) => setDraft((d) => ({ ...d, [f.key]: e.target.value }))}
          disabled={pending}
        />
        {f.description && <FieldDescription>{f.description}</FieldDescription>}
        {unknownToServer && (
          <FieldDescription>Not present in camera.conf yet.</FieldDescription>
        )}
        {f.key === "wifi_ssid" && ssidChanged && (
          <Alert className="border-amber-600/40 text-amber-800 dark:text-amber-300">
            <TriangleAlertIcon />
            <AlertTitle>The camera may move networks</AlertTitle>
            <AlertDescription>
              wifi switches to this network within ~10 s. If it does not
              come up (wrong password, out of range) it rolls back to the last
              working network and retries this one after 30 minutes. This page
              may lose the connection meanwhile.
            </AlertDescription>
          </Alert>
        )}
      </Field>
    )
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>camera.conf</CardTitle>
        <CardDescription>
          Only changed fields are written; comments and other lines are kept.
        </CardDescription>
      </CardHeader>
      <form onSubmit={onSubmit} aria-busy={pending}>
        <CardContent>
          <FieldGroup>
            {SECTIONS.map((s, i) => (
              <React.Fragment key={s.title}>
                {i > 0 && <FieldSeparator />}
                <FieldSet>
                  <FieldLegend>{s.title}</FieldLegend>
                  <FieldGroup>{s.fields.map(renderField)}</FieldGroup>
                </FieldSet>
              </React.Fragment>
            ))}
          </FieldGroup>
          {result && (
            <Alert className="mt-6">
              <CheckCircle2Icon />
              <AlertTitle>
                {result.changes.length
                  ? `Saved ${result.changes.length} change${result.changes.length === 1 ? "" : "s"}`
                  : "Nothing changed on the camera"}
              </AlertTitle>
              <AlertDescription>
                <ul className="list-disc pl-4">
                  {result.changes.map((c) => (
                    <li key={c.key}>
                      <code>{c.key}</code>:{" "}
                      {c.secret ? "updated" : `${c.from} → ${c.to}`}
                    </li>
                  ))}
                </ul>
                {result.restart.length > 0 && (
                  <p>Restarted: {result.restart.join(", ")}</p>
                )}
              </AlertDescription>
            </Alert>
          )}
        </CardContent>
        <CardFooter className="mt-6 flex-wrap justify-end gap-2">
          <span className="mr-auto text-sm text-muted-foreground" role="status">
            {changes.length
              ? `${changes.length} unsaved change${changes.length === 1 ? "" : "s"}`
              : "No changes"}
          </span>
          <Button
            type="submit"
            disabled={pending || !changes.length || invalid}
          >
            {pending && <Loader2Icon className="animate-spin" />}
            Save
          </Button>
        </CardFooter>
      </form>
    </Card>
  )
}
