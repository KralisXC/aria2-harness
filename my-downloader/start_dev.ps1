# Increment 23/26 - dev launcher for the two-process model (ASCII-only on purpose).
#
# NOTE: This file must stay pure ASCII. Windows PowerShell 5.1 reads .ps1 files as
# the ANSI codepage (GBK here) unless they carry a UTF-8 BOM. Chinese literals in a
# BOM-less UTF-8 file get mangled and can even swallow the closing quote, which makes
# the parser fail with "The string is missing the terminator". Do not re-add non-ASCII
# characters here unless you also save the file as UTF-8 with BOM.
#
# Starts the whole system in three steps:
#   1) Build both binaries WITH `--features wasm-runtime`. The tao/wry shell no longer
#      inherits that feature from Tauri's dev flow; without it the Wasm plugin backend
#      is compiled out and plugins declaring "engine": "wasm" are rejected at load
#      time ("wasm-runtime feature not enabled").
#   2) Launch aria2-service WITH ADMIN RIGHTS (triggers UAC). It spawns aria2c and
#      provides the SE_MANAGE_VOLUME_NAME privilege required by --file-allocation=falloc.
#   3) Launch the UI with NORMAL rights (vite dev server + the tao/wry shell). The UI
#      never elevates; it only talks to the service over a named pipe.
#
# Usage (from the aria2-harness directory):
#   powershell -ExecutionPolicy Bypass -File .\start_dev.ps1
#
# Prerequisite: Rust toolchain on PATH + node_modules installed (`npm install`).

$ErrorActionPreference = 'Stop'

# Derive every path from the script location; never hardcode absolute paths.
$root = $PSScriptRoot
$host_dir = Join-Path $root 'src-host'
$service = Join-Path $host_dir 'target\debug\aria2-service.exe'
$ui = Join-Path $host_dir 'target\debug\aria2-harness.exe'

# --- Stale-instance guard ---------------------------------------------------
# A stale aria2-service holds the single-instance named pipe. Worse: it was
# started ELEVATED, so a normal-rights Stop-Process is DENIED by the Windows
# integrity check and fails SILENTLY. The old (misconfigured) process then keeps
# serving the pipe while the UI keeps failing with "os error 5", and the new
# service cannot even start (FILE_FLAG_FIRST_PIPE_INSTANCE). So a blind kill is
# not enough: try, VERIFY, escalate under UAC, VERIFY again, and refuse to start
# if the process still survives.
function Clear-StaleService {
    $stale = Get-Process aria2-service -ErrorAction SilentlyContinue
    if (-not $stale) { return $true }

    $pids = ($stale | ForEach-Object { $_.Id }) -join ', '
    Write-Host "[start_dev] stale aria2-service found (PID: $pids)" -ForegroundColor Yellow

    $stale | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
    if (-not (Get-Process aria2-service -ErrorAction SilentlyContinue)) {
        Write-Host "[start_dev] stale service cleared" -ForegroundColor Cyan
        return $true
    }

    Write-Host "[start_dev] normal kill denied (the stale instance runs elevated)." -ForegroundColor Yellow
    Write-Host "[start_dev] escalating: confirm the UAC prompt to kill it..." -ForegroundColor Yellow
    Start-Process -FilePath 'taskkill.exe' -ArgumentList '/F', '/IM', 'aria2-service.exe' -Verb RunAs -Wait
    Start-Sleep -Milliseconds 500
    if (-not (Get-Process aria2-service -ErrorAction SilentlyContinue)) {
        Write-Host "[start_dev] stale service cleared (elevated)" -ForegroundColor Cyan
        return $true
    }

    Write-Host "[start_dev] ERROR: a stale aria2-service is STILL running." -ForegroundColor Red
    Write-Host "[start_dev] It holds the single-instance pipe, so the new service cannot start" -ForegroundColor Red
    Write-Host "[start_dev] and the UI would keep talking to the OLD instance (os error 5)." -ForegroundColor Red
    Write-Host "[start_dev] Clean it up manually from an ADMIN PowerShell:" -ForegroundColor Red
    Write-Host "[start_dev]   taskkill /F /IM aria2-service.exe" -ForegroundColor Red
    return $false
}

# Clear stale service BEFORE building: an elevated aria2-service holds a lock on
# its own exe, so cargo could not overwrite it (Access is denied).
if (-not (Clear-StaleService)) { exit 1 }

