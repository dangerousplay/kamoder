/**
 * Push-to-talk: browser microphone → `/api/audio/talk` (8 kHz mono S16LE,
 * 320-byte / 20 ms binary messages; see crates/web/src/audio.rs).
 *
 * One `Talker` lives as long as the audio card. The microphone stream, the
 * AudioContext and the `talk-worklet.ts` node are created on the first press
 * and kept (tracks disabled, context suspended between presses) so the
 * browser does not ask for permission again; `dispose()` releases them.
 * Every press opens its own WebSocket; release sends "end" and closes it.
 * Needs a secure context (getUserMedia, AudioWorklet).
 *
 * The input is the microphone picked with `setDevice` (saved per browser,
 * see mics.ts), else the system default — which on Linux desktops is often
 * a monitor or virtual source that captures silence. `startTest` runs the
 * capture without a socket so the level meter shows what would be sent.
 */

import { api, errorMessage } from "@/lib/api"
import { closeMessage, endSocket, openAudioSocket } from "@/lib/audio/socket"
import { saveMic, savedMic } from "@/lib/audio/mics"
import talkWorkletUrl from "@/lib/audio/talk-worklet.ts?worker&url"

/** Frames kept while the socket connects (1 s), so first words survive. */
const MAX_QUEUED = 50

export type TalkState = "idle" | "starting" | "talking"

export interface TalkEvents {
  onState: (state: TalkState) => void
  /** RMS level of the last 20 ms frame, 0..1 (0 when not talking). */
  onLevel: (level: number) => void
  /** Why talk stopped or could not start. */
  onError: (message: string) => void
  /** The microphone was opened: device labels are now readable. */
  onMicOpened?: () => void
  /** The microphone test started or stopped. */
  onTesting?: (testing: boolean) => void
}

interface FrameMessage {
  pcm: ArrayBuffer
  level: number
}

/** User-facing reason for a getUserMedia / AudioWorklet failure. */
function micError(e: unknown): string {
  if (e instanceof DOMException) {
    switch (e.name) {
      case "NotAllowedError":
      case "SecurityError":
        return "Microphone permission denied: allow it in the browser's site settings and try again"
      case "NotFoundError":
      case "OverconstrainedError":
        return "No microphone found"
      case "NotReadableError":
        return "The microphone is in use by another application"
    }
  }
  return `Microphone: ${errorMessage(e)}`
}

export class Talker {
  private readonly events: TalkEvents
  private ctx: AudioContext | null = null
  private stream: MediaStream | null = null
  private node: AudioWorkletNode | null = null
  private setup: Promise<void> | null = null
  private ws: WebSocket | null = null
  private queue: ArrayBuffer[] = []
  private pressed = false
  private testing = false
  private deviceId: string | null = savedMic()
  /** Bumped on every press/stop; stale async steps check it and bail. */
  private session = 0

  constructor(events: TalkEvents) {
    this.events = events
  }

  /** Whether this page can capture the microphone at all. */
  static supported(): boolean {
    return (
      window.isSecureContext &&
      !!navigator.mediaDevices?.getUserMedia &&
      typeof AudioWorkletNode !== "undefined"
    )
  }

  /** Button down. Call from the event handler (user gesture). */
  press() {
    if (this.pressed) return
    this.pressed = true
    const id = ++this.session
    this.events.onState("starting")
    try {
      this.ctx ??= new AudioContext()
      void this.ctx.resume()
    } catch (e) {
      this.fail(id, micError(e))
      return
    }
    void this.begin(id)
  }

  /** The chosen microphone id, or null for the system default. */
  device(): string | null {
    return this.deviceId
  }

  /**
   * Switches the input (null = system default) and remembers it. An open
   * microphone is closed; a running test or talk reopens the new one.
   */
  setDevice(deviceId: string | null) {
    if (deviceId === this.deviceId) return
    this.deviceId = deviceId
    saveMic(deviceId)
    this.closeMic()
    if (this.testing || this.pressed) {
      const id = this.session
      void this.ensureMic()
        .then(() => {
          if (id !== this.session && !this.testing) return
          this.capture(true)
        })
        .catch((e) => this.events.onError(micError(e)))
    }
  }

  /** Mic test on: capture and report the level, send nothing. Gesture. */
  startTest() {
    if (this.testing) return
    this.testing = true
    this.events.onTesting?.(true)
    try {
      this.ctx ??= new AudioContext()
      void this.ctx.resume()
    } catch (e) {
      this.stopTest()
      this.events.onError(micError(e))
      return
    }
    void this.ensureMic()
      .then(() => {
        if (this.testing) this.capture(true)
      })
      .catch((e) => {
        this.stopTest()
        this.events.onError(micError(e))
      })
  }

  stopTest() {
    if (!this.testing) return
    this.testing = false
    this.events.onTesting?.(false)
    if (!this.pressed) {
      this.capture(false)
      this.events.onLevel(0)
    }
  }

