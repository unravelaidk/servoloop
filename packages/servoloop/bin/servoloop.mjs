#!/usr/bin/env node
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import process from 'node:process';

const require = createRequire(import.meta.url);
const platform = process.platform;
const arch = process.arch;
let packageName;
if (platform === 'linux' && arch === 'x64') {
  const glibc = process.report?.getReport?.().header?.glibcVersionRuntime;
  if (!glibc) {
    console.error('ServoLoop requires glibc 2.39 or newer; musl Linux is not supported.');
    process.exit(1);
  }
  const match = /^(\d+)\.(\d+)$/.exec(glibc);
  const major = match ? Number(match[1]) : NaN;
  const minor = match ? Number(match[2]) : NaN;
  if (!match || !Number.isSafeInteger(major) || !Number.isSafeInteger(minor) || major < 2 || (major === 2 && minor < 39)) {
    console.error(`ServoLoop requires glibc 2.39 or newer (detected ${glibc}).`);
    process.exit(1);
  }
  packageName = 'servoloop-linux-x64';
} else if (platform === 'darwin' && arch === 'arm64') packageName = 'servoloop-darwin-arm64';
else if (platform === 'darwin' && arch === 'x64') packageName = 'servoloop-darwin-x64';
else if (platform === 'win32' && arch === 'x64') packageName = 'servoloop-win32-x64';
else {
  console.error(`ServoLoop does not support ${platform}/${arch}. Supported platforms are Linux x64 (glibc 2.39+), macOS arm64/x64, and Windows x64.`);
  process.exit(1);
}

let binary;
try { binary = require.resolve(`${packageName}/bin/servoloop${platform === 'win32' ? '.exe' : ''}`); }
catch {
  console.error(`ServoLoop's native package (${packageName}) is missing. Reinstall with optional dependencies enabled: pnpm add --global servoloop --force`);
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit', windowsHide: false });
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.once(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}
child.once('error', (error) => {
  console.error(`Unable to start ServoLoop: ${error.message}`);
  process.exit(1);
});
child.once('close', (code, signal) => {
  if (signal && process.platform !== 'win32') {
    const signalCode = { SIGHUP: 129, SIGINT: 130, SIGTERM: 143 }[signal];
    process.exit(signalCode ?? 1);
  }
  process.exit(code ?? 1);
});
