// @ts-check
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';

// https://astro.build/config
export default defineConfig({
  // Canonical origin. Used for absolute URLs in the generated sitemap and in the
  // markdown index at /llms.txt. Must match config/production.jsonc's `domain`.
  site: 'https://nanus.iamstan.dev',
  integrations: [
    starlight({
      title: 'nanus',
      description:
        'A coding agent you can take apart: a harness in safe Rust whose kernel, tools, adapters, and session log are plugins over a Cordis kernel.',
      favicon: '/favicon.svg',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/antstanley/nanus' },
      ],
      editLink: {
        baseUrl: 'https://github.com/antstanley/nanus/edit/main/apps/website/',
      },
      sidebar: [
        // Each group autogenerates from its directory, ordered by the `sidebar.order`
        // in each page's frontmatter. Adding a page under the directory is enough.
        { label: 'Getting started', items: [{ autogenerate: { directory: 'getting-started' } }] },
        { label: 'Guides', items: [{ autogenerate: { directory: 'guides' } }] },
        { label: 'Reference', items: [{ autogenerate: { directory: 'reference' } }] },
      ],
    }),
  ],
});
