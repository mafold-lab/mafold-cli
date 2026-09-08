# Install + run mafold (Mafold terminal client + the coding agent you already
# have) on Windows. The Unix twin of install.sh — same layout (~/.mafold), same
# release assets, same next step.
#
#   irm https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.ps1 | iex
#   mafold login
#
# Arguments cannot cross a `| iex` pipe, so `mafold login` is a second command
# rather than `&& mafold login`. To pass args in one line (the install.sh
# behaviour — everything after the script is forwarded to mafold, working dir =
# where you run this):
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.ps1))) agent --detach --token mb_xxxx
#
# ExecutionPolicy is not in the way: this is piped text, never a saved .ps1.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$MafoldArgs)

# Everything runs inside a scriptblock so the settings below ($ErrorActionPreference,
# $ProgressPreference) stay in ITS scope — `iex` executes in the caller's scope,
# and an installer must not repaint the shell it was pasted into. `return` for
# the same reason: a bare `exit` in an iex'd script closes the user's window,
# and a closed window is an error message nobody ever reads.
& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference    = 'SilentlyContinue'   # else Invoke-WebRequest on PS 5.1 spends most of the download drawing a progress bar

  $repo = 'mafold-lab/mafold-cli'

  # Windows PowerShell 5.1 still defaults to TLS 1.0/1.1 on older boxes, and
  # GitHub only serves 1.2+ — without this the download fails as a bare
  # "connection was closed" with nothing to search for.
  try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch {}

  # PROCESSOR_ARCHITECTURE is the *process* arch; a 32-bit PowerShell on a
  # 64-bit OS reports x86 and puts the real one in PROCESSOR_ARCHITEW6432.
  $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  if ($arch -eq 'x86' -and -not [Environment]::Is64BitOperatingSystem) {
    Write-Host "unsupported platform: 32-bit Windows - build from source: https://github.com/$repo" -ForegroundColor Red
    return
  }
  # There is one Windows build (x64). On Windows-on-ARM it runs under the
  # built-in x64 emulation, which is why ARM64 is not an error here.
  $target = 'x86_64-pc-windows-msvc'
  if ($arch -eq 'ARM64') { Write-Host "note: no native arm64 build yet - installing the x64 binary (Windows runs it emulated)" -ForegroundColor DarkGray }

  if (-not (Get-Command claude -ErrorAction SilentlyContinue)) {
    Write-Host "WARNING: Claude Code (claude) not found on PATH - needed for 'agent'. Fix with: npm install -g @anthropic-ai/claude-code  (or https://claude.com/claude-code)" -ForegroundColor Yellow
  }

  # Same home as everywhere else: mafold sets HOME from USERPROFILE at startup
  # (main.rs), so the daemon's pid/log/config and this binary share ~/.mafold.
  $dir = Join-Path $env:USERPROFILE '.mafold'
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $bin = Join-Path $dir 'mafold.exe'

  $base = "https://github.com/$repo/releases/latest/download"
  # `mafold-<triple>.exe` and `mafold-<triple>` are the same bytes (release.yml
  # uploads both); the extension-less one is what the self-updater fetches, and
  # the .sha256 is taken over those same bytes, so it verifies either.
  $url = "$base/mafold-$target.exe"

  Write-Host "downloading mafold ($target)..."
  $tmp    = Join-Path $dir ("mafold.download." + [guid]::NewGuid().ToString('N') + ".exe")
  $tmpSha = "$tmp.sha256"
  try {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $tmp

    # Checksum is MANDATORY, exactly as in the self-updater (update.rs): this
    # binary goes on to run a coding agent with --dangerously-skip-permissions,
    # so a download that cannot be verified is not installed.
    #
    # -OutFile, not `.Content`: GitHub serves release assets as
    # application/octet-stream, and Windows PowerShell 5.1 hands back a byte[]
    # rather than a string for those — the comparison would then never match.
    Invoke-WebRequest -UseBasicParsing -Uri "$base/mafold-$target.sha256" -OutFile $tmpSha
    $want = ((Get-Content -Raw $tmpSha) -replace '\s', '').ToUpper()
    $got  = (Get-FileHash -Algorithm SHA256 -Path $tmp).Hash.ToUpper()
    if (-not $want) { throw "no checksum published for mafold-$target - refusing to install" }
    if ($want -ne $got) { throw "checksum mismatch (expected $want, got $got) - refusing to install" }

    # An .exe that is currently running cannot be overwritten, but it CAN be
    # renamed out of the way — that is how a live daemon survives an install.
    if (Test-Path $bin) {
      $old = "$bin.old"
      Remove-Item -Force $old -ErrorAction SilentlyContinue
      # Still there ⇒ a previous binary is still running and holding it; park
      # this one under its own name instead of failing the install.
      if (Test-Path $old) { $old = "$bin.old-" + (Get-Date -Format 'yyyyMMddHHmmss') }
      Move-Item -Force $bin $old
    }
    Move-Item -Force $tmp $bin
  } catch {
    Write-Host "install failed: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "  manual: download $url and put it somewhere on PATH" -ForegroundColor DarkGray
    return
  } finally {
    Remove-Item -Force $tmp, $tmpSha -ErrorAction SilentlyContinue
  }
  Write-Host "installed -> $bin" -ForegroundColor Green

  # PATH, in both tenses: the User environment so every future shell finds it,
  # and this process so the `mafold login` typed right after this line does.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $dir.TrimEnd('\')) {
    $newPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $dir } else { $userPath.TrimEnd(';') + ';' + $dir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "added $dir to your PATH (new terminals pick it up automatically)" -ForegroundColor DarkGray
  }
  if (($env:PATH -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $dir.TrimEnd('\')) {
    $env:PATH = "$env:PATH;$dir"
  }
  # PowerShell caches command lookups per session, so a PATH edit alone does not
  # always make `mafold` resolve in THIS window. The alias is the guarantee —
  # it points at the same file, and new sessions resolve via PATH.
  Set-Alias -Name mafold -Value $bin -Scope Global -Force

  if ($MafoldArgs -and $MafoldArgs.Count -gt 0) {
    & $bin @MafoldArgs
  } else {
    Write-Host ""
    Write-Host "run:  mafold login                                  # pair this computer"
    Write-Host "      mafold agent --detach --token mb_xxx          # works in the current folder"
  }
}
