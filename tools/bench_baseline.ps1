<#
.SYNOPSIS
Committed criterion baseline + comparator for the core benches (BENCH-REGRESS-0824 plan step 2).

.DESCRIPTION
Three of this core's own recorded victories were lost without anyone noticing — schedule x2.5,
wide_iter x2.2, fragmented_iter x2.06 — and the entry that should have caught it read "No change",
because criterion compared against a baseline that had ALREADY regressed. A local discipline of
"a 20 % drop blocks the merge" cannot fire when there is nothing to compare with.

So: a baseline that lives in the repository, and a comparator that reads criterion's own
`estimates.json` files (no re-run needed — the numbers are already on disk after `cargo bench`).

  .\tools\bench_baseline.ps1 -Record          # snapshot the CURRENT criterion output, then COMMIT it
  .\tools\bench_baseline.ps1                  # compare the current output against the snapshot
  .\tools\bench_baseline.ps1 -Groups schedule,wide_iter

REFERENCE IMPLEMENTATIONS ARE THE MACHINE CHECK. Every group is measured for apex AND for bevy /
legion, and those move only when the MACHINE moves. So the comparator checks them first: if the
reference numbers drifted past their own tolerance, this run is not evidence about apex — it is
evidence about the machine — and the comparison is refused rather than reported as a regression.
That is exactly how the 2026-08-24 session established that the regression was real: bevy's numbers
matched the old records to within noise while apex's did not.

Exit codes: 0 no regression · 1 regression · 2 no baseline · 3 could not compare.

ASCII only (PowerShell 5.1 parses .ps1 as ANSI without a BOM).
#>
[CmdletBinding()]
param(
    [switch]$Record,
    # Run the benches first (a filter string passed to criterion). Omit to compare what is on disk.
    [string]$Run = '',
    [string[]]$Groups = @(),
    # A regression is a median this much slower. 20 % is the core's own merge rule.
    [double]$Tolerance = 0.20,
    # How far a reference implementation may move before this machine is a different machine.
    [double]$ReferenceTolerance = 0.15,
    [string]$BaselinePath = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $BaselinePath) { $BaselinePath = Join-Path $PSScriptRoot 'baselines\core_bench.baseline.json' }
$criterion = Join-Path $repo 'target\criterion'
$commit = (git -C $repo rev-parse HEAD).Trim()

# Which implementations are OURS and which are the reference the machine is judged by.
$OURS = @('apex', 'apex_chunked')

# The numbers this core ONCE MEASURED, lost, and won back (sh_final 2026-07-04; regression found and
# fixed 2026-08-27, BENCH-REGRESS-0824). The targets travel WITH the baseline and are printed beside
# it, because a baseline recorded while a regression is live would quietly make that state the
# standard -- the exact shape of the defect this tool exists to prevent ("No change" compared against
# an already-regressed baseline). The gate blocks a slide; the target says where home is.
$TARGETS = @{
    'schedule/apex'        = 26600.0
    'wide_iter/apex'       = 3490.0
    'fragmented_iter/apex' = 175.0
}

if ($Run) {
    Write-Host "[bench] cargo bench -p apex-bench --bench benchmarks --features `"bevy legion`" -- $Run"
    Push-Location $repo
    try {
        & cmd.exe /d /c "cargo bench -p apex-bench --bench benchmarks --features `"bevy legion`" -- $Run 2>&1" | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "cargo bench failed ($LASTEXITCODE)" }
    } finally { Pop-Location }
}

if (-not (Test-Path -LiteralPath $criterion)) {
    Write-Warning "no criterion output at $criterion -- run the benches first (-Run <filter>)"
    exit 3
}

# ── Read criterion's own estimates ───────────────────────────────────────────────────────────
# `new/estimates.json` is the LAST run; `base/` is what criterion itself compared against, and that
# is precisely the number this tool exists not to trust.
function Read-Estimates {
    $out = [ordered]@{}
    foreach ($g in (Get-ChildItem -LiteralPath $criterion -Directory | Sort-Object Name)) {
        if ($g.Name -eq 'report') { continue }
        if ($Groups.Count -gt 0 -and $Groups -notcontains $g.Name) { continue }
        $impls = [ordered]@{}
        foreach ($i in (Get-ChildItem -LiteralPath $g.FullName -Directory | Sort-Object Name)) {
            if ($i.Name -eq 'report') { continue }
            $est = Join-Path $i.FullName 'new\estimates.json'
            if (-not (Test-Path -LiteralPath $est)) { continue }
            $e = [System.IO.File]::ReadAllText($est) | ConvertFrom-Json
            $impls[$i.Name] = [Math]::Round([double]$e.median.point_estimate, 3)
        }
        if ($impls.Count -gt 0) { $out[$g.Name] = $impls }
    }
    return $out
}

$now = Read-Estimates
if ($now.Count -eq 0) { Write-Warning "criterion has no estimates to read"; exit 3 }

