$ErrorActionPreference = 'Stop'
$disks = @(Get-Disk)
$disksByNumber = @{}
foreach ($disk in $disks) {
    $number = [uint32]$disk.Number
    if ($disksByNumber.ContainsKey($number)) { throw "Duplicate storage disk number $number" }
    $disksByNumber[$number] = $disk
}
$items = @(Get-CimInstance -ClassName Win32_DiskDrive | ForEach-Object {
    $number = [uint32]$_.Index
    $disk = $disksByNumber[$number]
    if ($null -eq $disk) { throw "Disk $number is missing from the storage inventory" }
    if ([string]$disk.BusType -ne 'SCSI') { throw 'Unexpected non-SCSI disk in Hyper-V job' }
    [pscustomobject]@{
        number = $number
        lun = [uint32]$_.SCSILogicalUnit
        boot = [bool]$disk.IsBoot
        system = [bool]$disk.IsSystem
    }
})
ConvertTo-Json -InputObject $items -Compress
