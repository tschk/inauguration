import { afterAll, expect, test } from "bun:test";
import { fetch as serverFetch, server } from "../src/server";

afterAll(async () => {
  await server.stop(true);
});

test("URL parsing edge cases in fetch", async () => {
  const cases = [
    { url: "http://localhost/?foo=bar", expected: 200 },
    { url: "http://localhost/#hash", expected: 200 },
    { url: "http://localhost/?foo=bar#hash", expected: 200 },
    { url: "http://localhost///", expected: 200 },
    { url: "http://localhost/static/favicon.svg/", expected: 200 },
    { url: "http://localhost/static/favicon.svg//", expected: 200 },
    { url: "http://localhost/static/favicon.svg?v=1", expected: 200 },
    { url: "http://localhost/static/favicon.svg#hash", expected: 200 },
    { url: "http://localhost/static/favicon.svg?v=1#hash", expected: 200 },
    { url: "http://localhost/not-found?foo=1&bar=2", expected: 404 },
    { url: "http://localhost:3000/", expected: 200 },
    { url: "http://localhost:3000/?foo=1#bar", expected: 200 },
    { url: "https://example.com/", expected: 200 },
    { url: "http://localhost", expected: 200 },
    { url: "http://localhost/something//?foo=1#bar", expected: 404 },
  ];

  for (const { url, expected } of cases) {
    const req = new Request(url);
    const res = await serverFetch(req);
    expect(res.status).toBe(expected);
  }
});

test("fetch handles different HTTP methods", async () => {
  const methods = ["POST", "PUT", "DELETE", "PATCH"];
  for (const method of methods) {
    const req = new Request("http://localhost/", { method });
    const res = await serverFetch(req);
    expect(res.status).toBe(404);
  }

  const reqHead = new Request("http://localhost/", { method: "HEAD" });
  const resHead = await serverFetch(reqHead);
  expect(resHead.status).toBe(200);
});
