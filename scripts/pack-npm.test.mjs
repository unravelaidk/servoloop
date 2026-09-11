import test from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';

const root = new URL('..', import.meta.url).pathname;
const packer = join(root, 'scripts/pack-npm.mjs');
const dist = join(root, 'dist/npm');

function invoke(binary, version = '0.1.0', target = 'linux-x64-glibc-2.39') {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [packer, binary, version, target], { cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('close', (code) => resolve({ code, stdout, stderr }));
  });
}

test('rejects a shell fixture before creating package artifacts', async () => {
  const temp = await mkdtemp(join(tmpdir(), 'servoloop-pack-test-'));
  const fake = join(temp, 'servoloop');
  await writeFile(fake, '#!/bin/sh\nprintf native\n');
  await chmod(fake, 0o755);
  await rm(dist, { recursive: true, force: true });
  try {
    const result = await invoke(fake);
    assert.notEqual(result.code, 0);
    assert.match(`${result.stdout}${result.stderr}`, /ELF|executable/);
    assert.equal(existsSync(dist), false);
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
});

test('rejects a valid ELF header with the wrong architecture', async () => {
  const temp = await mkdtemp(join(tmpdir(), 'servoloop-pack-test-'));
  const fake = join(temp, 'wrong-arch');
  const elf = Buffer.alloc(64);
  elf.set([0x7f, 0x45, 0x4c, 0x46, 2, 1], 0);
  elf.writeUInt16LE(0xb7, 18); // AArch64, not x86-64.
  await writeFile(fake, elf);
  try {
    const result = await invoke(fake);
    assert.notEqual(result.code, 0);
    assert.match(`${result.stdout}${result.stderr}`, /x86-64/);
  } finally {
    await rm(temp, { recursive: true, force: true });
  }
});

test('packs the real Rust Linux release binary', async () => {
  const binary = join(root, 'target/release/servoloop');
  assert.equal(existsSync(binary), true, 'build target/release/servoloop before running this test');
  await rm(dist, { recursive: true, force: true });
  try {
    const result = await invoke(binary);
    assert.equal(result.code, 0, result.stderr);
    assert.equal(existsSync(join(dist, 'servoloop-linux-x64-0.1.0.tgz')), true);
    assert.equal(existsSync(join(dist, 'servoloop-0.1.0.tgz')), true);
  } finally {
    await rm(dist, { recursive: true, force: true });
  }
});
