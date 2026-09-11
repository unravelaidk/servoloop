#!/usr/bin/env node
import { mkdtemp, mkdir, readFile, writeFile, rm, cp } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
const run = promisify(execFile);
const [, , binary, version = '0.1.0', target] = process.argv;
if (!binary || !target) throw new Error('usage: node scripts/test-npm-install.mjs BINARY VERSION TARGET');
const root = resolve(import.meta.dirname, '..');
const tmp = await mkdtemp(join(tmpdir(), 'servoloop-pnpm-smoke-'));
const npmExecPath = process.env.npm_execpath;
const npm = npmExecPath || [
  join(resolve(process.execPath, '..'), 'node_modules/npm/bin/npm-cli.js'),
  join(resolve(process.execPath, '../..'), 'lib/node_modules/npm/bin/npm-cli.js')
].find(existsSync);
const npmCommand = npm ? [process.execPath, npm] : [process.platform === 'win32' ? 'npm.cmd' : 'npm'];
const pnpm = process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm';
const targets = { 'linux-x64-glibc-2.39': 'servoloop-linux-x64', 'darwin-arm64': 'servoloop-darwin-arm64', 'darwin-x64': 'servoloop-darwin-x64', 'win32-x64': 'servoloop-win32-x64' };
const nativeName = targets[target];
if (!nativeName) throw new Error(`unsupported npm target: ${target}`);
try {
  await run(npmCommand[0], [...npmCommand.slice(1), 'run', 'pack:npm', '--', binary, version, target], { cwd: root });
  const dist = join(root, 'dist/npm');
  const launcherTarball = join(dist, `servoloop-${version}.tgz`);
  const nativeTarball = join(dist, `${nativeName}-${version}.tgz`);
  const fixture = join(tmp, 'fixture'); await mkdir(join(fixture, 'bin'), { recursive: true });
  const launcher = JSON.parse(await readFile(join(root, 'packages/servoloop/package.json'), 'utf8'));
  launcher.version = version;
  launcher.optionalDependencies = { [nativeName]: `file:${nativeTarball}` };
  await writeFile(join(fixture, 'package.json'), JSON.stringify(launcher, null, 2) + '\n');
  await cp(join(root, 'packages/servoloop/bin/servoloop.mjs'), join(fixture, 'bin/servoloop.mjs'));
  const packed = (await run(npmCommand[0], [...npmCommand.slice(1), 'pack', '--ignore-scripts', '--pack-destination', fixture], { cwd: fixture })).stdout.trim().split(/\r?\n/).pop();
  const home = join(tmp, 'home'); const store = join(tmp, 'store'); const globalDir = join(tmp, 'global'); const bin = join(tmp, 'bin');
  await mkdir(bin, { recursive: true });
  const env = { ...process.env, HOME: home, PNPM_HOME: join(tmp, 'pnpm-home'), PATH: `${bin}${process.platform === 'win32' ? ';' : ':'}${process.env.PATH || ''}` };
  await run(pnpm, ['add', '--global', '--ignore-scripts', '--offline', '--store-dir', store, '--global-dir', globalDir, '--global-bin-dir', bin, join(fixture, packed)], { cwd: root, env });
  const command = process.platform === 'win32' ? join(bin, 'servoloop.cmd') : join(bin, 'servoloop');
  await run(command, ['--version'], { env });
  await run(command, ['--help'], { env });
  await run(command, ['run', '--demo', '--store', join(tmp, 'demo-store')], { env, stdio: 'pipe' });
  try { await run(command, ['invalid-subcommand'], { env }); throw new Error('invalid arguments unexpectedly succeeded'); } catch (error) { if (error.message.includes('unexpectedly succeeded')) throw error; }
  console.log('pnpm npm-package smoke test passed');
} finally { await rm(tmp, { recursive: true, force: true }); }
