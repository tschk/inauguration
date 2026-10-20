import { expect, test } from "bun:test";
import { serveIndex } from "../src/server";

test("serveIndex handles requests correctly", async () => {
  const req = new Request("http://localhost/");
  const res = await serveIndex(req);

  expect(res.status).toBe(200);
  expect(res.headers.get("content-type")).toContain("text/html");

  const text = await res.text();
  expect(text).toContain("<!DOCTYPE html>");
  expect(text).toContain("Ultrafast compiler pipeline");
});

test("serveIndex handles custom request url correctly", async () => {
  const req = new Request("http://localhost/some/custom/path");
  const res = await serveIndex(req);

  expect(res.status).toBe(200);
});
