# Crate-level Documentation

The Markdown files in this directory are the module-doc pages for
`polaris-ai`'s top-level modules. Each one is included into rustdoc via
`#[doc = include_str!("docs/X.md")]` in `src/lib.rs`, which is how the
content appears on
[docs.rs](https://docs.rs/polaris-ai).

## What lives here vs. where

| Concern | Location | Why |
|---------|----------|-----|
| Module overview pages (this directory) | `src/docs/*.md` | Shown on docs.rs at the module's URL. Best for "what is this module?" intros and concept tables. |
| **Catalogs** — Plugin / API / Resource indices | `src/docs/{plugins,apis,resources}.md` | First place a consumer lands when asking *"what does this workspace ship?"* — each one has a CI drift guard under `tests/`. |
| **Reference docs** — primitives, contracts, standards | `docs/reference/*.md` | The deep "how do `X` work and how must they be documented" pages. Lives at the repo root for GitHub-first browsing; the docs.rs `system` / `graph` / etc. module pages link back into them. |
| **Documentation standards** | `docs/reference/{plugins,api,resources}.md#documentation-standard` | The required-sections spec each plugin / API / resource rustdoc must follow. Enforced by `/review-docs` on every PR. |
| **Integration guide** ("how do I X?") | `docs/reference/guide.md` | The discovery front door — maps goals to the plugin/API/resource combinations that solve them. |

If you're authoring a new plugin / API / resource, the page you want is in
`docs/reference/`. If you're cataloguing one for discoverability, the page
you want is in `src/docs/`. The two layers are complementary, not duplicated:
the reference pages explain the contract, the catalogs index every concrete
implementation that satisfies it.
