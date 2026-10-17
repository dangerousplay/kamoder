/**
 * Typed client for the `anyka-web` REST API (design.md, "anyka-web API").
 *
 * - Base path `/api`; bearer token read from sessionStorage (`anyka.token`).
 * - Non-2xx responses throw `ApiError` (status + server `{error, code}` body).
 * - 401 on any authenticated call clears the token and emits
 *   `UNAUTHORIZED_EVENT`; the AuthProvider reacts and the router sends the
 *   user to /login (with a hard redirect as a fallback when nothing listens).
 */

export const API_BASE = "/api"
export const TOKEN_KEY = "anyka.token"
export const UNAUTHORIZED_EVENT = "anyka:unauthorized"

// ---------------------------------------------------------------- errors ----

/** JSON body the server returns on every error response. */
export interface ApiErrorBody {
  error: string
  code: string
}

export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, message: string, code = "unknown") {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.code = code
  }

  get isUnauthorized() {
    return this.status === 401
  }
  get isForbidden() {
    return this.status === 403
  }
  get isRateLimited() {
    return this.status === 429
  }
  /** 503: backend (video, PTZ, lights, follow, audio, supervisor) not available. */
  get isUnavailable() {
    return this.status === 503
  }
}

// ----------------------------------------------------------------- token ----

export function getToken(): string | null {
  return sessionStorage.getItem(TOKEN_KEY)
}

/** When the token stops being valid, in *browser* milliseconds; see auth.tsx. */
export const EXPIRES_KEY = "anyka.expires_at"

export function setToken(token: string, expiresAt?: number): void {
  sessionStorage.setItem(TOKEN_KEY, token)
  if (expiresAt !== undefined)
    sessionStorage.setItem(EXPIRES_KEY, String(expiresAt))
}

export function getTokenExpiry(): number | null {
  const v = Number(sessionStorage.getItem(EXPIRES_KEY))
  return Number.isFinite(v) && v > 0 ? v : null
}

export function clearToken(): void {
  sessionStorage.removeItem(TOKEN_KEY)
  sessionStorage.removeItem(EXPIRES_KEY)
}

function handleUnauthorized() {
  clearToken()
  const notified = !window.dispatchEvent(
    new Event(UNAUTHORIZED_EVENT, { cancelable: true })
  )
  // A listener (AuthProvider) calls preventDefault() and navigates itself.
  if (!notified && window.location.pathname !== "/login") {
    window.location.assign("/login")
  }
}

// ----------------------------------------------------------------- types ----

export type Role = "admin" | "viewer"

export interface LoginRequest {
  user: string
  password: string
}

export interface LoginResponse {
  token: string
  role: Role
  /** Expiry, seconds since the Unix epoch. */
  exp: number
}

export interface Me {
  user: string
  role: Role
  exp?: number
}

export type ServiceState =
  "running" | "starting" | "stopped" | "backoff" | "failed" | "disabled"

export interface ServiceStatus {
  name: string
  state: ServiceState
  pid?: number | null
  /** Seconds since the current instance started. */
  uptime_s?: number | null
  restarts: number
  enabled: boolean
  /** Critical services (e.g. wdt_feeder) cannot be stopped (409). */
  critical: boolean
  /** Log file name usable with the logs endpoints, if any. */
  log?: string | null
  last_exit?: string | null
}

export type ServiceAction = "start" | "stop" | "restart"

export interface LogFile {
  name: string
  size: number
  /** Last modification, seconds since the Unix epoch. */
  modified?: number
}

export interface LogTail {
  file: string
  lines: string[]
}

export interface StreamToken {
  token: string
  exp: number
}

export type ConfigValue = string | number | boolean

/** GET /api/config — secrets are never returned. */
export interface ConfigResponse {
  values: Record<string, ConfigValue>
  /** Secret keys (wifi_password, ssh_password, ...) that currently have a value. */
  secrets_set: string[]
}

/** PUT /api/config — only the keys to change; secrets are write-only. */
export interface ConfigUpdate {
  values: Record<string, ConfigValue>
}

export interface ConfigUpdateResult {
  /** Keys the server actually rewrote in camera.conf (CFG-02 "what changed"). */
  changed?: string[]
  /** Services the server restarted / that need a restart to apply. */
  restart?: string[]
}

