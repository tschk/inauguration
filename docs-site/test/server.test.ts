import { expect, test, mock } from "bun:test";
import { serveIndex, renderer } from "../src/server";

test("serveIndex constructs RenderContext correctly and calls renderer", async () => {
  const originalRender = renderer.render;

  let capturedCtx: any;
  renderer.render = mock(async (ctx: any) => {
    capturedCtx = ctx;
    return new Response("mocked");
  });

  try {
    const controller = new AbortController();
    const request = new Request("http://localhost/", { signal: controller.signal });
    const response = await serveIndex(request);

    expect(await response.text()).toBe("mocked");
    expect(capturedCtx.request).toBe(request);
    expect(capturedCtx.route.id).toBe("index");
    expect(capturedCtx.params).toEqual({});
    expect(capturedCtx.signal).toBe(controller.signal);
  } finally {
    renderer.render = originalRender;
  }
});

test("serveIndex handles custom request url correctly", async () => {
  const req = new Request("http://localhost/some/custom/path");
  const res = await serveIndex(req);

  expect(res.status).toBe(200);
});
