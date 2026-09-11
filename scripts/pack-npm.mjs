#!/usr/bin/env node
import { mkdtemp, mkdir, cp, writeFile, readFile, rm } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
const run = promisify(execFile);
const [, , binary, version = '0.1.0', target] = process.argv;
const targets = {
  'linux-x64-glibc-2.39': ['servoloop-linux-x64', 'servoloop'],
  'darwin-arm64': ['servoloop-darwin-arm64', 'servoloop'],
  'darwin-x64': ['servoloop-darwin-x64', 'servoloop'],
  'win32-x64': ['servoloop-win32-x64', 'servoloop.exe']
};
const semver = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
if (!binary || !target || !targets[target] || !semver.test(version)) {
  throw new Error('usage: node scripts/pack-npm.mjs BINARY VERSION TARGET (VERSION must be strict semver)');
}
const [name, executable] = targets[target];
const root = resolve(import.meta.dirname, '..');
const source = resolve(binary);
const stage = await mkdtemp(join(tmpdir(), 'servoloop-npm-'));
const npmExecPath = process.env.npm_execpath;
const npmCli = npmExecPath || [
  join(resolve(process.execPath, '..'), 'node_modules/npm/bin/npm-cli.js'),
  join(resolve(process.execPath, '../..'), 'lib/node_modules/npm/bin/npm-cli.js')
].find(existsSync);
const npm = npmCli ? [process.execPath, npmCli] : [process.platform === 'win32' ? 'npm.cmd' : 'npm'];
const pack = (cwd) => run(npm[0], [...npm.slice(1), 'pack', '--ignore-scripts'], { cwd, stdio: 'pipe' });
try {
  const dir = join(stage, name); await mkdir(join(dir, 'bin'), { recursive: true });
  const template = JSON.parse(await readFile(join(root, 'packages', name, 'package.json'), 'utf8'));
  template.version = version; await writeFile(join(dir, 'package.json'), JSON.stringify(template, null, 2) + '\n');
  await cp(source, join(dir, 'bin', executable));
  await cp(join(root, 'LICENSE'), join(dir, 'LICENSE')); await cp(join(root, 'crates/servoloop-providers/NOTICE'), join(dir, 'NOTICE'));
  await pack(dir);
  const launcher = join(stage, 'servoloop'); await mkdir(join(launcher, 'bin'), { recursive: true });
  const launcherPackage = JSON.parse(await readFile(join(root, 'packages/servoloop/package.json'), 'utf8'));
  launcherPackage.version = version;
  for (const dependency of Object.keys(launcherPackage.optionalDependencies)) launcherPackage.optionalDependencies[dependency] = version;
  await writeFile(join(launcher, 'package.json'), JSON.stringify(launcherPackage, null, 2) + '\n');
  await cp(join(root, 'packages/servoloop/bin/servoloop.mjs'), join(launcher, 'bin/servoloop.mjs'));
  await cp(join(root, 'LICENSE'), join(launcher, 'LICENSE')); await cp(join(root, 'crates/servoloop-providers/NOTICE'), join(launcher, 'NOTICE'));
  await pack(launcher);
  const output = join(root, 'dist/npm'); await mkdir(output, { recursive: true });
  await cp(join(dir, `${name}-${version}.tgz`), join(output, `${name}-${version}.tgz`));
  await cp(join(launcher, `servoloop-${version}.tgz`), join(output, `servoloop-${version}.tgz`));
  console.log(output);
} finally {
  await rm(stage, { recursive: true, force: true });
}
