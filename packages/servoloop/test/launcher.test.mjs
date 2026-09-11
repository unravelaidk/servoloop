import test, { after, before } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';

const packageDir = dirname(dirname(fileURLToPath(import.meta.url)));
const launcher = join(packageDir, 'bin/servoloop.mjs');
let fixture;
let native;


function run(args = []) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [launcher, ...args], { stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', reject);
    child.once('close', (code, signal) => resolve({ code, signal, stdout, stderr, child }));
  });
}

before(async () => {
  fixture = await mkdtemp(join(tmpdir(), 'servoloop-launcher-'));
  await mkdir(join(fixture, 'bin'), { recursive: true });
  native = join(fixture, 'bin/servoloop');
  await writeFile(join(fixture, 'package.json'), JSON.stringify({ name: 'servoloop-linux-x64', version: '0.1.0' }));
  await writeFile(native, `#!/usr/bin/env node
const args = process.argv.slice(2);
if (args[0] === 'signal-wait') { process.stdout.write('ready\\n'); setInterval(() => {}, 1000); }
else if (args[0] === 'fail') process.exit(Number(args[1]));
else { process.stdout.write(JSON.stringify(args)); process.stderr.write('fixture-stderr'); }
`);
  await chmod(native, 0o755);
  await mkdir(join(packageDir, 'node_modules'), { recursive: true });
  await symlink(fixture, join(packageDir, 'node_modules/servoloop-linux-x64'));
});

after(async () => {
  await rm(join(packageDir, 'node_modules/servoloop-linux-x64'), { force: true });
  await rm(fixture, { recursive: true, force: true });
});

test('passes arguments, unicode, and streams through unchanged', async () => {
  const result = await run(['path with spaces', '雪', 'quote"']);
  assert.equal(result.code, 0);
  assert.equal(result.stdout, JSON.stringify(['path with spaces', '雪', 'quote"']));
  assert.equal(result.stderr, 'fixture-stderr');
});

test('preserves native nonzero exit status', async () => {
  assert.equal((await run(['fail', '37'])).code, 37);
});

for (const [signal, code] of [['SIGTERM', 143], ['SIGINT', 130], ['SIGHUP', 129]]) {
  test(`forwards ${signal} and maps its POSIX exit status`, async () => {
    const child = spawn(process.execPath, [launcher, 'signal-wait'], { stdio: ['ignore', 'pipe', 'ignore'] });
    await new Promise((resolve, reject) => { child.once('spawn', resolve); child.once('error', reject); });
    await new Promise((resolve, reject) => { child.stdout.once('data', resolve); child.once('error', reject); });
    child.kill(signal);
    const result = await new Promise((resolve) => child.once('close', (exitCode, exitSignal) => resolve({ exitCode, exitSignal })));
    assert.equal(result.exitCode, code);
    assert.equal(result.exitSignal, null);
  });
}

test('reports a missing optional package with a global reinstall command', async () => {
  await rm(join(packageDir, 'node_modules/servoloop-linux-x64'), { recursive: true, force: true });
  const result = await run(['--version']);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /pnpm add --global servoloop --force/);
  await symlink(fixture, join(packageDir, 'node_modules/servoloop-linux-x64'));
});
