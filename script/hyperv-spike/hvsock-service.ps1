<#
.SYNOPSIS
Register or remove Motor's Hyper-V socket service on this host.
.DESCRIPTION
The host can only connect to a guest's Hyper-V socket listener on a service ID
that is registered under GuestCommunicationServices. This is the one host
configuration step the hvsock transport needs. -Install creates the key with an
owner marker and is a no-op when the spike's own key already exists. -Uninstall
removes only a key carrying that marker. A key with the same GUID created by
anything else is never changed. -Status only reports.
.EXAMPLE
.\hvsock-service.ps1 -Install
.\hvsock-service.ps1 -Uninstall
#>
param([switch]$Install, [switch]$Uninstall, [switch]$Status)
. "$PSScriptRoot\common.ps1"
if (@($Install, $Uninstall, $Status | Where-Object { $_ }).Count -ne 1) { throw 'FAIL choose exactly one of -Install, -Uninstall or -Status' }
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Assert-Spike $admin 'elevated-host' ([Security.Principal.WindowsIdentity]::GetCurrent().Name)
$path = Get-SpikeServiceKeyPath
$before = Get-SpikeServiceKeyState $path
Write-Host "EVIDENCE hvsock-service-key-before path=$path $($before | ConvertTo-Json -Compress)"
if ($Status) {
    Assert-Spike ($before.exists -and $before.owner -eq $SpikeHvsockOwner) 'hvsock-service-registered' $path
    return
}
$operation = if ($Install) { 'install' } else { 'uninstall' }
$action = Get-SpikeServiceKeyAction $operation $before.exists $before.owner
Write-Host "EVIDENCE hvsock-service-key-action $action"
switch ($action) {
    'create' {
        New-Item -Path $path -Force | Out-Null
        New-ItemProperty -LiteralPath $path -Name ElementName -Value 'Motor job agent (Hyper-V spike)' -PropertyType String | Out-Null
        New-ItemProperty -LiteralPath $path -Name MotorOwner -Value $SpikeHvsockOwner -PropertyType String | Out-Null
    }
    'remove' { Remove-Item -LiteralPath $path }
}
$after = Get-SpikeServiceKeyState $path
Write-Host "EVIDENCE hvsock-service-key-after $($after | ConvertTo-Json -Compress)"
if ($Install) {
    Assert-Spike ($after.exists -and $after.owner -eq $SpikeHvsockOwner) 'hvsock-service-installed' $path
} else {
    Assert-Spike (-not $after.exists) 'hvsock-service-uninstalled' $path
}
