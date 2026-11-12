# Launches smoothyfiles (optionally at a path), waits, screenshots, reports memory, kills it.
param(
    [string]$Path = "",
    [string]$Out = "shot.png",
    [int]$WaitMs = 1500,
    [string]$Exe = "$PSScriptRoot\..\target\debug\smoothyfiles.exe"
)
$args2 = @(); if ($Path) { $args2 = @("`"$Path`"") }
$sw = [Diagnostics.Stopwatch]::StartNew()
$p = if ($args2.Count) { Start-Process $Exe -ArgumentList $args2 -PassThru } else { Start-Process $Exe -PassThru }
while ($p.MainWindowHandle -eq 0 -and $sw.ElapsedMilliseconds -lt 15000) { Start-Sleep -Milliseconds 10; $p.Refresh() }
"window after $($sw.ElapsedMilliseconds) ms"
Start-Sleep -Milliseconds $WaitMs
& powershell -NoProfile -ExecutionPolicy Bypass -File "$PSScriptRoot\shot.ps1" -Out $Out
$p.Refresh()
"WorkingSet $([math]::Round($p.WorkingSet64/1MB,1)) MB, Private $([math]::Round($p.PrivateMemorySize64/1MB,1)) MB"
Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
