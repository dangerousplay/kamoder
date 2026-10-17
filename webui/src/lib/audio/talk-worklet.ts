/**
 * AudioWorklet: microphone → 8 kHz mono S16LE frames for `/api/audio/talk`.
 *
 * Runs at the context rate (44.1/48 kHz usually). Each input block is
 * downmixed to mono, low-passed by a 4th-order Butterworth at 3.4 kHz (two
 * biquads) so nothing above the 4 kHz Nyquist of the target aliases, then
 * resampled with a fractional step and linear interpolation. Every 160
 * samples (20 ms) the frame goes to the main thread as a transferred
 * ArrayBuffer together with its RMS level, `{ pcm, level }`.
 *
 * The main thread posts `{ active: boolean }`; nothing is sent while
 * inactive and a partial frame is dropped on deactivation.
 *
 * Loaded with `audioWorklet.addModule(url)` where `url` comes from Vite's
 * `?worker&url` import, so the file is bundled as its own chunk.
 */

// AudioWorkletGlobalScope (not in lib.dom).
declare const sampleRate: number
declare class AudioWorkletProcessor {
  readonly port: MessagePort
}
declare function registerProcessor(
  name: string,
  ctor: new () => AudioWorkletProcessor
): void

const OUT_RATE = 8000
const FRAME_SAMPLES = 160
const CUTOFF_HZ = 3400

/** RBJ low-pass biquad, transposed direct form II. */
class LowPass {
  private b0: number
  private b1: number
  private b2: number
  private a1: number
  private a2: number
  private z1 = 0
  private z2 = 0

  constructor(fc: number, q: number, fs: number) {
    const w0 = (2 * Math.PI * fc) / fs
    const cos = Math.cos(w0)
    const alpha = Math.sin(w0) / (2 * q)
    const a0 = 1 + alpha
    this.b0 = (1 - cos) / 2 / a0
    this.b1 = (1 - cos) / a0
    this.b2 = this.b0
    this.a1 = (-2 * cos) / a0
    this.a2 = (1 - alpha) / a0
  }

  run(x: number): number {
    const y = this.b0 * x + this.z1
    this.z1 = this.b1 * x - this.a1 * y + this.z2
    this.z2 = this.b2 * x - this.a2 * y
    return y
  }
}

class TalkProcessor extends AudioWorkletProcessor {
  private active = false
  /** Input samples per output sample. */
  private readonly step = sampleRate / OUT_RATE
  private readonly filters =
    sampleRate > OUT_RATE
      ? // Butterworth 4th order = Q 0.5412 and 1.3066 sections.
        [
          new LowPass(CUTOFF_HZ, 0.5412, sampleRate),
          new LowPass(CUTOFF_HZ, 1.3066, sampleRate),
        ]
      : []
  /** Position of the next output sample, in input samples after `prev`. */
  private next = 1
  private prev = 0
  private frame = new Int16Array(FRAME_SAMPLES)
  private fill = 0
  private energy = 0

  constructor() {
    super()
    this.port.onmessage = (e: MessageEvent<{ active?: boolean }>) => {
      this.active = !!e.data.active
      if (!this.active) {
        this.fill = 0
        this.energy = 0
      }
    }
  }

  process(inputs: Float32Array[][]): boolean {
    const channels = inputs[0]
    if (!this.active || !channels || channels.length === 0) return true
    const n = channels[0].length
    const scale = 1 / channels.length
    for (let i = 0; i < n; i++) {
      let x = 0
      for (const ch of channels) x += ch[i]
      x *= scale
      for (const f of this.filters) x = f.run(x)
      // Emit every output instant that falls in (prev, x].
      while (this.next <= 1) {
        this.emit(this.prev + (x - this.prev) * this.next)
        this.next += this.step
      }
      this.next -= 1
      this.prev = x
    }
    return true
  }

  private emit(y: number) {
    const s = Math.max(-32768, Math.min(32767, Math.round(y * 32767)))
    this.frame[this.fill++] = s
    this.energy += y * y
    if (this.fill < FRAME_SAMPLES) return
    const level = Math.sqrt(this.energy / FRAME_SAMPLES)
    const pcm = this.frame.buffer
    this.port.postMessage({ pcm, level }, [pcm])
    this.frame = new Int16Array(FRAME_SAMPLES)
    this.fill = 0
    this.energy = 0
  }
}

registerProcessor("kamoder-talk", TalkProcessor)
