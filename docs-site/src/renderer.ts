import { crepusRenderer } from "@tschk/crepus-moonshine";
import type { RenderContext, Renderer } from "@tschk/moonshine-framework";
import { headHtml } from "./head";

function wrapDocument(html: string): string {
  const htmlIdx = html.indexOf("<html>");
  const headIdx = html.indexOf("<head>");

  if (htmlIdx !== -1 && headIdx !== -1) {
    if (htmlIdx < headIdx) {
      return (
        html.slice(0, htmlIdx) +
        '<html lang="en">' +
        html.slice(htmlIdx + 6, headIdx) +
        `<head>${headHtml}` +
        html.slice(headIdx + 6)
      );
    } else {
      return (
        html.slice(0, headIdx) +
        `<head>${headHtml}` +
        html.slice(headIdx + 6, htmlIdx) +
        '<html lang="en">' +
        html.slice(htmlIdx + 6)
      );
    }
  }

  let res = html;
  if (htmlIdx !== -1) {
    res = res.slice(0, htmlIdx) + '<html lang="en">' + res.slice(htmlIdx + 6);
  }
  if (headIdx !== -1) {
    const newHeadIdx = res.indexOf("<head>");
    res =
      res.slice(0, newHeadIdx) +
      `<head>${headHtml}` +
      res.slice(newHeadIdx + 6);
  }
  return res;
}

export const renderer: Renderer = {
  name: "crepus-head",
  async render(context: RenderContext): Promise<Response> {
    const res = await crepusRenderer.render(context);
    const html = await res.text();
    return new Response(wrapDocument(html), {
      status: res.status,
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  },
  async prerender(context: RenderContext): Promise<string> {
    const html = await crepusRenderer.prerender(context);
    return wrapDocument(html);
  },
};