export interface SystemStats {
  /** Seconds since boot; the time base for rates. */
  uptime: number
  /** Cumulative CPU jiffies: busy and total. */
  cpu_busy: number
  cpu_total: number
  load1: number
  mem_total_kb: number
  mem_available_kb: number
  mem_free_kb: number
  interfaces: { name: string; rx_bytes: number; tx_bytes: number }[]
}

export interface CameraStream {
  codec: string
  profile: string
  width: number
  height: number
  fps: number
  gop: number
  target_kbps: number
  /** Measured bitrate. */
  kbps: number
  encoder: string
}

export interface CameraLive {
  index: number
  /**
   * "running", "starting", "failed", "unavailable" (camera service has no
   * status for it) or "missing" (the board profile expects it, nothing found).
   */
  state: string
  width: number
  height: number
  format: string
  encoding: string
  frames: number
  fps: number
  sensor_width: number
  sensor_height: number
  sensor_id?: string | null
  tuning?: string | null
  /** Hardware H.264 stream (RTSP), when encoding. */
  stream?: CameraStream | null
  error?: string | null
}

export interface CameraDetails {
  index: number
  sensor: string
  bus?: string | null
  channels: number
  /** RTSP path of the stream, as the server names it. */
  rtsp_path: string
  /** Port the rtsp service listens on. */
  rtsp_port: number
  /** rtsp://<host>[:port]/<path> for the host this page was loaded from; null when unknown. */
  rtsp_url: string | null
  live: CameraLive
}

export interface FlashPartition {
  device: string
  name: string
  size_bytes: number
}

/** One image sensor (`v4l-subdevN`) and its capture channels. */
export interface Camera {
  index: number
  /** Sensor driver, e.g. "sc2331". */
  sensor: string
  /** I²C location, e.g. "0-0030". */
  bus?: string | null
  channels: number
}

/** The board profile in use and why it was chosen. */
export interface BoardInfo {
  name: string
  reason: string
  description?: string | null
}

export interface Hardware {
  soc?: string | null
  cpu?: string | null
  bogomips?: number | null
  /** Device-tree model string. */
  model?: string | null
  /** Board profile; null when it could not be resolved. */
  board?: BoardInfo | null
  kernel?: string | null
  memory_total_kb?: number | null
  memory_available_kb?: number | null
  uptime_s?: number | null
  flash: FlashPartition[]
  cameras: Camera[]
  camera_count: number
}

export interface WifiStatus {
  iface: string
  connected: boolean
  ssid?: string | null
  ip?: string | null
  gateway?: string | null
  signal_dbm?: number | null
  /** e.g. "healthy", "reconnecting", "rolled_back". */
  state?: string
  /** anyka-wifi health verdict (associated to target, addressed, gateway ok). */
  healthy?: boolean | null
  /** Network anyka-wifi is trying to reach (camera.conf `wifi_ssid`). */
  target_ssid?: string | null
  /** Last network observed working (rollback target). */
  last_good_ssid?: string | null
  /** Seconds until anyka-wifi rolls back / retries the held network. */
  rollback_in_s?: number | null
}

export interface PtzPreset {
  id: number
  name: string
  pan: number
  tilt: number
}

export interface PtzStatus {
  available: boolean
  /** Backend name: "preset_motor", "sdk" or "sim". */
  backend?: string
  pan: number
  tilt: number
  moving: boolean
  presets?: PtzPreset[]
  /** Travel range in the backend's units (degrees, or raw steps until calibrated). */
  limits?: PtzLimits
}

export interface PtzLimits {
  pan_min_deg: number
  pan_max_deg: number
  tilt_min_deg: number
  tilt_max_deg: number
}

/** Continuous move; velocities in [-1, 1]. */
export interface PtzMove {
  pan: number
  tilt: number
  speed?: number
}

/** Absolute move to a position or to a stored preset. */
export type PtzGoto = { pan: number; tilt: number } | { preset: number }

/** Day = IR-cut filter in, IR LEDs off; night = filter out, IR LEDs on. */
export type LightsMode = "day" | "night"

/** Red/blue lights; `period_ms` is one on/off cycle (200–10000, default 1000). */
export type LightsAlarm =
  { state: "off" } | { state: "on" } | { state: "blink"; period_ms?: number }

/** `GET /api/lights`: the lights owner's state. */
export interface LightsState {
  mode: LightsMode
  white: boolean
  alarm: LightsAlarm
  /** Speaker amplifier off. */
  muted: boolean
  /**
   * Roles this board has (board profile); controls of the others are hidden.
   * Absent from owners older than the field: read as every role.
   */
  supported?: LightRole[]
}

