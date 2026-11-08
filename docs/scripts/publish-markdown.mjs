import { cp, access } from 'node:fs/promises';

// Vocs emits Markdown twins under assets/md. Publish direct /page.md URLs too.
await cp('dist/public/assets/md', 'dist/public', { recursive: true });
if (!process.env.VERCEL) process.exit(0);
// The Vercel adapter copies static output before this post-build step runs.
// Keep the deployed output in sync with the portable static build.
try {
  await access('.vercel/output/config.json');
} catch (error) {
  if (error.code === 'ENOENT') process.exit(0);
  throw error;
}
await cp('dist/public/assets/md', '.vercel/output/static', { recursive: true });
