import { cp, mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { RenderContext, RouteArtifact } from "@tschk/moonshine-framework";
import { pageIr } from "./ir";
import { renderer } from "./renderer";

const route: RouteArtifact = {
  id: "index",
  path: "/",
  file: "",
  mode: "static",
  runtime: "bun",
  decision: "static",
  clientEntries: [],
};

const notFoundHtml = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Not Found — inauguration</title>
<meta name="robots" content="noindex">
<style>
  body{margin:0;min-height:100vh;background:#09090b;color:#a1a1aa;font-family:"JetBrains Mono",ui-monospace,monospace;display:flex;align-items:center;justify-content:center}
  main{text-align:center;padding:2rem}
  h1{color:#f4f4f5;font-size:1.25rem}
  a{color:#3b82f6}
</style>
</head>
<body>
<main>
<h1>Not Found</h1>
<p>This path is not part of the inauguration docs site.</p>
<p><a href="/">Back to inauguration</a></p>
</main>
</body>
</html>
`;

const redirects = `/docs/jit.html /docs/benchmarks/jit.html 301
/docs/polyglot-compilers.html /docs/benchmarks/polyglot-compilers.html 301
/docs/self-host-vs-native.html /docs/benchmarks/self-host-vs-native.html 301
`;

export async function buildSite(
  options: { outDir?: string } = {},
): Promise<void> {
  const siteDir = join(import.meta.dir, "..");
  const repoRoot = join(siteDir, "..");
  const outDir =
    options.outDir ?? process.env.DOCS_SITE_OUT ?? join(siteDir, "dist");

  await rm(outDir, { recursive: true, force: true });
  await mkdir(outDir, { recursive: true });

  const ctx: RenderContext = {
    request: new Request("http://localhost/"),
    route,
    params: {},
    data: pageIr,
    signal: new AbortController().signal,
  };

  const docsOut = join(outDir, "docs");

  await Promise.all([
    // Render and write index.html
    renderer.prerender(ctx).then((html) => writeFile(join(outDir, "index.html"), html)),

    // Copy static directory
    cp(join(siteDir, "static"), join(outDir, "static"), {
      recursive: true,
    }),

    // Build docs
    (async () => {
      await mkdir(docsOut, { recursive: true });
      const hook = join(siteDir, "scripts", "docs-hook.sh");
      const proc = Bun.spawn(
        [
          "bash",
          hook,
          "--docs-src",
          join(repoRoot, "docs"),
          "--out-dir",
          docsOut,
          "--site-name",
          "inauguration",
        ],
        { cwd: siteDir, stdout: "inherit", stderr: "inherit" },
      );
      const code = await proc.exited;
      if (code !== 0) {
        throw new Error(`docs-gen exited ${code}`);
      }
    })(),

    // Write static files
    writeFile(join(outDir, "404.html"), notFoundHtml),
    writeFile(join(outDir, "_redirects"), redirects),
    writeFile(join(outDir, "CNAME"), "inauguration.tsc.hk\n"),
  ]);

  console.log(`wrote ${outDir}`);
}

async function main(): Promise<void> {
  await buildSite();
}

if (import.meta.main) {
  main();
}
