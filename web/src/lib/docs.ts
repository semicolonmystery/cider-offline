import { readFileSync } from "node:fs";
import { join } from "node:path";

import { DOCS_URL } from "@/lib/site";

// The book's table of contents. `next build` runs with web/ as the working directory.
const SUMMARY_PATH = join(process.cwd(), "..", "docs", "src", "SUMMARY.md");

// A chapter link in SUMMARY.md: `[Title](path/to/chapter.md)`. Draft chapters
// (`[Title]()`) and external links don't match.
const CHAPTER_LINK = /\[[^\]]*\]\(\s*<?([^)\s>]+\.md)>?\s*\)/g;

/** Absolute URLs of every chapter page mdBook renders, in SUMMARY.md order. */
export function docsChapterUrls(): string[] {
  const summary = readFileSync(SUMMARY_PATH, "utf8");
  const urls = Array.from(summary.matchAll(CHAPTER_LINK), (match) => {
    const page = match[1].replace(/^\.\//, "").replace(/\.md$/, ".html");
    return new URL(page, DOCS_URL).href;
  });
  return [...new Set(urls)];
}
