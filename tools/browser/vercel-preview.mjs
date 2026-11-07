// Local compatibility check: serve the built routes through Vercel's actual Node handler.
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const manifest = JSON.parse(await readFile(resolve(root, 'web/.vercel/remix-build-result.json'), 'utf8'));
const { createRequestHandler } = await import(pathToFileURL(resolve(root, 'web/node_modules/@vercel/remix/server.js')));
const bundles = Object.values(manifest.buildManifest.serverBundles);
if (bundles.length !== 1) throw new Error('Update this check for multiple server bundles');
const build = await import(pathToFileURL(resolve(root, 'web', bundles[0].file)));
const handler = createRequestHandler({ build, mode: 'production' });
const assets = resolve(root, 'web/build/client');
const mime = { '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.ico': 'image/x-icon', '.png': 'image/png', '.woff2': 'font/woff2', '.json': 'application/json' };
createServer(async (req, res) => {
  try {
    const file = resolve(assets, '.' + decodeURIComponent(new URL(req.url, 'http://localhost').pathname));
    if (file.startsWith(assets + sep) && await stat(file).then(s => s.isFile(), () => false)) {
      res.writeHead(200, { 'Content-Type': mime[extname(file)] ?? 'application/octet-stream' });
      res.end(await readFile(file));
      return;
    }
    req.headers['x-forwarded-proto'] = 'http';
    await handler(req, res);
  } catch (error) {
    console.error(error);
    if (!res.headersSent) res.writeHead(500);
    res.end();
  }
}).listen(Number(process.env.PORT ?? 5195), '127.0.0.1');
