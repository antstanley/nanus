import type { APIRoute, GetStaticPaths } from 'astro';
import { getCollection } from 'astro:content';
import { docToMarkdown } from '../lib/markdown';

// Serve every docs page as raw Markdown at `/<slug>.md` (e.g.
// /getting-started/quickstart.md) for agents and plain-text readers. Starlight
// renders the HTML at `/<slug>/`; this endpoint writes the `.md` sibling into the
// same build output, so both versions deploy together.
export const getStaticPaths: GetStaticPaths = async () => {
  const docs = await getCollection('docs');
  return docs.map((doc) => ({
    params: { slug: doc.id },
    props: { doc },
  }));
};

export const GET: APIRoute = ({ props }) => {
  return new Response(docToMarkdown(props.doc), {
    headers: {
      'Content-Type': 'text/markdown; charset=utf-8',
    },
  });
};
