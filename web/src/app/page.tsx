import type { Metadata } from "next";

import { SITE_OPEN_GRAPH } from "@/lib/metadata";
import { DOCS_PATH, SITE_NAME } from "@/lib/site";

// The root has no content yet: it sends visitors and crawlers to the book.
export const metadata: Metadata = {
  alternates: { canonical: DOCS_PATH },
  openGraph: { ...SITE_OPEN_GRAPH, url: DOCS_PATH },
};

export default function Home() {
  return (
    <main className="flex flex-1 items-center justify-center p-6">
      {/* React hoists this into <head>. */}
      <meta httpEquiv="refresh" content={`0; url=${DOCS_PATH}`} />
      <p className="text-muted-foreground">
        Redirecting to the{" "}
        <a href={DOCS_PATH} className="text-primary underline underline-offset-4">
          {SITE_NAME} documentation
        </a>
        …
      </p>
    </main>
  );
}