/** A lights line role of the board profile (`lights-proto` `Role`). */
export type LightRole = "ir_cut" | "ir_leds" | "white" | "alarm" | "amp"

/** `PUT /api/lights` (admin): any subset; answers the full state. */
export type LightsUpdate = Partial<Omit<LightsState, "supported">>

/** Direction of an audio device (`audio-proto` `DeviceKind`). */
export type AudioDeviceKind = "input" | "output"

/** `active`: capturing / playing; `error`: last open or ioctl failed. */
export type AudioDeviceState = "active" | "idle" | "error"

/** A microphone or speaker of the board (`audio-proto` `Device`). */
export interface AudioDevice {
  /** Stable id from the board profile (`mic`, `speaker`). */
  id: string
  kind: AudioDeviceKind
  name: string
  state: AudioDeviceState
  /** Rate on the socket, Hz (8000). */
  rate: number
  /** Gain (input) or volume (output), driver units; null without control. */
  gain: number | null
  /** Accepted gain values, inclusive. */
  gain_range: [number, number]
  /** Why the device is in `error`. */
  error: string | null
}

/** `GET /api/audio/devices`. */
export interface AudioDevices {
  devices: AudioDevice[]
  talk: {
    /** Port of the HTTPS server (push-to-talk needs it); null when down. */
    https_port: number | null
  }
}

/** What the follow-movement tracker is doing (`follow-proto` `State`). */
export type FollowState =
  | "idle"
  | "tracking"
  | "settling"
  | "returning"
  | "paused"
  | "disabled"
  | "unavailable"

/** The subject being followed; frame coordinates scaled to 0..1000. */
export interface FollowTarget {
  /** 0 = left edge, 1000 = right edge. */
  x_permille: number
  /** 0 = top edge, 1000 = bottom edge. */
  y_permille: number
  /** Motion-grid cells the subject covers. */
  cells: number
}

/** `GET /api/follow`: the follow service's status. */
export interface FollowStatus {
  /** The persisted on/off switch. */
  enabled: boolean
  state: FollowState
  target: FollowTarget | null
  /** PTZ moves commanded since the service started. */
  moves: number
}

export interface User {
  name: string
  role: Role
}

export interface CreateUser {
  name: string
  password: string
  role: Role
}

export interface UpdateUser {
  role?: Role
  password?: string
}

export interface ChangePassword {
  current: string
  new: string
}

export interface SetupStatus {
  /** The first-run wizard was finished (or skipped) by an admin. */
  completed: boolean
  onvif_configured: boolean
  onvif_user?: string | null
  ssh_password_set: boolean
  ssh_keys: number
}

export interface SshKey {
  algorithm: string
  comment: string
  /** `SHA256:…`, as `ssh-keygen -l` prints it. */
  fingerprint: string
}

export interface SshStatus {
  user: string
  password_set: boolean
  keys: SshKey[]
}

export interface OnvifCredentials {
  user: string
  password: string
}

// ---------------------------------------------------------------- update ----
// `GET /api/update` passes the update service's `Status` through as-is.

export type UpdateSlotName = "a" | "b"

export type UpdatePhase =
  | "idle"
  | "checking"
  | "downloading"
  | "verifying"
  | "installing"
  | "awaiting_reboot"
  | "trial"
  | "error"

export type UpdateChannel = "stable" | "beta"

export interface UpdateSlot {
  slot: UpdateSlotName
  version: string | null
  bootable: boolean
  successful: boolean
  tries_left: number
  dir_present: boolean
}

export interface UpdateProgress {
  bytes_done: number
  bytes_total: number | null
  /** What the bytes are (e.g. "download", "verify", "install"). */
  what: string
}

export interface UpdateAvailable {
  version: string
  channel: UpdateChannel
  seq: number
  notes_url: string | null
  size: number | null
  downloaded: boolean
  downgrade: boolean
}

export interface UpdateError {
  code: string
  message: string
  /** Unix seconds. */
  at: number | null
}

/** Maintenance window for auto-install, local hours 0–23. */
export interface UpdateWindow {
  start_hour: number
  end_hour: number
}

export interface UpdateSettings {
  channel: UpdateChannel
  auto_check: boolean
  auto_install: boolean
  window: UpdateWindow | null
  repo: string
}

