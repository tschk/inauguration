# Documentation

Guides for **inlang** (`.in`), the inauguration compiler, and the docs site.

| Guide | Description |
| --- | --- |
| [inlang (.in)](in-language.md) | Syntax, packages, capabilities, Core IR surface |
| [General compiler](general-compiler.md) | CLI driver, pipeline stages, owned compile |
| [Multi-front Core IR](multi-frontend-ir.md) | `UnifiedModule`, fronts, lowering |
| [Language fronts](languages.md) | Live matrix; `in languages --json` |
| [Parser surface](parser-surface.md) | Extension → `ParserId`, maturity levels |
| [Native backend](native-backend.md) | MIR, JIT, AArch64 / x86_64 emit |
| [Emit profiles](emit-profiles.md) | `default` / `harden` / `lean` anti-decomp & inlining |
| [Docs-site](docs-site.md) | `crepus web`, `backend.in`, Cloudflare deploy |
| [Changelog](changelog.md) | Released crate and GitHub binary versions |
| [Benchmarks](benchmarks/README.md) | JIT, polyglot `in` vs native toolchains, self-host vs rustc |

**Published HTML**

`crepus web build` runs **`[targets.docs]`**: Markdown here → `docs-site/dist/docs/` (for example `in-language.html`, `benchmarks/jit.html`). Notes under [`internal/`](internal/) are not published.

Live language matrix: **`in languages --json`**.