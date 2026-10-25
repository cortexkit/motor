# The provisioning manifest is copied from the sealed image, never from a running guest.
function Get-RunnerProvisionedAgentHash([AllowNull()][string]$RecordText) {
    if ([string]::IsNullOrWhiteSpace($RecordText)) { return $null }
    $match = [regex]::Match($RecordText.Trim(), '\A([0-9A-Fa-f]{64})\s+motor-winagent\.exe\z')
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value.ToUpperInvariant()
}

function Get-RunnerSealedAgentHash([string]$Parent, [string]$EvidenceDirectory = '', [int]$VolumeWaitSeconds = 15) {
    $value = Use-DiagnosticWindowsDisk -Path $Parent -EvidenceDirectory $EvidenceDirectory -VolumeWaitSeconds $VolumeWaitSeconds -Action {
        param($volume)
        $record = Get-PlainGuestItem $volume 'build\provision\winagent.sha256'
        if ($null -eq $record) { return $null }
        if ($record.PSIsContainer) { return $null }
        Get-RunnerProvisionedAgentHash ([IO.File]::ReadAllText($record.FullName))
    }
    if ($value -is [string] -and $value -match '\A[0-9A-F]{64}\z') { return $value }
    return $null
}
