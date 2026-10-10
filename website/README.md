# Kamoder documentation site

[Fumadocs](https://fumadocs.dev) on Next.js, exported as a static site and
deployed to GitHub Pages by `.github/workflows/docs.yml`.

```bash
npm ci
npm run dev          # http://localhost:3000
npm run build        # static export → out/
npm run types:check
npm run lint
```

- Pages: `content/docs/**/*.mdx`; sidebar order and separators in `meta.json`.
- Sidebar icons: `icon: <LucideName>` in frontmatter or `meta.json`
  (any name from [lucide.dev](https://lucide.dev/icons)).
- Link to repository files with `<RepoLink path="tools/autostart" />`; the
  owner/repo come from `GITHUB_REPOSITORY` at build time.
- `DOCS_BASE_PATH=/<repo>` builds for a project Pages site (CI sets it).
