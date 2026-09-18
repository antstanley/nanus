# nanus docs site

The documentation site for `nanus`, built with [Astro](https://astro.build) and
[Starlight](https://starlight.astro.build) and published to
[nanus.iamstan.dev](https://nanus.iamstan.dev) by
[blogwright](https://github.com/antstanley/blogwright).

It is a pnpm workspace member of the repository (see the root `pnpm-workspace.yaml`),
so dependencies install once from the root lockfile.

## Working on it

```sh
pnpm install                 # from the repository root
pnpm --filter @nanus/website dev       # dev server at http://localhost:4321
pnpm --filter @nanus/website build     # static build into apps/website/dist
pnpm --filter @nanus/website typecheck # astro check
```

## Content

Pages are Markdown or MDX under `src/content/docs/`, grouped by directory. The sidebar in
`astro.config.mjs` autogenerates one group per directory, ordered by the `sidebar.order`
in each page's frontmatter, so adding a page is enough to add it to the navigation.

`src/content/docs/index.mdx` is the splash landing page.

## HTML and Markdown

Every page is published twice:

- **HTML** at its directory URL - `/getting-started/quickstart/` - rendered by Starlight.
- **Markdown** at the same path with a `.md` suffix - `/getting-started/quickstart.md` -
  produced by `src/pages/[...slug].md.ts`, which serialises each content entry through
  `src/lib/markdown.ts`. The frontmatter is stripped to `title` and `description`; the body
  is the page's raw Markdown.

`src/pages/llms.txt.ts` writes the [llms.txt](https://llmstxt.org) index that points agents
at the Markdown URLs. Both are static files in `apps/website/dist`, so the CloudFront
distribution blogwright manages serves them like any other page.

## Deployment

Deployment is blogwright's job, driven by the repository's workflows:

- `.github/workflows/production.yml` publishes to `nanus.iamstan.dev` on merge to `main`
  (path-filtered) and on every GitHub release, using `config/production.jsonc`.
- `.github/workflows/preview.yml` stands up a per-PR preview at
  `https://pr-<n>.preview.nanus.iamstan.dev` and tears it down when the PR closes, using
  `config/preview.jsonc`.

Both run the site's build inside blogwright's Lambda MicroVM; the GitHub runner only
orchestrates with the `blogwright` CLI.
