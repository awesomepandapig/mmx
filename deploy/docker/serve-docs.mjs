import { createServer } from 'node:http';
import { stat, readFile } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';

// Serve Vocs full-static output, including extensionless pages and Markdown exports.
const root = resolve(process.env.DOCS_ROOT ?? 'public');
const types = {
  '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8', '.json': 'application/json',
  '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon',
  '.woff2': 'font/woff2', '.txt': 'text/plain; charset=utf-8',
  '.md': 'text/markdown; charset=utf-8', '.xml': 'application/xml',
};
createServer(async (req, res) => {
  if (!['GET', 'HEAD'].includes(req.method)) {
    res.writeHead(405, { Allow: 'GET, HEAD' }).end();
    return;
  }
  let path;
  try {
    path = resolve(root, '.' + decodeURIComponent(new URL(req.url, 'http://localhost').pathname));
  } catch {
    res.writeHead(400).end();
    return;
  }
  if (path !== root && !path.startsWith(root + sep)) {
    res.writeHead(403).end();
    return;
  }
  for (const candidate of [path, resolve(path, 'index.html')]) {
    try {
      if (!(await stat(candidate)).isFile()) continue;
      const body = await readFile(candidate);
      res.writeHead(200, {
        'Content-Type': types[extname(candidate)] ?? 'application/octet-stream',
        'Content-Length': body.length,
        'X-Content-Type-Options': 'nosniff',
      });
      res.end(req.method === 'HEAD' ? undefined : body);
      return;
    } catch (error) {
      if (!['ENOENT', 'ENOTDIR'].includes(error.code)) {
        console.error('Static document read failed:', error.code);
        res.writeHead(500).end();
        return;
      }
    }
  }
  res.writeHead(404, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(req.method === 'HEAD' ? undefined : await readFile(resolve(root, '404.html')).catch(() => 'Not found'));
}).listen(Number(process.env.PORT ?? 3000), process.env.HOST ?? '0.0.0.0');
