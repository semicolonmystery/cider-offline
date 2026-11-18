import type { Metadata } from "next";

import { OG_IMAGE, SITE_DESCRIPTION, SITE_NAME, SITE_TITLE } from "@/lib/site";

// Open Graph fields shared by every page. A page that sets `openGraph` replaces
// the layout's object, so it spreads this one and overrides what differs.
export const SITE_OPEN_GRAPH = {
  type: "website",
  siteName: SITE_NAME,
  title: SITE_TITLE,
  description: SITE_DESCRIPTION,
  locale: "en",
  url: "/",
  images: [OG_IMAGE],
} satisfies Metadata["openGraph"];

export const SITE_TWITTER = {
  card: "summary_large_image",
  title: SITE_TITLE,
  description: SITE_DESCRIPTION,
  images: [OG_IMAGE],
} satisfies Metadata["twitter"];
