import type { Metadata } from "next";

import { buttonVariants } from "@/components/ui/button";
import { DOCS_PATH, REPO_URL } from "@/lib/site";

// Exported as out/404.html, which GitHub Pages serves for every missing path.
// Next adds `noindex` to the not-found page by itself.
export const metadata: Metadata = {
  title: "Page not found",
};

export default function NotFound() {
  return (
    <main className="flex flex-1 flex-col items-center justify-center gap-6 p-6 text-center">
      <p className="text-sm font-semibold tracking-widest text-primary">404</p>
      <h1 className="font-heading text-3xl font-semibold tracking-tight sm:text-4xl">
        Page not found
      </h1>
      <p className="max-w-md text-muted-foreground">
        This page doesn&apos;t exist. It may have moved, or the link may be wrong.
      </p>
      <div className="flex flex-wrap justify-center gap-3">
        {/* Plain links styled as buttons: the base-ui Button would add role="button". */}
        <a href={DOCS_PATH} className={buttonVariants({ size: "lg" })}>
          Read the docs
        </a>
        <a href={REPO_URL} className={buttonVariants({ variant: "outline", size: "lg" })}>
          View on GitHub
        </a>
      </div>
    </main>
  );
}
