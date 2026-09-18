import type { APIRoute } from 'astro';
import { getCollection } from 'astro:content';

// https://llmstxt.org - a Markdown index that points agents at the machine-readable
// version of every docs page. Each entry links to the `.md` twin produced by
// src/pages/[...slug].md.ts rather than to the rendered HTML.

// Mirrors the sidebar groups in astro.config.mjs; pages list in sidebar order.
const GROUPS: Array<{ prefix: string; label: string }> = [
  { prefix: 'getting-started/', label: 'Getting started' },
  { prefix: 'guides/', label: 'Guides' },
  { prefix: 'reference/', label: 'Reference' },
];

export const GET: APIRoute = async ({ site }) => {
  const abs = (path: string) => (site ? new URL(path, site).href : path);
  const docs = await getCollection('docs');

  const lines: string[] = [];
  lines.push('# nanus');
  lines.push('');
  lines.push(
    '> A coding agent you can take apart: a harness in safe Rust whose kernel, ' +
      'tools, adapters, and session log are plugins over a Cordis kernel.',
  );
  lines.push('');
  lines.push(
    'Every documentation page is available as Markdown by appending `.md` to its ' +
      `URL path (e.g. ${abs('/getting-started/quickstart.md')}).`,
  );
  lines.push('');

  for (const group of GROUPS) {
    const entries = docs
      .filter((doc) => doc.id.startsWith(group.prefix))
      .sort((a, b) => (a.data.sidebar?.order ?? 99) - (b.data.sidebar?.order ?? 99));
    if (entries.length === 0) continue;
    lines.push(`## ${group.label}`);
    lines.push('');
    for (const doc of entries) {
      const desc = doc.data.description ? `: ${doc.data.description}` : '';
      lines.push(`- [${doc.data.title}](${abs(`/${doc.id}.md`)})${desc}`);
    }
    lines.push('');
  }

  return new Response(lines.join('\n'), {
    headers: {
      'Content-Type': 'text/plain; charset=utf-8',
    },
  });
};
