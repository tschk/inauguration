import { createBunServer } from "@tschk/moonshine-deploy-bun";
import { tryServeStatic } from "@tschk/moonshine-server";
import type { RenderContext, RouteArtifact } from "@tschk/moonshine-framework";
import { join } from "node:path";
import { pageIr } from "./ir";
import { renderer } from "./renderer";

const rootDir = join(import.meta.dir, "..");
const staticDir = join(rootDir, "static");
const distDir = join(rootDir, "dist");

const indexRoute: RouteArtifact = {
  id: "index",
  path: "/",
  file: "",
  mode: "static",
  runtime: "bun",
  decision: "static",
  clientEntries: [],
};

async function serveIndex(request: Request): Promise<Response> {
  const ctx: RenderContext = {
    request,
    route: indexRoute,
    params: {},
    data: pageIr,
    signal: request.signal ?? new AbortController().signal,
  };
  return renderer.render(ctx);
}

const fetch = async (request: Request): Promise<Response> => {
  const urlStr = request.url;
  const start = urlStr.indexOf("/", urlStr.indexOf("://") + 3);
  let pathname = "/";
  if (start !== -1) {
    const endQuery = urlStr.indexOf("?", start);
    const endHash = urlStr.indexOf("#", start);
    let end = urlStr.length;
    if (endQuery !== -1 && endHash !== -1) {
      end = Math.min(endQuery, endHash);
    } else if (endQuery !== -1) {
      end = endQuery;
    } else if (endHash !== -1) {
      end = endHash;
    }
    pathname = urlStr.slice(start, end);
    let i = pathname.length - 1;
    while (i > 0 && pathname.charCodeAt(i) === 47) {
      i--;
    }
    pathname = i === pathname.length - 1 ? pathname : pathname.slice(0, i + 1);
  }
  const method = request.method;

  if (method === "GET" || method === "HEAD") {
    if (pathname === "/") return serveIndex(request);
    if (pathname.startsWith("/static/")) {
      const staticRes = await tryServeStatic(
        staticDir,
        "/" + pathname.slice("/static/".length),
      );
      if (staticRes) return staticRes;
    }
    const docsRes = await tryServeStatic(distDir, pathname);
    if (docsRes) return docsRes;
  }

  return new Response("Not Found", { status: 404 });
};

const port = process.env.PORT ? Number(process.env.PORT) : 3000;
const server = createBunServer({ fetch, port });

if (import.meta.main) {
  console.log(`inauguration docs-site → http://localhost:${server.port}`);
}

export { server, fetch, renderer };