if ($Record) {
    $snapshot = [ordered]@{
        schema = 'apex-core-bench-baseline-v1'
        recorded = (Get-Date).ToUniversalTime().ToString('o')
        git_commit = $commit
        host = [ordered]@{
            os = [System.Environment]::OSVersion.VersionString
            cpu_logical = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors
        }
        unit = 'nanoseconds (criterion median point estimate)'
        reference_impls = @('bevy', 'legion')
        # Recorded victories this core has LOST and is expected to win back (PS.5). Present so a
        # reader of the baseline cannot mistake "what it costs now" for "what it should cost".
        targets_ns = $TARGETS
        targets_note = 'sh_final 2026-07-04; RECOVERED 2026-08-27 (BENCH-REGRESS-0824 closed): the baseline below is the state AFTER the fix, so a slide back to the regression is a regression again'
        groups = $now
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $BaselinePath) | Out-Null
    $snapshot | ConvertTo-Json -Depth 8 | Set-Content -Encoding utf8 $BaselinePath
    Write-Host "baseline written: $BaselinePath  ($($now.Count) groups)"
    Write-Host "COMMIT IT together with the reason it moved."
    exit 0
}

if (-not (Test-Path -LiteralPath $BaselinePath)) {
    Write-Warning "no baseline at $BaselinePath -- record one with -Record (and commit it)"
    exit 2
}
$base = [System.IO.File]::ReadAllText($BaselinePath) | ConvertFrom-Json

# ── The machine check, before any verdict about the code ─────────────────────────────────────
$refDrift = @()
foreach ($g in $base.groups.PSObject.Properties.Name) {
    if ($Groups.Count -gt 0 -and $Groups -notcontains $g) { continue }
    if (-not $now.Contains($g)) { continue }
    foreach ($i in $base.groups.$g.PSObject.Properties.Name) {
        if ($OURS -contains $i) { continue }
        if (-not $now[$g].Contains($i)) { continue }
        $was = [double]$base.groups.$g.$i
        $is = [double]$now[$g][$i]
        if ($was -eq 0) { continue }
        $d = ($is - $was) / $was
        if ([Math]::Abs($d) -gt $ReferenceTolerance) {
            $refDrift += ("{0}/{1}: {2:N1} -> {3:N1} ns ({4:P0})" -f $g, $i, $was, $is, $d)
        }
    }
}

# ── Compare ours ─────────────────────────────────────────────────────────────────────────────
$rows = @()
$failures = @()
$missing = @()
foreach ($g in $base.groups.PSObject.Properties.Name) {
    if ($Groups.Count -gt 0 -and $Groups -notcontains $g) { continue }
    foreach ($i in $base.groups.$g.PSObject.Properties.Name) {
        if ($OURS -notcontains $i) { continue }
        $was = [double]$base.groups.$g.$i
        if (-not $now.Contains($g) -or -not $now[$g].Contains($i)) {
            $missing += "$g/$i (baseline $was ns) was not measured this run"
            continue
        }
        $is = [double]$now[$g][$i]
        $d = $null
        if ($was -ne 0) { $d = ($is - $was) / $was }
        $bad = ($null -ne $d -and $d -gt $Tolerance)
        $rows += [pscustomobject]@{ name = "$g/$i"; baseline = $was; now = $is; delta = $d; bad = $bad }
        if ($bad) { $failures += ("{0}: {1:N1} -> {2:N1} ns ({3:P0} slower)" -f "$g/$i", $was, $is, $d) }
    }
}

Write-Host ""
Write-Host "=== core bench gate ($commit vs baseline $($base.git_commit)) ==="
$fmt = "{0,-28} {1,14} {2,14} {3,9} {4,14} {5}"
Write-Host ($fmt -f 'group/impl', 'baseline ns', 'now ns', 'delta', 'target ns', '')
foreach ($r in ($rows | Sort-Object name)) {
    $d = '-'
    if ($null -ne $r.delta) { $d = "{0:P1}" -f $r.delta }
    $mark = ''
    if ($r.bad) { $mark = 'REGRESSION' }
    # A row with a target that is still far away says so on every run: a green gate here means
    # "no slide since the snapshot", never "this is where it should be".
    $target = '-'
    if ($TARGETS.ContainsKey($r.name)) {
        $t = $TARGETS[$r.name]
        $target = [Math]::Round($t, 1)
        if ($r.now -gt $t * 1.1 -and $mark -eq '') { $mark = ("owed {0:N1}x" -f ($r.now / $t)) }
    }
    Write-Host ($fmt -f $r.name, ([Math]::Round($r.baseline, 1)), ([Math]::Round($r.now, 1)), $d, $target, $mark)
}

if ($refDrift.Count -gt 0) {
    Write-Host ""
    Write-Warning "NOT COMPARABLE: the reference implementations moved, so this machine is not the one the baseline was taken on:"
    foreach ($d in $refDrift) { Write-Warning "  $d" }
    Write-Warning "Nothing about the core follows from the table above. Settle the machine and rerun, or re-record with a reason."
    exit 3
}
foreach ($m in $missing) { Write-Warning $m }
Write-Host ""
if ($failures.Count -gt 0) {
    Write-Host "$($failures.Count) regression(s) past $([int]($Tolerance * 100)) %:" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  $f" -ForegroundColor Red }
    exit 1
}
if ($missing.Count -gt 0) {
    Write-Host "no regression among what was measured, but $($missing.Count) baseline entr(y/ies) had no fresh number" -ForegroundColor Yellow
    exit 3
}
Write-Host "no regression" -ForegroundColor Green
$owed = @($rows | Where-Object { $TARGETS.ContainsKey($_.name) -and $_.now -gt $TARGETS[$_.name] * 1.1 })
if ($owed.Count -gt 0) {
    Write-Host "...but $($owed.Count) group(s) are still short of their recorded numbers (PS.5 / BENCH-REGRESS-0824) -- green here means 'no slide', not 'arrived'." -ForegroundColor Yellow
}
exit 0
