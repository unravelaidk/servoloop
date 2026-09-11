param([string]$Archive, [string]$ChecksumFile, [string]$Prefix = "$HOME\.local\bin")
$ErrorActionPreference = 'Stop'
if (-not $Archive -or -not $ChecksumFile) { throw 'Use -Archive FILE -ChecksumFile SHA256SUMS.' }
$archiveName = [regex]::Escape((Split-Path $Archive -Leaf))
$line = Get-Content -LiteralPath $ChecksumFile | Where-Object { $_ -match ('^[0-9a-fA-F]{64}\s+\*?' + $archiveName + '\s*$') } | Select-Object -First 1
if (-not $line) { throw 'No SHA-256 entry for the archive.' }
$expected = ($line -split '\s+')[0].ToLowerInvariant()
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
if ($expected -ne $actual) { throw 'Checksum verification failed.' }
[Reflection.Assembly]::LoadWithPartialName('System.IO.Compression.FileSystem') | Out-Null
$zip = [IO.Compression.ZipFile]::OpenRead($Archive)
try {
  $allowed = @('bin/', 'bin/servoloop.exe', 'LICENSE', 'NOTICE', 'README')
  foreach ($entry in $zip.Entries) {
    if ($allowed -notcontains $entry.FullName) { throw "Archive contains unexpected path: $($entry.FullName)" }
  }
} finally { $zip.Dispose() }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Expand-Archive -LiteralPath $Archive -DestinationPath $tmp
  $binary = Join-Path $tmp 'bin\servoloop.exe'
  if (-not (Test-Path -LiteralPath $binary)) { throw 'Archive does not contain servoloop.exe.' }
  New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
  $staged = Join-Path $Prefix ('.servoloop.tmp.' + $PID)
  Copy-Item -LiteralPath $binary -Destination $staged
  Move-Item -Force -LiteralPath $staged -Destination (Join-Path $Prefix 'servoloop.exe')
} finally { Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue }
Write-Host "Installed $Prefix\servoloop.exe. Add $Prefix to PATH if needed."