/** Trial boot health (OTA-11): commit after `ok_for_s` reaches 180 s. */
export interface UpdateHealth {
  ok_for_s: number
  waiting_on: string[]
  /** Seconds left before the guard reboots into the previous slot. */
  deadline_s: number
}

export interface UpdateStatus {
  version: string
  slot: UpdateSlotName
  trial: boolean
  committed: boolean
  slots: UpdateSlot[]
  phase: UpdatePhase
  progress: UpdateProgress | null
  available: UpdateAvailable | null
  /** Unix seconds. */
  last_check: number | null
  last_error: UpdateError | null
  settings: UpdateSettings
  /** False when the firmware has no built-in signing key. */
  trusted_key: boolean
  health: UpdateHealth | null
}

export interface UpdateInstallOptions {
  /** Reinstall even if the version is not newer. */
  force: boolean
  /** Reboot into the new slot right away. */
  reboot: boolean
}

export interface Ok {
  ok: boolean
}

// ---------------------------------------------------------------- client ----

type Method = "GET" | "POST" | "PUT" | "DELETE"

interface RequestOptions {
  body?: unknown
  signal?: AbortSignal
  /** Skip the global 401 handling (login itself). */
  anonymous?: boolean
}

async function toApiError(res: Response): Promise<ApiError> {
  let message = res.statusText || `HTTP ${res.status}`
  let code = "http_" + res.status
  try {
    const body = (await res.json()) as Partial<ApiErrorBody>
    if (body.error) message = body.error
    if (body.code) code = body.code
  } catch {
    // Non-JSON error body (e.g. proxy error page): keep the status text.
  }
  return new ApiError(res.status, message, code)
}

async function send(
  method: Method,
  path: string,
  opts: RequestOptions = {}
): Promise<Response> {
  const headers: Record<string, string> = { Accept: "application/json" }
  const token = opts.anonymous ? null : getToken()
  if (token) headers.Authorization = `Bearer ${token}`
  if (opts.body !== undefined) headers["Content-Type"] = "application/json"

  let res: Response
  try {
    res = await fetch(API_BASE + path, {
      method,
      headers,
      body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
      signal: opts.signal,
    })
  } catch (e) {
    if (e instanceof DOMException && e.name === "AbortError") throw e
    throw new ApiError(0, "Camera unreachable", "network")
  }

  if (!res.ok) {
    if (res.status === 401 && !opts.anonymous) handleUnauthorized()
    throw await toApiError(res)
  }
  return res
}

async function request<T>(
  method: Method,
  path: string,
  opts?: RequestOptions
): Promise<T> {
  const res = await send(method, path, opts)
  if (res.status === 204) return undefined as T
  const text = await res.text()
  return (text ? JSON.parse(text) : undefined) as T
}

const enc = encodeURIComponent

/**
 * POSTs a raw file body with upload progress (fetch has none). Same auth and
 * error handling as `send`.
 */
function uploadRaw<T>(
  path: string,
  file: Blob,
  onProgress?: (done: number, total: number) => void,
  signal?: AbortSignal
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const xhr = new XMLHttpRequest()
    xhr.open("POST", API_BASE + path)
    xhr.setRequestHeader("Accept", "application/json")
    xhr.setRequestHeader("Content-Type", "application/octet-stream")
    const token = getToken()
    if (token) xhr.setRequestHeader("Authorization", `Bearer ${token}`)
    xhr.upload.onprogress = (e) =>
      onProgress?.(e.loaded, e.lengthComputable ? e.total : file.size)
    xhr.onerror = () => reject(new ApiError(0, "Camera unreachable", "network"))
    xhr.onabort = () => reject(new DOMException("Aborted", "AbortError"))
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(
            (xhr.responseText ? JSON.parse(xhr.responseText) : undefined) as T
          )
        } catch {
          resolve(undefined as T)
        }
        return
      }
      if (xhr.status === 401) handleUnauthorized()
      let message = xhr.statusText || `HTTP ${xhr.status}`
      let code = "http_" + xhr.status
      try {
        const body = JSON.parse(xhr.responseText) as Partial<ApiErrorBody>
        if (body.error) message = body.error
        if (body.code) code = body.code
      } catch {
        // Non-JSON error body: keep the status text.
      }
      reject(new ApiError(xhr.status, message, code))
    }
    signal?.addEventListener("abort", () => xhr.abort(), { once: true })
    xhr.send(file)
  })
}

