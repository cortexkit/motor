# Diagnostic-only Windows loader snaps. This uses the in-bundle native debugger,
# not a machine installation of gflags/WinDbg. Only two image flags are changed;
# their exact original values and kinds are saved before any mutation.
function Save-RunnerLoaderState([string]$Path, $State) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $State -Depth 10), [Text.UTF8Encoding]::new($false))
}
function Restore-RunnerLoaderState($Registry, $Entries) {
    $errors = [Collections.Generic.List[string]]::new()
    foreach ($entry in @($Entries)) {
        $key = $null
        try {
            if ($entry.image -notin @('cargo.exe','rustc.exe')) { throw 'Unexpected loader-snaps image name' }
            $path = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\' + $entry.image
            $key = $Registry.OpenSubKey($path, $true)
            if ($entry.value_existed) {
                if ($null -eq $key) { $key = $Registry.CreateSubKey($path) }
                $kind = [Enum]::Parse([Microsoft.Win32.RegistryValueKind], [string]$entry.kind)
                $key.SetValue('GlobalFlag', $entry.original, $kind)
                if ($key.GetValueKind('GlobalFlag') -ne $kind -or [string]$key.GetValue('GlobalFlag') -cne [string]$entry.original) { throw 'Restored GlobalFlag readback mismatch' }
            } elseif ($null -ne $key) {
                $key.DeleteValue('GlobalFlag', $false)
                if ($key.GetValueNames() -contains 'GlobalFlag') { throw 'GlobalFlag remained after restore' }
            }
            if (-not $entry.key_existed -and $null -ne $key -and $key.GetValueNames().Count -eq 0 -and $key.GetSubKeyNames().Count -eq 0) {
                $key.Dispose(); $key = $null
                $Registry.DeleteSubKey($path, $false)
            }
            $entry.restored = $true
        } catch { $errors.Add("Restore loader snaps for $($entry.image) failed: $_") } finally { if ($null -ne $key) { $key.Dispose() } }
    }
    return $errors.ToArray()
}
function Invoke-RunnerLoaderSnaps([string]$Evidence, [scriptblock]$Action, $Registry = $null) {
    $snapshot = Join-Path $Evidence 'loader-snaps-registry.json'
    if (Test-Path -LiteralPath $snapshot) { throw 'Fresh loader-snaps snapshot required' }
    $owned = $null -eq $Registry
    if ($owned) { $Registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64) }
    $mutex = [Threading.Mutex]::new($false, 'Global\MotorRunnerPrepLoaderSnaps')
    $locked = $false; $failure = $null
    $state = [ordered]@{schema=1; diagnostic_only=$true; registry_view='Registry64'; flag='FLG_SHOW_LDR_SNAPS 0x00000002'; entries=@(); restore_errors=@()}
    try {
        $locked = $mutex.WaitOne(0)
        if (-not $locked) { throw 'Another loader-snaps collection is active' }
        foreach ($image in @('cargo.exe','rustc.exe')) {
            $path = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\' + $image
            $key = $Registry.OpenSubKey($path, $true)
            try {
                $entry = @{image=$image; key_existed=($null -ne $key); value_existed=$false; kind=$null; original=$null; applied=$false; restored=$false}
                [uint32]$flags = 0
                if ($null -ne $key -and $key.GetValueNames() -contains 'GlobalFlag') {
                    $entry.value_existed = $true
                    $entry.kind = $key.GetValueKind('GlobalFlag').ToString()
                    $entry.original = $key.GetValue('GlobalFlag', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                    if ($entry.kind -eq 'String') { $flags = [Convert]::ToUInt32(([string]$entry.original -replace '^0x',''), 16) }
                    elseif ($entry.kind -eq 'DWord') { $flags = [uint32]([long]$entry.original -band 4294967295L) }
                    else { throw "Unsupported original GlobalFlag kind $($entry.kind); refusing to change it" }
                }
                $state.entries += $entry
                Save-RunnerLoaderState $snapshot $state
                if ($null -eq $key) { $key = $Registry.CreateSubKey($path) }
                $entry.applied_value = '{0:x8}' -f ($flags -bor 2)
                $key.SetValue('GlobalFlag', $entry.applied_value, [Microsoft.Win32.RegistryValueKind]::String)
                $entry.applied = $true
                if ([string]$key.GetValue('GlobalFlag') -cne $entry.applied_value) { throw 'Loader-snaps flag readback mismatch' }
                Save-RunnerLoaderState $snapshot $state
            } finally { if ($null -ne $key) { $key.Dispose() } }
        }
        & $Action
    } catch { $failure = $_; throw } finally {
        $restoreFailure = $null
        if ($locked) {
            $state.restore_errors = @(Restore-RunnerLoaderState $Registry $state.entries)
            try { Save-RunnerLoaderState $snapshot $state } catch { [Console]::Error.WriteLine("Loader-snaps snapshot write failed: $_") }
            $mutex.ReleaseMutex()
            if ($state.restore_errors.Count) {
                [Console]::Error.WriteLine(($state.restore_errors -join '; '))
                if ($null -eq $failure) { $restoreFailure = $state.restore_errors -join '; ' }
            }
        }
        $mutex.Dispose()
        if ($owned) { $Registry.Dispose() }
        if ($null -ne $restoreFailure) { throw $restoreFailure }
    }
}
