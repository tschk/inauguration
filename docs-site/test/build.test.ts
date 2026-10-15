import { afterEach, beforeEach, expect, mock, spyOn, test } from "bun:test";
import * as fs from "node:fs/promises";
import { join } from "node:path";
import { buildSite } from "../src/build";
import { renderer } from "../src/renderer";

let mockSpawn: ReturnType<typeof spyOn>;

beforeEach(() => {
  // Module mocks survive mock.restore() and would leak into dist/site tests.
  spyOn(fs, "rm").mockResolvedValue(undefined);
  spyOn(fs, "mkdir").mockResolvedValue(undefined);
  spyOn(fs, "cp").mockResolvedValue(undefined);
  spyOn(fs, "writeFile").mockResolvedValue(undefined);
  spyOn(renderer, "prerender").mockResolvedValue("mock-html");
  mockSpawn = spyOn(Bun, "spawn").mockImplementation(() => {
    return {
      exited: Promise.resolve(0),
    } as any;
  });
});

afterEach(() => {
  mock.restore();
  mockSpawn.mockRestore();
  delete process.env.DOCS_SITE_OUT;
});

test("buildSite creates and populates the output directory", async () => {
  const outDir = "/tmp/mock-dist";
  await buildSite({ outDir });

  expect(fs.rm).toHaveBeenCalledWith(outDir, { recursive: true, force: true });
  expect(fs.mkdir).toHaveBeenCalledWith(outDir, { recursive: true });

  const docsOut = join(outDir, "docs");
  expect(fs.mkdir).toHaveBeenCalledWith(docsOut, { recursive: true });

  expect(renderer.prerender).toHaveBeenCalled();
  expect(fs.writeFile).toHaveBeenCalledWith(
    join(outDir, "index.html"),
    "mock-html",
  );

  expect(fs.cp).toHaveBeenCalled();
  const cpCalls = (fs.cp as ReturnType<typeof mock>).mock.calls;
  expect(cpCalls[0][1]).toBe(join(outDir, "static"));
  expect(cpCalls[0][2]).toEqual({ recursive: true });

  expect(mockSpawn).toHaveBeenCalled();
  const spawnCalls = mockSpawn.mock.calls;
  expect(spawnCalls[0][0][0]).toBe("bash");
  expect(spawnCalls[0][0].includes("--out-dir")).toBe(true);
  expect(spawnCalls[0][0].includes(docsOut)).toBe(true);

  expect(fs.writeFile).toHaveBeenCalledWith(
    join(outDir, "404.html"),
    expect.any(String),
  );
  expect(fs.writeFile).toHaveBeenCalledWith(
    join(outDir, "_redirects"),
    expect.any(String),
  );
  expect(fs.writeFile).toHaveBeenCalledWith(
    join(outDir, "CNAME"),
    "inauguration.tsc.hk\n",
  );
});

test("buildSite throws when docs-gen exits with non-zero code", async () => {
  mockSpawn.mockRestore();
  mockSpawn = spyOn(Bun, "spawn").mockImplementation(() => {
    return {
      exited: Promise.resolve(1),
    } as any;
  });

  const outDir = "/tmp/mock-dist-fail";
  await expect(buildSite({ outDir })).rejects.toThrow("docs-gen exited 1");
});

test("buildSite uses DOCS_SITE_OUT environment variable as fallback", async () => {
  process.env.DOCS_SITE_OUT = "/tmp/env-dist";
  await buildSite({});

  expect(fs.rm).toHaveBeenCalledWith("/tmp/env-dist", {
    recursive: true,
    force: true,
  });
  expect(fs.mkdir).toHaveBeenCalledWith("/tmp/env-dist", { recursive: true });
});
