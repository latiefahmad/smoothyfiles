# Hybrid renderer benchmark: switch timings and frame times under GPU / CPU load.
#   pwsh tools\hybrid-bench.ps1 -Scenario timeline             # auto: GPU stress start/stop -> switch timings
#   pwsh tools\hybrid-bench.ps1 -Scenario frames -Mode gpu -Load gpu   # frame times + PgDn wall time
#   -Load: idle | gpu (gpustress 100%) | cpu (all cores, below-normal priority busy loops)
#   -Scenario spike: GPU 100% for 1 s only, must NOT switch
param(
  [ValidateSet("timeline", "frames", "spike")] [string]$Scenario = "frames",
  [ValidateSet("auto", "gpu", "cpu")] [string]$Mode = "auto",
  [ValidateSet("idle", "gpu", "cpu")] [string]$Load = "idle",
  [string]$Path = "C:\Windows\System32",
  [int]$Presses = 40,
  [int]$StressSeconds = 20,
  [double]$ChunkMs = 4,
  [string]$OutDir = "$env:TEMP\hybrid-bench"
)
. "$PSScriptRoot\ui.ps1"
$root = Split-Path $PSScriptRoot -Parent
$script:RFExe = "$root\target\release\smoothyfiles.exe"
$gpustress = "$root\target\release\gpustress.exe"
New-Item -ItemType Directory -Force $OutDir | Out-Null
if ($Scenario -eq "frames") { $StressSeconds = 40 }
$tag = "$Scenario-$Mode-$Load-c$ChunkMs"
$log = "$OutDir\$tag.log"; $raw = "$OutDir\$tag.raw.csv"
Remove-Item $log, $raw -ErrorAction SilentlyContinue

function Pct($sorted, [double]$p) { if ($sorted.Count -eq 0) { return $null }; $sorted[[int][Math]::Round(($sorted.Count - 1) * $p)] }

function Start-App {
  $env:SMOOTHYFILES_RENDERER = $Mode
  $env:SMOOTHYFILES_FRAME_LOG = $raw
  $env:RUST_LOG = "info,gpui_windows=debug"
  $script:RF = Start-Process $script:RFExe -ArgumentList "`"$Path`"" -PassThru -RedirectStandardError $log
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while ($script:RF.MainWindowHandle -eq 0 -and $sw.ElapsedMilliseconds -lt 15000) { Start-Sleep -Milliseconds 20; $script:RF.Refresh() }
  if ($script:RF.HasExited) { throw "smoothyfiles exited immediately (single instance forwarded to another running instance?)" }
  Start-Sleep -Milliseconds 700
  Focus-RF
}

$cpuJobs = @()
function Start-Load {
  if ($Load -eq "gpu") {
    $script:GS = Start-Process $gpustress -ArgumentList "--seconds $StressSeconds --intensity 100 --chunk-ms $ChunkMs" -PassThru -WindowStyle Hidden -RedirectStandardOutput "$OutDir\$tag.gpustress.txt"
  } elseif ($Load -eq "cpu") {
    $n = [Environment]::ProcessorCount
    for ($i = 0; $i -lt $n; $i++) {
      $p = Start-Process powershell -ArgumentList "-NoProfile -Command `"`$e=(Get-Date).AddSeconds($StressSeconds); `$x=0; while((Get-Date) -lt `$e){ for(`$k=0;`$k -lt 200000;`$k++){`$x+=`$k} }`"" -PassThru -WindowStyle Hidden
      $p.PriorityClass = "BelowNormal"; $script:cpuJobs += $p
    }
  }
}
function Stop-Load {
  if ($script:GS -and -not $script:GS.HasExited) { Stop-Process -Id $script:GS.Id -Force -ErrorAction SilentlyContinue }
  foreach ($p in $script:cpuJobs) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
}

