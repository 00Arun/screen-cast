// Local e2e harness: serves dist/, the test pages and the cast API on one origin.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createCastServer } from '../../dist/server.js';

const root = fileURLToPath(new URL('../..', import.meta.url));
const cast = createCastServer({ path: '/cast' });
const types = { '.js': 'text/javascript', '.html': 'text/html', '.css': 'text/css' };
const server = createServer(async (req, res) => {
  if (await cast.handle(req, res)) return;
  const path = normalize(new URL(req.url, 'http://x').pathname).replace(/^\/+/, '');
  if (!/^(dist|test\/e2e)\//.test(path)) return res.writeHead(404).end();
  try {
    const body = await readFile(join(root, path));
    res.writeHead(200, { 'Content-Type': types[extname(path)] ?? 'application/octet-stream' }).end(body);
  } catch {
    res.writeHead(404).end();
  }
});
server.listen(Number(process.argv[2] ?? 4173), '127.0.0.1', () => console.log('ready'));
