import { afterAll, expect, test } from "bun:test";

process.env.PORT = "0";
const { server } = await import("../src/server");

const base = server.url.origin;

afterAll(async () => {
  await server.stop(true);
});

test("GET / returns 200 HTML with page content", async () => {
  const res = await fetch(`${base}/`);
  expect(res.status).toBe(200);
  expect(res.headers.get("content-type")).toContain("text/html");
  const html = await res.text();
  expect(html).toContain("<!DOCTYPE html>");
  expect(html).toContain('<html lang="en">');
  expect(html).toContain('<meta charset="utf-8">');
  expect(html).toContain(
    '<meta name="viewport" content="width=device-width, initial-scale=1">',
  );
  expect(html).toContain("html, body { margin: 0; }");
  expect(html).toContain("/static/vendor/unocss.js");
  expect(html).toContain("inauguration");
  expect(html).toContain("Ultrafast compiler pipeline");
  expect(html).not.toContain(
    "THE STACK: INLANG + CREPUSCULARITYUltrafast compiler pipeline",
  );
  expect(html).toContain("flex-1 max-w-2xl flex flex-col");
  expect(html).toMatch(
    /class="block text-blue-400[^"]*mb-2"[^>]*>[\s\S]*THE STACK: INLANG \+ CREPUSCULARITY/,
  );
  expect(html).toMatch(
    /class="block text-2xl[^"]*mb-4"[^>]*>[\s\S]*Ultrafast compiler pipeline/,
  );
  expect(html).toContain("PIPELINE CAPABILITIES");
  expect(html).toContain("DOCUMENTATION DIRECTORY");
  expect(html).toContain("https://github.com/tschk/inauguration");
  expect(html).not.toContain(
    "https://github.com/semitechnological/inauguration",
  );
  expect(html).toContain("JetBrains Mono");
  expect(html).toContain('class="min-h-screen');
  expect(html.slice(html.indexOf("<body"))).not.toContain("style=");
});

test("GET /static/favicon.svg returns 200 svg", async () => {
  const res = await fetch(`${base}/static/favicon.svg`);
  expect(res.status).toBe(200);
  expect(res.headers.get("content-type")).toContain("svg");
});

test("GET unknown path returns 404", async () => {
  const res = await fetch(`${base}/does-not-exist`);
  expect(res.status).toBe(404);
});
