import type { MetadataRoute } from "next";

import { docsChapterUrls } from "@/lib/docs";
import { DOCS_URL } from "@/lib/site";

export const dynamic = "force-static";

// The root only redirects to /docs/, so it is left out.
export default function sitemap(): MetadataRoute.Sitemap {
  return [DOCS_URL, ...docsChapterUrls()].map((url) => ({ url }));
}