// ------------------------------------------------------------- endpoints ----

export const api = {
  // auth
  login: (body: LoginRequest) =>
    request<LoginResponse>("POST", "/auth/login", { body, anonymous: true }),
  me: (signal?: AbortSignal) => request<Me>("GET", "/me", { signal }),

  // services
  services: (signal?: AbortSignal) =>
    request<ServiceStatus[]>("GET", "/services", { signal }),
  serviceAction: (name: string, action: ServiceAction) =>
    request<ServiceStatus | undefined>(
      "POST",
      `/services/${enc(name)}/${action}`
    ),

  // logs
  logs: (signal?: AbortSignal) =>
    request<LogFile[]>("GET", "/logs", { signal }),
  logTail: (file: string, lines = 500, signal?: AbortSignal) =>
    request<LogTail>("GET", `/logs/${enc(file)}?lines=${lines}`, { signal }),
  logStreamToken: () => request<StreamToken>("POST", "/logs/stream-token"),

  // config
  config: (signal?: AbortSignal) =>
    request<ConfigResponse>("GET", "/config", { signal }),
  updateConfig: (body: ConfigUpdate) =>
    request<ConfigUpdateResult>("PUT", "/config", { body }),

  // system
  wifi: (signal?: AbortSignal) =>
    request<WifiStatus>("GET", "/wifi", { signal }),
  reboot: () => request<void>("POST", "/system/reboot"),
  stats: (signal?: AbortSignal) =>
    request<SystemStats>("GET", "/system/stats", { signal }),
  cameras: (signal?: AbortSignal) =>
    request<CameraDetails[]>("GET", "/cameras", { signal }),
  hardware: (signal?: AbortSignal) =>
    request<Hardware>("GET", "/hardware", { signal }),
  /** Unauthenticated liveness probe (used to detect the end of a reboot). */
  health: (signal?: AbortSignal) =>
    request<unknown>("GET", "/health", { signal, anonymous: true }),

  // video
  /** Fetches one JPEG with the bearer header; 503 when video is down. */
  snapshot: async (signal?: AbortSignal, camera?: number): Promise<Blob> =>
    (
      await send(
        "GET",
        camera == null ? "/snapshot.jpg" : `/snapshot.jpg?camera=${camera}`,
        { signal }
      )
    ).blob(),

  // ptz
  ptz: (signal?: AbortSignal) => request<PtzStatus>("GET", "/ptz", { signal }),
  ptzMove: (body: PtzMove) => request<void>("POST", "/ptz/move", { body }),
  ptzStop: () => request<void>("POST", "/ptz/stop"),
  ptzHome: () => request<void>("POST", "/ptz/home"),
  ptzGoto: (body: PtzGoto) => request<void>("POST", "/ptz/goto", { body }),
  ptzPresets: (signal?: AbortSignal) =>
    request<PtzPreset[]>("GET", "/ptz/presets", { signal }),
  /** Stores the current position as a preset. */
  ptzSavePreset: (name: string) =>
    request<PtzPreset>("POST", "/ptz/presets", { body: { name } }),
  ptzDeletePreset: (id: number) =>
    request<void>("DELETE", `/ptz/presets/${id}`),

  // lights
  lights: (signal?: AbortSignal) =>
    request<LightsState>("GET", "/lights", { signal }),
  updateLights: (body: LightsUpdate) =>
    request<LightsState>("PUT", "/lights", { body }),

  // audio
  audioDevices: (signal?: AbortSignal) =>
    request<AudioDevices>("GET", "/audio/devices", { signal }),
  /** Sets mic gain / speaker volume (admin); answers the device after it. */
  updateAudioDevice: (id: string, gain: number) =>
    request<AudioDevice>("PUT", `/audio/devices/${enc(id)}`, {
      body: { gain },
    }),

  // follow movement
  follow: (signal?: AbortSignal) =>
    request<FollowStatus>("GET", "/follow", { signal }),
  /** Switches following on or off (admin); answers the fresh status. */
  updateFollow: (enabled: boolean) =>
    request<FollowStatus>("PUT", "/follow", { body: { enabled } }),

  // users
  users: (signal?: AbortSignal) => request<User[]>("GET", "/users", { signal }),
  createUser: (body: CreateUser) => request<User>("POST", "/users", { body }),
  updateUser: (name: string, body: UpdateUser) =>
    request<User>("PUT", `/users/${enc(name)}`, { body }),
  deleteUser: (name: string) => request<void>("DELETE", `/users/${enc(name)}`),
  /**
   * Changing the password bumps the user's generation, which invalidates the
   * current token; the server answers with a fresh session to adopt.
   */
  changeOwnPassword: (body: ChangePassword) =>
    request<LoginResponse>("PUT", "/users/me/password", { body }),

  // onvif
  setOnvifCredentials: (body: OnvifCredentials) =>
    request<void>("PUT", "/onvif/credentials", { body }),

  // first-run setup
  setup: (signal?: AbortSignal) =>
    request<SetupStatus>("GET", "/setup", { signal }),
  completeSetup: () => request<SetupStatus>("POST", "/setup"),
  reopenSetup: () => request<SetupStatus>("DELETE", "/setup"),

  // ssh
  ssh: (signal?: AbortSignal) => request<SshStatus>("GET", "/ssh", { signal }),
  setSshPassword: (password: string) =>
    request<SshStatus>("PUT", "/ssh/password", { body: { password } }),
  addSshKey: (key: string) =>
    request<SshStatus>("POST", "/ssh/keys", { body: { key } }),
  removeSshKey: (fingerprint: string) =>
    request<SshStatus>("DELETE", `/ssh/keys?fingerprint=${enc(fingerprint)}`),

  // firmware updates (503 `update_unavailable` when the service is down)
  updateStatus: (signal?: AbortSignal) =>
    request<UpdateStatus>("GET", "/update", { signal }),
  /** Asks the repo for a newer release; answers after the check (~30 s). */
  updateCheck: () => request<UpdateStatus>("POST", "/update/check"),
  updateDownload: () => request<Ok>("POST", "/update/download"),
  /** Installs the downloaded package into the inactive slot. */
  updateInstall: (body: UpdateInstallOptions) =>
    request<Ok>("POST", "/update/install", { body }),
  /** Streams a `.kup` (raw body, 64 MiB cap) and installs it. */
  updateUpload: (
    file: File,
    opts: UpdateInstallOptions,
    onProgress?: (done: number, total: number) => void,
    signal?: AbortSignal
  ) =>
    uploadRaw<Ok>(
      `/update/upload?force=${opts.force}&reboot=${opts.reboot}`,
      file,
      onProgress,
      signal
    ),
  updateRollback: (body: { reboot: boolean }) =>
    request<Ok>("POST", "/update/rollback", { body }),
  updateReboot: () => request<Ok>("POST", "/update/reboot"),
  updateSettings: (body: UpdateSettings) =>
    request<UpdateStatus>("PUT", "/update/settings", { body }),
}

