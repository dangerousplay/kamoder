import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';
import { Cctv, Download } from 'lucide-react';
import { appName, repoUrl } from './shared';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <Cctv className="size-5" aria-hidden />
          <span className="font-semibold">{appName}</span>
        </>
      ),
    },
    links: [
      {
        icon: <Download />,
        text: 'Releases',
        url: `${repoUrl}/releases`,
        external: true,
      },
    ],
    githubUrl: repoUrl,
  };
}
