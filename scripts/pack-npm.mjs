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

function readU32(data, offset, littleEndian) {
  return littleEndian ? data.readUInt32LE(offset) : data.readUInt32BE(offset);
}

function validateBinaryHeader(data, target) {
  const isElf = data.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]));
  const isMachOLittle = data.readUInt32LE(0) === 0xfeedfacf;
  const isMachOBig = data.readUInt32BE(0) === 0xfeedfacf;
  const isPe = data.subarray(0, 2).toString() === 'MZ' && data.length >= 0x40;

  if (target === 'linux-x64-glibc-2.39') {
    if (!isElf || data[4] !== 2 || data[5] !== 1 || data.readUInt16LE(18) !== 0x3e) {
      throw new Error('binary is not a 64-bit little-endian x86-64 ELF executable');
    }
    return;
  }
  if (target === 'darwin-arm64' || target === 'darwin-x64') {
    const little = isMachOLittle;
    const big = isMachOBig;
    if (!little && !big) {
      throw new Error('binary is not a 64-bit Mach-O executable');
    }
    const cpu = readU32(data, 4, little);
    const expected = target === 'darwin-arm64' ? 0x0100000c : 0x01000007;
    if (cpu !== expected) throw new Error(`Mach-O CPU does not match ${target}`);
    return;
  }
  if (target === 'win32-x64') {
    const peOffset = isPe ? data.readUInt32LE(0x3c) : -1;
    if (!isPe || peOffset < 0 || peOffset + 6 > data.length || data.subarray(peOffset, peOffset + 4).toString() !== 'PE\0\0' || data.readUInt16LE(peOffset + 4) !== 0x8664) {
      throw new Error('binary is not a PE x64 executable');
    }
    return;
  }
}

async function validateBinary() {
  const header = await readFile(source, { encoding: null });
  validateBinaryHeader(header, target);

  const workspace = await readFile(join(root, 'Cargo.toml'), 'utf8');
  const workspaceVersion = workspace.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!workspaceVersion || version !== workspaceVersion) {
    throw new Error(`package version ${version} must match Cargo workspace version ${workspaceVersion || '(missing)'}`);
  }

  const hostTarget = process.platform === 'linux' && process.arch === 'x64' ? 'linux-x64-glibc-2.39'
    : process.platform === 'darwin' && process.arch === 'arm64' ? 'darwin-arm64'
      : process.platform === 'darwin' && process.arch === 'x64' ? 'darwin-x64'
        : process.platform === 'win32' && process.arch === 'x64' ? 'win32-x64' : null;
  // Cross-target binaries must never be executed by the packager.
  if (hostTarget === target) {
    try {
      const result = await run(source, ['--version'], { stdio: 'pipe', timeout: 10_000 });
      if (`${result.stdout}${result.stderr}`.trim() !== `servoloop ${version}`) {
        throw new Error(`binary --version did not report exactly "servoloop ${version}"`);
      }
    } catch (error) {
      throw new Error(`native binary version check failed: ${error.message}`);
    }
  }
}

await validateBinary();
const stage = await mkdtemp(join(tmpdir(), 'servoloop-npm-'));
const npmExecPath = /(?:^|[/\\])npm-cli\.js$/.test(process.env.npm_execpath || '')
  ? process.env.npm_execpath : undefined;
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
  await cp(join(root, 'LICENSE'), join(dir, 'LICENSE')); await cp(join(root, 'NOTICE'), join(dir, 'NOTICE'));
  await pack(dir);
  const launcher = join(stage, 'servoloop'); await mkdir(join(launcher, 'bin'), { recursive: true });
  const launcherPackage = JSON.parse(await readFile(join(root, 'packages/servoloop/package.json'), 'utf8'));
  launcherPackage.version = version;
  for (const dependency of Object.keys(launcherPackage.optionalDependencies)) launcherPackage.optionalDependencies[dependency] = version;
  await writeFile(join(launcher, 'package.json'), JSON.stringify(launcherPackage, null, 2) + '\n');
  await cp(join(root, 'packages/servoloop/bin/servoloop.mjs'), join(launcher, 'bin/servoloop.mjs'));
  await cp(join(root, 'LICENSE'), join(launcher, 'LICENSE')); await cp(join(root, 'NOTICE'), join(launcher, 'NOTICE'));
  await pack(launcher);
  const output = join(root, 'dist/npm'); await mkdir(output, { recursive: true });
  await cp(join(dir, `${name}-${version}.tgz`), join(output, `${name}-${version}.tgz`));
  await cp(join(launcher, `servoloop-${version}.tgz`), join(output, `servoloop-${version}.tgz`));
  console.log(output);
} finally {
  await rm(stage, { recursive: true, force: true });
}