// ------------------------------------------------------------ URL helpers ----
// <img>/<EventSource> cannot send headers: these URLs carry a short-lived
// stream token from `api.logStreamToken()` as `?token=`.

export function logStreamUrl(file: string, streamToken: string): string {
  return `${API_BASE}/logs/${enc(file)}/stream?token=${enc(streamToken)}`
}

export function snapshotUrl(streamToken: string, bust = Date.now()): string {
  return `${API_BASE}/snapshot.jpg?token=${enc(streamToken)}&t=${bust}`
}

export function liveMjpegUrl(streamToken: string, camera?: number): string {
  const cam = camera == null ? "" : `&camera=${camera}`
  return `${API_BASE}/live.mjpeg?token=${enc(streamToken)}${cam}`
}

/**
 * WebSocket URL of `/api/audio/talk` or `/api/audio/listen` on the host this
 * page came from (`wss:` when the page is HTTPS).
 */
export function audioSocketUrl(
  kind: "talk" | "listen",
  streamToken: string
): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:"
  return `${proto}//${window.location.host}${API_BASE}/audio/${kind}?token=${enc(streamToken)}`
}

/**
 * RTSP URL of a camera: the server's `rtsp_url`, else built from the host this
 * page was loaded from and the served port and path.
 */
export function cameraRtspUrl(
  cam: Pick<CameraDetails, "rtsp_url" | "rtsp_port" | "rtsp_path">
): string {
  return (
    cam.rtsp_url ??
    `rtsp://${window.location.hostname}:${cam.rtsp_port}/${cam.rtsp_path}`
  )
}

/** Human-readable message for toasts. */
export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.isRateLimited) return "Too many attempts, try again later"
    return e.message
  }
  if (e instanceof Error) return e.message
  return String(e)
}
