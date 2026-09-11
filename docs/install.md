# Install ServoLoop CLI

This page describes the currently supported binary archives and local
installation paths. The packaging workflow uploads artifacts for inspection;
it does not publish releases, so there is no public download URL yet.

## Supported binary targets

The workflow builds and smoke-tests these native targets:

- Linux x86_64, built on Ubuntu 24.04 with glibc 2.39.
- Windows x86_64.
- macOS arm64.
- macOS x86_64.

Checksums verify integrity, not authenticity. Obtain an archive and its
`SHA256SUMS` file from the same trusted workflow run over HTTPS and verify its
provenance before installing. macOS archives are unsigned; macOS may show a
Gatekeeper quarantine warning. Review source and release provenance rather
than applying a blanket quarantine bypass.

Each archive has the same layout: `bin/servoloop` (or
`bin/servoloop.exe`), `LICENSE`, `NOTICE`, and a minimal `README`.

## Install a downloaded archive on Unix

Download an archive and `SHA256SUMS` from a workflow artifact, inspect them,
then run the installer locally. It verifies the checksum before extraction:

```bash
bash scripts/install.sh \
  --archive ./servoloop-VERSION-macos-arm64.tar.gz \
  --checksum-file ./SHA256SUMS
```

The default destination is `~/.local/bin`. Set `--prefix` for another
user-writable directory. The installer never edits shell startup files; add
the destination to `PATH` yourself. It does not use `sudo`.

Download mode requires an explicit `--version` and `SERVOLOOP_RELEASE_URL`,
but no release host exists yet. Prefer downloading, inspecting, and then
running the script over piping a remote script directly to a shell.

## Install on Windows

Use PowerShell after downloading and inspecting the Windows ZIP and checksum:

```powershell
.\scripts\install.ps1 -Archive .\servoloop-VERSION-windows-x86_64.zip `
  -ChecksumFile .\SHA256SUMS
```

The default destination is `$HOME\.local\bin`. Use `-Prefix` for another
user-writable directory. The script verifies the archive before extraction and
does not modify `PATH`.

## Verify an installation

Run these commands after adding the destination to `PATH`:

```bash
servoloop --version
servoloop --help
servoloop run --demo --store "$HOME/.local/state/servoloop-demo"
```

The demo emits JSON events. An explicit store path prevents a repository-local
`.servoloop` directory.