# Reads new log lines every 100 ms for $ms, prints interesting ones with elapsed seconds.
function Watch-Log([int]$ms, [Diagnostics.Stopwatch]$clock, [ref]$events) {
  if (-not $script:lr) {
    $fs = [IO.File]::Open($log, "Open", "Read", "ReadWrite"); $script:lr = New-Object IO.StreamReader $fs
  }
  $end = $clock.ElapsedMilliseconds + $ms
  while ($clock.ElapsedMilliseconds -lt $end) {
    while (($line = $script:lr.ReadLine()) -ne $null) {
      if ($line -match "switch requested|pre-warmed|renderer load|mode|fast start|frame-stats|could not") {
        $e = "{0,6:N1}s  {1}" -f ($clock.ElapsedMilliseconds / 1000), $line
        $events.Value += $e
        if ($line -notmatch "frame-stats") { Write-Host $e }
      }
    }
    Start-Sleep -Milliseconds 100
  }
}

try {
  if (Get-Process smoothyfiles -ErrorAction SilentlyContinue) { throw "another smoothyfiles is already running; refusing to start (single instance)" }
  Start-App
  $clock = [Diagnostics.Stopwatch]::StartNew()
  $events = @()
  if ($Scenario -eq "timeline") {
    Watch-Log 8000 $clock ([ref]$events)
    Write-Host "== stress start at $($clock.Elapsed.TotalSeconds) s"; $Load = "gpu"; Start-Load
    $tStart = $clock.Elapsed.TotalSeconds
    Watch-Log (($StressSeconds + 16) * 1000) $clock ([ref]$events)
    Write-Host "== stress ended at about $($tStart + $StressSeconds) s"
    $events | Set-Content "$OutDir\$tag.events.txt"
  }
  elseif ($Scenario -eq "spike") {
    Watch-Log 8000 $clock ([ref]$events)
    $StressSeconds = 1; $Load = "gpu"; Write-Host "== 1 s spike at $($clock.Elapsed.TotalSeconds) s"; Start-Load
    Watch-Log 14000 $clock ([ref]$events)
    $events | Set-Content "$OutDir\$tag.events.txt"
  }
  else {
    Watch-Log 4000 $clock ([ref]$events)
    Start-Load
    Watch-Log 9000 $clock ([ref]$events)   # let auto mode settle under load
    Focus-RF
    $l0 = if (Test-Path $raw) { @(Get-Content $raw).Count } else { 0 }
    Start-Sleep -Milliseconds 2600   # frame log flushes every 2 s; fence the window
    $l0 = if (Test-Path $raw) { @(Get-Content $raw).Count } else { 0 }
    $press = New-Object System.Collections.Generic.List[double]
    $total = [Diagnostics.Stopwatch]::StartNew()
    foreach ($dir in "{PGDN}", "{PGUP}") {
      for ($i = 0; $i -lt $Presses; $i++) {
        $t = [Diagnostics.Stopwatch]::StartNew()
        [System.Windows.Forms.SendKeys]::SendWait($dir)
        $press.Add($t.Elapsed.TotalMilliseconds)
        Start-Sleep -Milliseconds 40
      }
    }
    $wall = $total.Elapsed.TotalMilliseconds
    Start-Sleep -Milliseconds 2600
    $lines = if (Test-Path $raw) { @(Get-Content $raw) } else { @() }
    $win = if ($lines.Count -gt $l0) { $lines[$l0..($lines.Count - 1)] } else { @() }
    $us = @($win | ForEach-Object { [double]($_.Split(",")[1]) } | Sort-Object)
    $backs = ($win | ForEach-Object { $_.Split(",")[0] } | Group-Object | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join " "
    $ps = @($press | Sort-Object)
    $res = [ordered]@{
      mode = $Mode; load = $Load; frames = $us.Count; backends = $backs
      frame_p50_ms = [Math]::Round((Pct $us 0.5) / 1000, 2); frame_p95_ms = [Math]::Round((Pct $us 0.95) / 1000, 2)
      frame_p99_ms = [Math]::Round((Pct $us 0.99) / 1000, 2); frame_max_ms = [Math]::Round((Pct $us 1.0) / 1000, 2)
      presses = $ps.Count; wall_ms = [Math]::Round($wall)
      press_p50_ms = [Math]::Round((Pct $ps 0.5), 1); press_p95_ms = [Math]::Round((Pct $ps 0.95), 1); press_max_ms = [Math]::Round((Pct $ps 1.0), 1)
    }
    $res | ConvertTo-Json -Compress | Tee-Object "$OutDir\$tag.result.json"
  }
}
finally {
  Stop-Load
  if ($script:lr) { $script:lr.Dispose() }
  Stop-RF
}
