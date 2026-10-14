import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

// GitHub Pages serves a project site under /<repo>. The Pages workflow sets
// DOCS_BASE_PATH from actions/configure-pages; locally it is empty.
const basePath = process.env.DOCS_BASE_PATH ?? '';

/** @type {import('next').NextConfig} */
const config = {
  output: 'export',
  reactStrictMode: true,
  basePath,
  trailingSlash: true,
  images: { unoptimized: true },
  env: {
    NEXT_PUBLIC_BASE_PATH: basePath,
    // "owner/repo"; Actions sets GITHUB_REPOSITORY on every run.
    NEXT_PUBLIC_GITHUB_REPOSITORY: process.env.GITHUB_REPOSITORY ?? 'OWNER/kamoder',
    NEXT_PUBLIC_GITHUB_BRANCH: process.env.DOCS_GIT_BRANCH ?? 'main',
  },
};

export default withMDX(config);