  /** Button up (or pointer left, tab hidden): stop talking. */
  release() {
    if (!this.pressed) return
    this.session++
    this.stop()
  }

  /** Stops talking and frees the microphone and the audio context. */
  dispose() {
    this.release()
    this.stopTest()
    this.session++
    this.closeMic()
    void this.ctx?.close().catch(() => {})
    this.ctx = null
  }

  private closeMic() {
    this.stream?.getTracks().forEach((t) => t.stop())
    this.stream = null
    this.node?.disconnect()
    this.node = null
    this.setup = null
  }

  /** Starts or pauses the capture path (tracks, worklet, context). */
  private capture(on: boolean) {
    this.stream?.getAudioTracks().forEach((t) => (t.enabled = on))
    this.node?.port.postMessage({ active: on })
    if (on) void this.ctx?.resume()
    else void this.ctx?.suspend().catch(() => {})
  }

  private async begin(id: number) {
    const token = api.logStreamToken()
    token.catch(() => {}) // awaited below; avoid an unhandled rejection
    try {
      await this.ensureMic()
    } catch (e) {
      this.fail(id, micError(e))
      return
    }
    if (id !== this.session) return
    // Capture right away; frames wait in the queue until the socket opens.
    this.queue = []
    this.capture(true)

    let ws: WebSocket
    try {
      ws = await openAudioSocket("talk", token)
    } catch (e) {
      this.fail(id, `Talk: ${errorMessage(e)}`)
      return
    }
    if (id !== this.session) {
      endSocket(ws)
      return
    }
    this.ws = ws
    let opened = false
    ws.onopen = () => {
      opened = true
      for (const pcm of this.queue) ws.send(pcm)
      this.queue = []
      if (id === this.session) this.events.onState("talking")
    }
    ws.onclose = (e) => {
      if (id !== this.session) return
      this.ws = null
      this.fail(id, closeMessage(e.code, opened))
    }
  }

  private async ensureMic() {
    const live = this.stream
      ?.getAudioTracks()
      .some((t) => t.readyState === "live")
    if (this.setup && live !== false) return this.setup
    if (this.stream) {
      // The microphone went away (unplugged, revoked): start over.
      this.closeMic()
    }
    const ctx = this.ctx
    if (!ctx) throw new Error("no audio context")
    this.setup = (async () => {
      if (!Talker.supported()) {
        throw new Error("this browser cannot capture audio on this page")
      }
      const constraints = (deviceId: string | null) => ({
        audio: {
          deviceId: deviceId ? { exact: deviceId } : undefined,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
          channelCount: 1,
        },
      })
      let stream: MediaStream
      try {
        stream = await navigator.mediaDevices.getUserMedia(
          constraints(this.deviceId)
        )
      } catch (e) {
        // The saved microphone is gone: fall back to the default.
        if (
          !this.deviceId ||
          !(e instanceof DOMException) ||
          (e.name !== "OverconstrainedError" && e.name !== "NotFoundError")
        )
          throw e
        this.deviceId = null
        saveMic(null)
        stream = await navigator.mediaDevices.getUserMedia(constraints(null))
      }
      try {
        await ctx.audioWorklet.addModule(talkWorkletUrl)
        const source = ctx.createMediaStreamSource(stream)
        const node = new AudioWorkletNode(ctx, "kamoder-talk", {
          numberOfInputs: 1,
          numberOfOutputs: 1,
          outputChannelCount: [1],
        })
        node.port.onmessage = (e: MessageEvent<FrameMessage>) =>
          this.frame(e.data)
        // A silent path to the destination keeps the node rendering.
        const mute = ctx.createGain()
        mute.gain.value = 0
        source.connect(node).connect(mute).connect(ctx.destination)
        this.stream = stream
        this.node = node
        this.events.onMicOpened?.()
      } catch (e) {
        stream.getTracks().forEach((t) => t.stop())
        throw e
      }
    })()
    try {
      await this.setup
    } catch (e) {
      this.setup = null
      throw e
    }
  }

  private frame({ pcm, level }: FrameMessage) {
    if (!this.pressed && !this.testing) return
    this.events.onLevel(level)
    if (!this.pressed) return
    const ws = this.ws
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(pcm)
    } else if (this.queue.length < MAX_QUEUED) {
      this.queue.push(pcm)
    }
  }

  private fail(id: number, message: string | null) {
    if (id !== this.session) return
    this.session++
    this.stop()
    if (message) this.events.onError(message)
  }

  /** Back to idle: capture paused, socket ended. */
  private stop() {
    this.pressed = false
    // A running mic test keeps the capture (and the meter) going.
    if (!this.testing) {
      this.capture(false)
      this.events.onLevel(0)
    }
    endSocket(this.ws)
    this.ws = null
    this.queue = []
    this.events.onState("idle")
  }
}
