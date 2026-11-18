# CLAUDE.md (web/)

The website at <https://cider-offline.podik.cz>: a Next.js 16 static export
(React 19, TypeScript, Tailwind 4, shadcn on base-ui). For now it has no
content of its own. The root redirects to the mdBook, which the Pages workflow
builds into `out/docs`. The repository rules in `../CLAUDE.md` apply too.

## 1. Rules

### 1.1 Verify against docs, not memory

Read the docs for the installed version before using an API. For Next.js that
is `node_modules/next/dist/docs/`; for anything else, use context7 or the
official docs. Next 16, React 19 and Tailwind 4 have breaking changes against
what you likely remember.

### 1.2 "Done" means verified

`npm run lint`, `npm run typecheck` and `npm run build` must all pass. Until
they do, the work is written but not verified.

### 1.3 No git operations unless told

No branches, commits or pushes on your own initiative. Commits are the owner's:
no `Co-Authored-By` trailers and no Claude/AI mentions.

### 1.4 shadcn only through the CLI

Add components with `npx shadcn@latest add <name>`. Never edit
`src/components/ui/`; compose around it. A link that looks like a button is an
`<a>` with `buttonVariants(...)`, not `<Button render={<a />}>`.

### 1.5 Static export only

Everything renders at build time (`output: "export"`). There are no server
features: no Route Handlers that read the request, no redirects, no headers,
no Server Actions. A metadata or route file that generates a file needs
`export const dynamic = "force-static"`.

### 1.6 No hard-coded site values

The URL, name, tagline, description, repository and OG image come from
`src/lib/site.ts`. `../docs/theme/head.hbs` repeats some of them for the
book, so keep the two in sync.

## 2. Where code goes

| Folder | Holds |
|---|---|
| `src/app/` | Routes only, thin: layout, page, not-found, sitemap, robots, `og.png/` and the icon files. |
| `src/components/ui/` | shadcn primitives, CLI-managed. Never edit. |
| `src/components/` | Shared components. |
| `src/lib/` | Site constants, shared metadata and helpers. |

A component starts next to its only consumer. It is promoted to
`src/components/` when a second consumer needs it.

## 3. Things that will surprise you

- `npm run build` reads outside `web/`. The sitemap comes from
  `../docs/src/SUMMARY.md`, and `og.png` draws `../packaging/icons/512x512.png`.
- The OG image is a route handler (`src/app/og.png/route.tsx`), not
  `opengraph-image.tsx`. In a static export that convention writes a file with
  no extension, which GitHub Pages would serve as `application/octet-stream`.
- `src/app/icon.svg`, `apple-icon.png` and `favicon.ico` are copies from
  `../packaging/icons/`. Copy them again after the app icon changes.
- Light and dark follow `prefers-color-scheme`; there is no theme switcher.
  The colours in `src/app/globals.css` are the app's palette.
- `out/404.html` is the 404 page for the whole domain, `/docs/*` included.

## 4. Commands

```bash
npm ci               # install from the lockfile
npm run dev          # dev server on localhost:3000 (no book under /docs/)
npm run lint
npm run typecheck
npm run build        # static export to out/
```

The full site, like `.github/workflows/pages.yml` builds it: run
`npm run build`, then `mdbook build ../docs --dest-dir out/docs`, then the
workflow's step that rewrites the book's `.md` canonical URLs.
