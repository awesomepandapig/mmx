#!/usr/bin/env node
// Verify release manifests using anonymous registry tokens, never local login credentials.
import { readFileSync } from 'node:fs';

const bundle = JSON.parse(readFileSync(process.argv[2] ?? '', 'utf8'));
if (!/^ghcr\.io\/[a-z0-9_.-]+\/mmx$/.test(bundle.registry ?? '')) throw Error('Invalid release registry');
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(bundle.version ?? '')) throw Error('Invalid release version');
const references = [
  ...Object.values(bundle.images).filter(value => value.startsWith(`${bundle.registry}/`)),
  `${bundle.registry}/charts/mmx:${bundle.version}`,
];
const failed = [];
for (const reference of references) {
  const [, name, digest, tag] = /^ghcr\.io\/(.+?)(?:@(sha256:[a-f0-9]{64})|:([^/:]+))$/.exec(reference) ?? [];
  if (!name) throw Error(`Invalid GHCR reference: ${reference}`);
  try {
    const url = new URL('https://ghcr.io/token');
    url.searchParams.set('service', 'ghcr.io');
    url.searchParams.set('scope', `repository:${name}:pull`);
    const auth = await fetch(url, { signal: AbortSignal.timeout(15_000) });
    if (!auth.ok) throw Error(`anonymous token HTTP ${auth.status}`);
    const { token, access_token: accessToken } = await auth.json();
    if (!token && !accessToken) throw Error('anonymous token missing');
    const manifest = await fetch(`https://ghcr.io/v2/${name}/manifests/${digest ?? tag}`, {
      method: 'HEAD',
      headers: {
        Authorization: `Bearer ${token ?? accessToken}`,
        Accept: 'application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json',
      },
      signal: AbortSignal.timeout(15_000),
    });
    if (!manifest.ok) throw Error(`anonymous manifest HTTP ${manifest.status}`);
    console.log(`Public: ${reference}`);
  } catch (error) {
    failed.push(name);
    console.error(`Not publicly readable: ${reference}: ${error.message}`);
  }
}
if (failed.length) {
  console.error(`Make these GHCR packages public in GitHub package settings, then rerun the publish job: ${failed.join(', ')}`);
  process.exitCode = 1;
}
