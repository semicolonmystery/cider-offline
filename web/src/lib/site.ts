// Site constants: the single source for metadata, the OG image and the sitemap.
// docs/theme/head.hbs repeats SITE_URL and SITE_DESCRIPTION for the book; keep them in sync.

export const SITE_URL = "https://cider-offline.podik.cz";
export const SITE_NAME = "Cider Offline";
export const SITE_TAGLINE = "Use Cider (Apple Music client) offline";
export const SITE_TITLE = `${SITE_NAME}: ${SITE_TAGLINE}`;
export const SITE_DESCRIPTION =
  "A local proxy that caches your Apple Music library, downloads songs and lyrics, and serves them to Cider when you're offline.";
export const SITE_HOST = new URL(SITE_URL).host;
export const REPO_URL = "https://github.com/semicolonmystery/cider-offline";

/** The Open Graph / Twitter image, rendered by `app/og.png/route.tsx`. */
export const OG_IMAGE = {
  url: "/og.png",
  width: 1200,
  height: 630,
  type: "image/png",
  alt: SITE_TITLE,
} as const;

/** The mdBook, built into `out/docs` by the Pages workflow. */
export const DOCS_PATH = "/docs/";
export const DOCS_URL = `${SITE_URL}${DOCS_PATH}`;
