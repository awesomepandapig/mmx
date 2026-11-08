import { readFileSync, writeFileSync } from 'node:fs';
const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? '')) throw Error('Invalid release version');
for (const [path, pattern] of [
  ['services/mmx/Cargo.toml', /(^name = "mmx"\nversion = ")[^"]+/m],
  ['Cargo.lock', /(^name = "mmx"\nversion = ")[^"]+/m],
]) {
  const text = readFileSync(path, 'utf8');
  if (!pattern.test(text)) throw Error(`Cannot find mmx package version in ${path}`);
  writeFileSync(path, text.replace(pattern, `$1${version}`));
}
