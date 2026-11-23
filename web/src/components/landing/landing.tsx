// The landing page: header, hero, the tour of the app and the footer. Everything
// here is server-rendered; only the tour's animation and the Download label run
// in the browser.

import Link from "next/link";

import { buttonVariants } from "@/components/ui/button";
import { DOCS_PATH, REPO_URL, SITE_NAME, SITE_PROMISE, SPONSOR_URL } from "@/lib/site";
import { cn } from "@/lib/utils";

import { DownloadButton } from "./download-button";
import { Tour } from "./tour";

const linkClass =
  "rounded-sm underline-offset-4 decoration-from-font hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring";

function SiteHeader() {
  return (
    <header className="mx-auto flex h-16 w-full max-w-[1200px] items-center justify-between px-4 sm:px-8">
      <Link href="/" className={cn(linkClass, "flex items-center gap-2.5 font-semibold hover:no-underline")}>
        {/* eslint-disable-next-line @next/next/no-img-element -- static export, unoptimized images */}
        <img src="/app-icon.svg" alt="" width={28} height={28} className="size-7" />
        {SITE_NAME}
      </Link>
      <nav aria-label="Main" className="flex items-center gap-1 text-[0.9375rem]">
        <a href={DOCS_PATH} className={cn(linkClass, "inline-flex min-h-11 items-center px-3")}>
          Docs
        </a>
        <a href={REPO_URL} className={cn(linkClass, "inline-flex min-h-11 items-center px-3")}>
          GitHub
        </a>
        <a
          href={SPONSOR_URL}
          target="_blank"
          rel="noopener noreferrer"
          className={cn(linkClass, "inline-flex min-h-11 items-center px-3")}
        >
          Sponsor
        </a>
      </nav>
    </header>
  );
}

function Hero() {
  return (
    <div className="mx-auto flex max-w-[1200px] flex-col items-center px-4 pt-10 pb-4 text-center sm:px-8 sm:pt-14">
      <h1 className="text-[clamp(2.75rem,6vw+1rem,5.5rem)] leading-none font-bold tracking-[-0.035em]">
        {SITE_NAME}
      </h1>
      <p className="mt-4 max-w-[30ch] text-[clamp(1.125rem,0.6vw+1rem,1.375rem)] leading-[1.45] text-balance text-muted-foreground">
        {SITE_PROMISE}
      </p>
      <div className="mt-7 flex flex-wrap items-center justify-center gap-3">
        <DownloadButton />
        <a
          href={DOCS_PATH}
          className={cn(
            buttonVariants({ variant: "outline", size: "lg" }),
            "h-12 rounded-[10px] px-6 text-base font-semibold",
          )}
        >
          Read the docs
        </a>
      </div>
      <p className="mt-4 max-w-[46ch] text-sm leading-normal text-pretty text-muted-foreground">
        For Windows, macOS and Linux. Needs Cider 4 and an Apple Music subscription.
      </p>
    </div>
  );
}

function SiteFooter() {
  return (
    <footer className="border-t border-border bg-sidebar">
      <div className="mx-auto grid max-w-[1200px] gap-6 px-4 py-12 text-sm leading-normal text-muted-foreground sm:px-8">
        <p className="max-w-[65ch] text-pretty">
          {SITE_NAME} is for personal use with your own Apple Music subscription, so you can keep
          listening to your library in Cider when you&apos;re offline. It is not affiliated with or
          endorsed by Apple or the Cider Collective. Use it at your own risk.
        </p>
        <p className="flex flex-wrap gap-x-6 gap-y-2">
          <span>Open source under the MIT license.</span>
          <a href={REPO_URL} className={cn(linkClass, "text-foreground underline")}>
            Source code on GitHub
          </a>
          <a href={DOCS_PATH} className={cn(linkClass, "text-foreground underline")}>
            Documentation
          </a>
          <a
            href={SPONSOR_URL}
            target="_blank"
            rel="noopener noreferrer"
            className={cn(linkClass, "text-foreground underline")}
          >
            Sponsor
          </a>
        </p>
      </div>
    </footer>
  );
}

export function Landing() {
  return (
    <>
      <SiteHeader />
      <main className="flex-1">
        <Hero />
        <Tour />
      </main>
      <SiteFooter />
    </>
  );
}
