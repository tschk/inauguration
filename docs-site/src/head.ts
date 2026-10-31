export const seoTitle = "inauguration — inlang & hybrid compiler docs";
export const seoDescription =
  "Ultrafast Core IR → native JIT pipeline for .in (inlang) and polyglot fronts. Documentation built with crepuscularity.";

export const headHtml = `<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${seoTitle}</title>
<meta name="description" content="${seoDescription}">
<meta name="robots" content="index,follow">
<meta name="twitter:card" content="summary">
<meta name="theme-color" content="#09090b">
<meta name="generator" content="crepuscularity">
<link rel="canonical" href="https://inauguration.tsc.hk/">
<link rel="icon" href="/static/favicon.svg" type="image/svg+xml">
<meta name="theme-color" content="#09090b">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:ital,wght@0,100..800;1,100..800&display=swap" rel="stylesheet">
<script src="/static/splash.js" defer></script>
<link rel="stylesheet" href="/static/page-switcher.css">
<script src="/static/page-switcher.js" defer></script>
<script src="/static/vendor/unocss.js"></script>
<script>window.addEventListener("load",function(){document.querySelectorAll("[data-crepus-root]").forEach(function(n){n.classList.add("ready")})})</script>
<noscript><style>
[data-crepus-root] { opacity: 1 !important; }
.in-docs-page { padding: 24px; }
.site-header, .site-header > div:first-child, .header-links { display: flex; gap: 12px; }
.site-header, .header-links { flex-wrap: wrap; }
.site-header img { width: 32px; height: 32px; }
.in-docs-page a { color: #60a5fa; }
.site-header a { color: #a1a1aa; }
</style></noscript>
<style>
html { scroll-behavior: smooth; }
html, body { margin: 0; }
body {
  margin: 0;
  min-height: 100vh;
  background: #09090b;
  color: #f4f4f5;
  font-family: "JetBrains Mono", ui-monospace, SFMono-Regular, monospace;
  -webkit-font-smoothing: antialiased;
}
[data-crepus-root] { opacity: 0; }
[data-crepus-root].ready { opacity: 1; }
</style>
<style>
.in-docs-page {
  --cp-bg: #09090b;
  --cp-fg: #f4f4f5;
  --cp-muted: #a1a1aa;
  --cp-border: #27272a;
  --cp-elevated: #18181b;
  --cp-code: #09090b;
  --cp-accent: #3b82f6;
  --cp-shadow: 0 4px 20px rgba(0, 0, 0, 0.5);
  font-family: "JetBrains Mono", ui-monospace, SFMono-Regular, monospace;
  background-color: var(--cp-bg);
  color: var(--cp-fg);
  -webkit-font-smoothing: antialiased;
}
.in-docs-page code,
.in-docs-page pre,
.in-docs-page .font-mono {
  font-family: "JetBrains Mono", ui-monospace, SFMono-Regular, monospace;
}
.crepus-t-text { color: var(--cp-fg); }
.crepus-t-muted { color: var(--cp-muted); }
.crepus-t-border { border-color: var(--cp-border) !important; }
.crepus-t-card {
  background-color: var(--cp-elevated);
  border: 1px solid var(--cp-border);
  box-shadow: var(--cp-shadow);
}
.crepus-t-pre {
  background-color: var(--cp-elevated);
  color: var(--cp-fg);
  border: 1px solid var(--cp-border);
}
.crepus-t-btn-primary {
  background-color: var(--cp-fg);
  color: var(--cp-bg);
  border: 1px solid var(--cp-fg);
}
.crepus-t-btn-primary:hover { opacity: 0.92; background-color: #ffffff; }
.crepus-t-btn-ghost {
  border: 1px solid var(--cp-border);
  color: var(--cp-fg);
  background-color: var(--cp-bg);
}
.crepus-t-btn-ghost:hover { background-color: var(--cp-elevated); }
.crepus-t-icon { color: var(--cp-accent); }
.in-docs-page a:focus-visible {
  outline: 2px solid var(--cp-accent);
  outline-offset: 3px;
}
@media (prefers-reduced-motion: reduce) {
  .crepus-entrance { animation: none !important; }
}
@keyframes in-docs-fade-up {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: none; }
}
.crepus-entrance { animation: in-docs-fade-up 0.45s ease-out both; }
.crepus-e1 { animation-delay: 0.02s; }
.crepus-e2 { animation-delay: 0.06s; }
.crepus-e3 { animation-delay: 0.1s; }
.crepus-e4 { animation-delay: 0.14s; }
.crepus-e5 { animation-delay: 0.16s; }
.crepus-e6 { animation-delay: 0.2s; }
@keyframes in-docs-logo-pulse {
  0%, 100% { box-shadow: 0 0 0 0 rgba(59, 130, 246, 0); }
  50% { box-shadow: 0 0 20px 2px rgba(59, 130, 246, 0.25); }
}
.in-docs-logo-pulse { animation: in-docs-logo-pulse 4s ease-in-out infinite; }
@keyframes in-docs-shimmer {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.65; }
}
.in-docs-shimmer { animation: in-docs-shimmer 3s ease-in-out infinite; }
@keyframes in-docs-terminal-glow {
  0%, 100% { border-color: #27272a; }
  50% { border-color: #3f3f46; box-shadow: 0 0 24px rgba(59, 130, 246, 0.08); }
}
.in-docs-terminal-glow { animation: in-docs-terminal-glow 5s ease-in-out infinite; }
.in-docs-card-hover {
  transition: transform 0.25s ease, border-color 0.25s ease, box-shadow 0.25s ease;
}
.in-docs-card-hover:hover {
  transform: translateY(-2px);
  border-color: #3b82f6 !important;
  box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
}
@keyframes in-docs-chip-float {
  0%, 100% { transform: translateY(0); }
  50% { transform: translateY(-1px); }
}
.in-docs-chip-float:nth-child(odd) { animation: in-docs-chip-float 4s ease-in-out infinite; }
.in-docs-chip-float:nth-child(even) { animation: in-docs-chip-float 4.5s ease-in-out infinite 0.5s; }
</style>
`;
