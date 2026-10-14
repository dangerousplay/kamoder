import { createGetUrl } from 'fumadocs-core/source';

export const appName = 'Kamoder';
export const docsRoute = '/docs';
export const docsImageRoute = '/og/docs';
export const docsContentRoute = '/llms.mdx/docs';

/** Prefix for client-side fetches; Next adds it to links but not to fetch(). */
export const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

const [user, repo] = (process.env.NEXT_PUBLIC_GITHUB_REPOSITORY ?? 'OWNER/kamoder').split('/');

export const gitConfig = {
  user,
  repo,
  branch: process.env.NEXT_PUBLIC_GITHUB_BRANCH ?? 'main',
};

export const repoUrl = `https://github.com/${gitConfig.user}/${gitConfig.repo}`;

const getContentUrl = createGetUrl(docsContentRoute);

export function getPageMarkdownUrl(page: { slugs: string[]; locale?: string }) {
  const segments = [...page.slugs, 'content.md'];

  return { segments, url: getContentUrl(segments, page.locale) };
}

const getImageUrl = createGetUrl(docsImageRoute);

export function getPageImageUrl(page: { slugs: string[]; locale?: string }) {
  const segments = [...page.slugs, 'image.png'];

  return { segments, url: getImageUrl(segments, page.locale) };
}
