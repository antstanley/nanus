import type { CollectionEntry } from 'astro:content';

/**
 * Serialize a docs entry to a standalone Markdown document: its YAML frontmatter
 * (title and description only) followed by the page's raw Markdown body.
 *
 * The build exports one of these per page at `/<slug>.md` (see
 * `src/pages/[...slug].md.ts`), so every HTML page has a Markdown twin that any
 * static host - including the CloudFront distribution blogwright manages - serves
 * as-is. `/llms.txt` indexes them.
 */
export function docToMarkdown(doc: CollectionEntry<'docs'>): string {
  const { title, description } = doc.data;

  const frontmatter = [
    '---',
    `title: ${JSON.stringify(title)}`,
    ...(description ? [`description: ${JSON.stringify(description)}`] : []),
    '---',
  ].join('\n');

  const body = (doc.body ?? '').trim();

  return `${frontmatter}\n\n${body}\n`;
}