# --- Port 1420 guard --------------------------------------------------------
# vite.config.ts uses strictPort, so ANY leaked dev server makes the next launch
# die with "Port 1420 is already in use" and the whole script hangs/aborts.
# Kill whatever LISTENS on 1420 (and its child tree, e.g. esbuild) before starting.
# Deliberately scoped to the PORT OWNER only: a blanket `taskkill /IM node.exe`
# would also kill unrelated Node processes (editors, other projects).
function Clear-Port1420 {
    $owners = @()
    try {
        $owners = Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction Stop |
            Select-Object -ExpandProperty OwningProcess -Unique
    }
    catch {
        $owners = netstat -ano -p tcp |
            Select-String 'LISTENING' |
            Select-String ':1420\s' |
            ForEach-Object { ($_ -split '\s+')[-1] } |
            Sort-Object -Unique
    }

    foreach ($ownerPid in $owners) {
        if (-not $ownerPid -or [int]$ownerPid -le 0) { continue }
        $name = ''
        try { $name = (Get-Process -Id $ownerPid -ErrorAction Stop).ProcessName } catch { }
        Write-Host "[start_dev] port 1420 held by PID $ownerPid ($name) - killing" -ForegroundColor Yellow
        Start-Process -FilePath 'taskkill.exe' `
            -ArgumentList '/PID', $ownerPid, '/T', '/F' `
            -NoNewWindow -Wait -ErrorAction SilentlyContinue | Out-Null
    }

    # Verify the port is actually free; strictPort would fail immediately otherwise.
    for ($i = 0; $i -lt 20; $i++) {
        $still = $null
        try {
            $still = Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction SilentlyContinue
        }
        catch {
            $still = netstat -ano -p tcp | Select-String 'LISTENING' | Select-String ':1420\s'
        }
        if (-not $still) {
            if ($owners.Count -gt 0) {
                Write-Host "[start_dev] port 1420 is free" -ForegroundColor Cyan
            }
            return
        }
        Start-Sleep -Milliseconds 250
    }
    Write-Host "[start_dev] WARNING: port 1420 is still occupied; vite (strictPort) may fail" -ForegroundColor Red
}

# --- Step 1/3: build ---------------------------------------------------------
# NOTE: use Start-Process (not `& cargo ...`) so cargo's stderr progress output
# cannot trip $ErrorActionPreference='Stop'.
Write-Host "[start_dev] [1/3] building binaries (features: wasm-runtime)..." -ForegroundColor Cyan
$build = Start-Process -FilePath 'cargo.exe' `
    -ArgumentList 'build', '--features', 'wasm-runtime', '--bin', 'aria2-service', '--bin', 'aria2-harness' `
    -WorkingDirectory $host_dir -NoNewWindow -Wait -PassThru
if ($build.ExitCode -ne 0) {
    Write-Host "[start_dev] ERROR: cargo build failed (exit $($build.ExitCode))" -ForegroundColor Red
    exit 1
}
foreach ($bin in @($service, $ui)) {
    if (-not (Test-Path $bin)) {
        Write-Host "[start_dev] ERROR: binary missing after build: $bin" -ForegroundColor Red
        exit 1
    }
}

# --- Step 2/3: core service (elevated) ---------------------------------------
Write-Host "[start_dev] [2/3] launching core service with admin rights (confirm the UAC prompt)..." -ForegroundColor Cyan
Start-Process -FilePath $service -Verb RunAs
Start-Sleep -Seconds 2

# --- Step 3/3: UI (vite dev server + shell) ---------------------------------
# Free 1420 right before vite starts: strictPort aborts the whole dev session if
# a previous run leaked a dev server.
Clear-Port1420
Push-Location $root
$vite = $null
try {
    Write-Host "[start_dev] [3/3] launching UI (vite dev server + tao/wry shell)..." -ForegroundColor Cyan
    $vite = Start-Process -FilePath 'npm.cmd' -ArgumentList 'run', 'dev' -PassThru -NoNewWindow

    # --- Wait for Vite instead of a blind Start-Sleep -------------------------
    # Vite's cold start (dependency pre-bundling) can take 20s+, while the old fixed
    # 2s wait launched the wry window too early: WebView2 loaded before the dev server
    # was up, so the user got a blank page and had to refresh manually.
    # Probe BOTH IP stacks. `server.host` is `false` (localhost) in vite.config.ts and
    # on Windows "localhost" may resolve to ::1, so a 127.0.0.1-only loop would spin
    # forever -- that mismatch is exactly why this used to be a fixed sleep.
    $probeUrls = @('http://127.0.0.1:1420', 'http://[::1]:1420')
    $ready = $null
    for ($i = 0; $i -lt 240; $i++) {
        if ($vite.HasExited) {
            Write-Host "[start_dev] ERROR: vite dev server exited early (code $($vite.ExitCode))" -ForegroundColor Red
            exit 1
        }
        foreach ($u in $probeUrls) {
            try {
                $r = Invoke-WebRequest -Uri $u -UseBasicParsing -TimeoutSec 2
                if ($r.StatusCode -eq 200) { $ready = $u; break }
            }
            catch { }
        }
        if ($ready) { break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ready) {
        Write-Host "[start_dev] ERROR: vite did not answer on port 1420 within 120s; refusing to open the UI" -ForegroundColor Red
        exit 1
    }
    Write-Host ("[start_dev] vite ready at $ready (waited ~{0:N1}s)" -f (($i + 1) * 0.5)) -ForegroundColor Cyan

    & $ui
}
finally {
    Pop-Location
    if ($vite) {
        # npm.cmd is the parent of node.exe; /T kills the whole tree so :1420 is freed.
        # vite.config.ts uses strictPort, so a leaked server would break the next run.
        Start-Process -FilePath 'taskkill.exe' -ArgumentList '/PID', $vite.Id, '/T', '/F' -NoNewWindow -Wait -ErrorAction SilentlyContinue | Out-Null
    }
    # On a clean exit the UI tells the service to shut down over the named pipe.
    # This is a safety net in case dev is interrupted with Ctrl-C.
    Get-Process aria2-service -ErrorAction SilentlyContinue | Stop-Process -Force
    Write-Host "[start_dev] core service process cleaned up" -ForegroundColor Cyan
}
