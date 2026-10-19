import { expect, test } from "bun:test";
import { serveIndex } from "../src/server";

test("serveIndex renders the index page", async () => {
  const req = new Request("http://localhost/");
  const res = await serveIndex(req);
  expect(res.status).toBe(200);
  expect(res.headers.get("content-type")).toContain("text/html");
  const html = await res.text();
  expect(html).toContain("<!DOCTYPE html>");
  expect(html).toContain("inauguration");
});
