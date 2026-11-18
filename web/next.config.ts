import type { NextConfig } from "next";

// A static export for GitHub Pages. The workflow builds the mdBook into
// out/docs, so the site has no basePath and keeps the default trailingSlash:
// `/` is out/index.html and missing paths get out/404.html.
const nextConfig: NextConfig = {
  output: "export",
  images: {
    unoptimized: true,
  },
};

export default nextConfig;
