function Test-MotorLinkTarget {
    param([string]$Actual, [string]$Expected)
    # Windows PowerShell can expose an NT UNC substitute name without its
    # leading namespace prefix. Do not canonicalize, trim, or resolve paths.
    if ($Expected.StartsWith('\\', [StringComparison]::Ordinal) -and $Actual.StartsWith('UNC\', [StringComparison]::Ordinal)) {
        $Actual = '\\' + $Actual.Substring(4)
    }
    $comparison = [StringComparison]::Ordinal
    if ($Expected -match '^[A-Za-z]:\\') { $comparison = [StringComparison]::OrdinalIgnoreCase }
    return [string]::Equals($Actual, $Expected, $comparison)
}
