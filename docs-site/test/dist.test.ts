import { afterAll, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSite } from "../src/build";

const outDir = await mkdtemp(join(tmpdir(), "inauguration-docs-dist-"));

afterAll(async () => {
  await rm(outDir, { recursive: true, force: true });
});

await buildSite({ outDir });

const homepage = await readFile(join(outDir, "index.html"), "utf8");
const docsPage = await readFile(
  join(outDir, "docs", "in-language.html"),
  "utf8",
);
const languagesPage = await readFile(
  join(outDir, "docs", "languages.html"),
  "utf8",
);
const favicon = await readFile(join(outDir, "static", "favicon.svg"), "utf8");
const splash = await readFile(join(outDir, "static", "splash.js"), "utf8");
const unocss = await readFile(
  join(outDir, "static", "vendor", "unocss.js"),
  "utf8",
);
const notFound = await readFile(join(outDir, "404.html"), "utf8");

test("homepage is moonshine landing HTML with tschk GitHub links", () => {
  expect(homepage).toContain("<!DOCTYPE html>");
  expect(homepage).toContain('<html lang="en">');
  expect(homepage).toContain('<meta charset="utf-8">');
  expect(homepage).toContain(
    '<meta name="viewport" content="width=device-width, initial-scale=1">',
  );
  expect(homepage).toContain("html, body { margin: 0; }");
  expect(homepage).toContain("/static/vendor/unocss.js");
  expect(homepage).toContain("DOCUMENTATION DIRECTORY");
  expect(homepage).not.toContain(
    "THE STACK: INLANG + CREPUSCULARITYUltrafast compiler pipeline",
  );
  expect(homepage).toContain("flex-1 max-w-2xl flex flex-col");
  expect(homepage).toContain("https://github.com/tschk/inauguration");
  expect(homepage).not.toContain(
    "https://github.com/semitechnological/inauguration",
  );
});

test("docs HTML is generated documentation, not the homepage", () => {
  expect(docsPage).toContain("<!DOCTYPE html>");
  expect(docsPage).toContain("doc-shell");
  expect(docsPage).toContain("inlang");
  expect(docsPage).not.toBe(homepage);
  expect(docsPage).not.toContain('data-moonshine-app="index"');
  expect(docsPage).not.toContain("DOCUMENTATION DIRECTORY");
  expect(languagesPage).toContain("doc-shell");
  expect(languagesPage).not.toBe(homepage);
});

test("changelog is generated docs HTML and linked from the homepage", async () => {
  const changelog = await readFile(
    join(outDir, "docs", "changelog.html"),
    "utf8",
  );
  expect(changelog).toContain("doc-shell");
  expect(changelog).toContain("0.9.8");
  expect(changelog).toContain("0.9.7");
  expect(changelog).toContain("dual-emit");
  expect(changelog).toContain("remove_dead_functions");
  expect(changelog).toContain("Changelog");
  expect(changelog).toContain("doc-nav-section");
  expect(changelog).toContain("Project");
  expect(changelog).not.toBe(homepage);
  expect(homepage).toContain("./docs/changelog.html");
  expect(homepage).toContain("Changelog");
});

test("static assets are real files, not homepage HTML", () => {
  expect(favicon).toContain("<svg");
  expect(favicon).not.toContain("<!DOCTYPE html>");
  expect(favicon).not.toBe(homepage);
  expect(splash.startsWith("<!DOCTYPE html>")).toBe(false);
  expect(splash).not.toBe(homepage);
  expect(unocss.startsWith("<!DOCTYPE html>")).toBe(false);
  expect(unocss).not.toBe(homepage);
});

test("unknown paths are not SPA-rewritten to the homepage", async () => {
  const redirects = Bun.file(join(outDir, "_redirects"));
  if (await redirects.exists()) {
    const text = await redirects.text();
    expect(text).not.toMatch(/\/\*\s+\/index\.html\s+200/);
  }
  expect(notFound).toContain("Not Found");
  expect(notFound).not.toBe(homepage);
  expect(notFound).not.toContain("DOCUMENTATION DIRECTORY");
});
