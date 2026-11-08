#!/usr/bin/env node
// Stage immutable release inputs. Input is image-name<TAB>sha256 digest per line.
import { cpSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const [version, registry, digestFile, output] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? '')) throw Error('Expected a SemVer release version without v');
if (!/^ghcr\.io\/[a-z0-9_.-]+\/mmx$/.test(registry ?? '')) throw Error('Expected lowercase GHCR owner/mmx registry');
if (!digestFile || !output) throw Error('Usage: prepare.mjs VERSION REGISTRY DIGESTS_TSV OUTPUT');
const own = ['node', 'matching-engine', 'api', 'mmx', 'outbox', 'websocket', 'market-data', 'trade-reporter'];
const upstream = {
  nats: 'docker.io/library/nats:2-alpine',
  valkey: 'docker.io/valkey/valkey:8-alpine',
  questdb: 'docker.io/questdb/questdb:8.2.3',
  tests: 'docker.io/curlimages/curl:8.11.1',
};
const digests = Object.fromEntries(readFileSync(digestFile, 'utf8').trim().split('\n').map(line => {
  const [name, digest, extra] = line.split('\t');
  if (!name || extra || !/^sha256:[a-f0-9]{64}$/.test(digest ?? '')) throw Error(`Invalid digest record: ${name}`);
  return [name, digest];
}));
for (const name of [...own, ...Object.keys(upstream)]) if (!digests[name]) throw Error(`Missing digest: ${name}`);
const images = Object.fromEntries(own.map(name => [name, `${registry}/${name}@${digests[name]}`]));
for (const [name, ref] of Object.entries(upstream)) images[name] = `${ref.slice(0, ref.lastIndexOf(':'))}@${digests[name]}`;
mkdirSync(output, { recursive: true });
const chart = resolve(output, 'chart');
cpSync('deploy/helm/mmx', chart, { recursive: true });
let values = readFileSync(`${chart}/values.yaml`, 'utf8');
values = values.replace(/^image:\n[\s\S]*?(?=^imagePullSecrets:)/m, `image:\n  registry: ${registry}\n  tag: ${JSON.stringify(version)}\n  pullPolicy: IfNotPresent\n  digests:\n${own.map(name => `    ${name}: ${digests[name]}`).join('\n')}\n\n`);
for (const [name, ref] of Object.entries(upstream)) values = values.replace(`image: ${ref}`, `image: ${images[name]}`);
writeFileSync(`${chart}/values.yaml`, values);
let metadata = readFileSync(`${chart}/Chart.yaml`, 'utf8');
metadata = metadata.replace(/^version:.*$/m, `version: ${version}`).replace(/^appVersion:.*$/m, `appVersion: ${JSON.stringify(version)}`);
writeFileSync(`${chart}/Chart.yaml`, metadata);
let compose = readFileSync('deploy/compose.yaml', 'utf8');
for (const name of own) {
  const source = '${MMX_REGISTRY:-ghcr.io/awesomepandapig/mmx}/' + name + ':${MMX_VERSION:-0.1.0}';
  if (!compose.includes(source)) throw Error(`Compose source image changed: ${name}`);
  compose = compose.replaceAll(source, images[name]);
}
for (const [name, ref] of Object.entries(upstream)) compose = compose.replaceAll(ref, images[name]);
writeFileSync(`${output}/compose.yaml`, compose);
writeFileSync(`${output}/image-digests.json`, JSON.stringify({ version, registry, images }, null, 2) + '\n');
