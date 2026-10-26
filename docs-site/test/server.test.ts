import { expect, test, spyOn } from "bun:test";
import { serveIndex, renderer } from "../src/server";

test("serveIndex constructs RenderContext correctly and calls renderer", async () => {
  let capturedCtx: any;
  const renderSpy = spyOn(renderer, "render").mockImplementation(async (ctx: any) => {
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
    expect(capturedCtx.signal).toBe(request.signal);
  } finally {
    renderSpy.mockRestore();
  }
});
