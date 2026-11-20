import type { MetadataRoute } from "next";

import { docsChapterUrls } from "@/lib/docs";
import { DOCS_URL, SITE_URL } from "@/lib/site";

export const dynamic = "force-static";

// The landing page, then the book's pages in SUMMARY.md order.
export default function sitemap(): MetadataRoute.Sitemap {
  return [SITE_URL, DOCS_URL, ...docsChapterUrls()].map((url) => ({ url }));
}
