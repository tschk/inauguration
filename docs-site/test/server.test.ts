import { expect, test } from "bun:test";
import { fetch } from "../src/server";

test("fetch handles URL parsing edge cases", async () => {
  const mockRequest = (url: string) => new Request(url, { method: "GET" });

  // 1. Base case: standard paths
  const resRoot = await fetch(mockRequest("http://localhost/"));
  expect(resRoot.status).toBe(200);

  const resStatic = await fetch(
    mockRequest("http://localhost/static/favicon.svg"),
  );
  expect(resStatic.status).toBe(200);

  // 2. Query parameters
  const resQuery = await fetch(mockRequest("http://localhost/?foo=bar"));
  expect(resQuery.status).toBe(200);

  const resStaticQuery = await fetch(
    mockRequest("http://localhost/static/favicon.svg?foo=bar"),
  );
  expect(resStaticQuery.status).toBe(200);

  // 3. Hash fragments
  const resHash = await fetch(mockRequest("http://localhost/#section"));
  expect(resHash.status).toBe(200);

  const resStaticHash = await fetch(
    mockRequest("http://localhost/static/favicon.svg#section"),
  );
  expect(resStaticHash.status).toBe(200);

  // 4. Both query and hash
  const resQueryHash = await fetch(
    mockRequest("http://localhost/?foo=bar#section"),
  );
  expect(resQueryHash.status).toBe(200);

  const resHashQuery = await fetch(
    mockRequest("http://localhost/#section?foo=bar"),
  );
  expect(resHashQuery.status).toBe(200);

  // 5. Trailing slashes
  const resTrailingSlash = await fetch(
    mockRequest("http://localhost/static/favicon.svg/"),
  );
  expect(resTrailingSlash.status).toBe(200);

  const resMultipleSlashes = await fetch(
    mockRequest("http://localhost/static/favicon.svg///"),
  );
  expect(resMultipleSlashes.status).toBe(200);

  const resRootMultipleSlashes = await fetch(
    mockRequest("http://localhost///"),
  );
  expect(resRootMultipleSlashes.status).toBe(200);

  // 6. Combined trailing slashes, query, and hash
  const resCombined = await fetch(
    mockRequest("http://localhost/static/favicon.svg///?foo=bar#section"),
  );
  expect(resCombined.status).toBe(200);

  // 7. Request with implicit trailing slash
  const reqNoPath = new Request("http://localhost");
  const resNoPath = await fetch(reqNoPath);
  expect(resNoPath.status).toBe(200);
});
