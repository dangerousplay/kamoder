import Link from 'next/link';
import {
  ArrowRight,
  Cctv,
  Cpu,
  Download,
  Lock,
  Network,
  ShieldCheck,
  Wifi,
  type LucideIcon,
} from 'lucide-react';
import { repoUrl } from '@/lib/shared';

const features: { icon: LucideIcon; title: string; body: string }[] = [
  {
    icon: Lock,
    title: 'No cloud, no vendor app',
    body: 'The vendor app never starts. Nothing leaves your network unless you set it up.',
  },
  {
    icon: Network,
    title: 'Standard protocols',
    body: 'ONVIF Profile S, RTSP with Digest auth, SSH and a web admin UI with a JSON API.',
  },
  {
    icon: Wifi,
    title: 'Self-healing Wi-Fi',
    body: 'Rollback to the last good network, and a ladder up to a USB power cycle of the radio.',
  },
  {
    icon: Cpu,
    title: 'Built for 32 MB',
    body: 'Small Rust services under one supervisor, cross-compiled for the ARM926EJ-S and uClibc.',
  },
  {
    icon: ShieldCheck,
    title: 'Signed releases',
    body: 'Built in GitHub Actions, signed keylessly with Sigstore, with build provenance.',
  },
  {
    icon: Cctv,
    title: 'Runs from the SD card',
    body: 'Remove the card and nothing on the camera has changed, unless you choose to autostart.',
  },
];

export default function HomePage() {
  return (
    <main className="flex flex-1 flex-col">
      <section className="mx-auto flex w-full max-w-5xl flex-col items-center gap-6 px-6 pt-20 pb-12 text-center">
        <span className="inline-flex items-center gap-2 rounded-full border px-3 py-1 text-xs text-fd-muted-foreground">
          <Cctv className="size-3.5" aria-hidden /> Anyka AK3918 · ARMv5 · Rust
        </span>
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl">
          Open firmware for cheap IP cameras
        </h1>
        <p className="max-w-2xl text-lg text-fd-muted-foreground">
          Kamoder replaces the vendor app on Anyka AK3918 cameras with a small, auditable
          stack written in Rust, running from the SD card.
        </p>
        <div className="flex flex-wrap justify-center gap-3">
          <Link
            href="/docs/getting-started"
            className="inline-flex items-center gap-2 rounded-lg bg-fd-primary px-5 py-2.5 text-sm font-medium text-fd-primary-foreground"
          >
            Get started <ArrowRight className="size-4" aria-hidden />
          </Link>
          <Link
            href="/docs/cameras"
            className="inline-flex items-center gap-2 rounded-lg border px-5 py-2.5 text-sm font-medium"
          >
            <Cctv className="size-4" aria-hidden /> Supported cameras
          </Link>
          <a
            href={`${repoUrl}/releases`}
            className="inline-flex items-center gap-2 rounded-lg border px-5 py-2.5 text-sm font-medium"
          >
            <Download className="size-4" aria-hidden /> Releases
          </a>
        </div>
      </section>

      <section className="mx-auto grid w-full max-w-5xl gap-4 px-6 pb-20 sm:grid-cols-2 lg:grid-cols-3">
        {features.map(({ icon: Icon, title, body }) => (
          <div key={title} className="rounded-xl border bg-fd-card p-5 text-left">
            <Icon className="mb-3 size-6 text-fd-primary" aria-hidden />
            <h2 className="mb-1 font-semibold">{title}</h2>
            <p className="text-sm text-fd-muted-foreground">{body}</p>
          </div>
        ))}
      </section>
    </main>
  );
}
