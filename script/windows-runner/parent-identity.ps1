# Service startup hashes the parent through the same guarded handle used for its identity record.
if (-not ('Motor.ParentFile' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using Microsoft.Win32.SafeHandles;

namespace Motor {
    public static class ParentFile {
        [StructLayout(LayoutKind.Sequential)]
        struct FileInformation {
            public uint Attributes, CreationLow, CreationHigh, AccessLow, AccessHigh;
            public uint WriteLow, WriteHigh, VolumeSerial, SizeHigh, SizeLow;
            public uint Links, IndexHigh, IndexLow;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct BasicInformation {
            public long CreationTime, LastAccessTime, LastWriteTime, ChangeTime;
            public uint Attributes;
        }
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern SafeFileHandle CreateFileW(string path, uint access, uint share,
            IntPtr security, uint disposition, uint flags, IntPtr template);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool GetFileInformationByHandle(SafeFileHandle handle, out FileInformation info);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool GetFileInformationByHandleEx(SafeFileHandle handle, int kind,
            out BasicInformation info, uint size);
        [DllImport("advapi32.dll", SetLastError = true)]
        static extern bool GetKernelObjectSecurity(SafeFileHandle handle, uint requested,
            [Out] byte[] descriptor, uint length, out uint needed);

        static void Check(bool success) {
            if (!success) throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        public static FileStream Open(string path) {
            // OPEN_REPARSE_POINT covers the leaf; reject redirected directories too.
            for (string dir = Path.GetDirectoryName(Path.GetFullPath(path)); dir != null;
                dir = Path.GetDirectoryName(dir)) {
                if ((File.GetAttributes(dir) & FileAttributes.ReparsePoint) != 0)
                    throw new IOException("Reparse directory refused: " + dir);
            }
            // GENERIC_READ | READ_CONTROL, FILE_SHARE_READ only, OPEN_EXISTING,
            // FILE_FLAG_OPEN_REPARSE_POINT. Other readers (including Hyper-V) coexist.
            var handle = CreateFileW(path, 0x80020000, 1, IntPtr.Zero, 3, 0x00200000, IntPtr.Zero);
            if (handle.IsInvalid) {
                int error = Marshal.GetLastWin32Error();
                handle.Dispose();
                throw new Win32Exception(error);
            }
            try {
                FileInformation info;
                Check(GetFileInformationByHandle(handle, out info));
                if ((info.Attributes & (0x400 | 0x10)) != 0)
                    throw new IOException("Parent or record is not a regular non-reparse file");
                return new FileStream(handle, FileAccess.Read);
            } catch { handle.Dispose(); throw; }
        }
        public static FileSecurity Security(FileStream file) {
            uint needed;
            GetKernelObjectSecurity(file.SafeFileHandle, 7, null, 0, out needed);
            if (needed == 0) throw new Win32Exception(Marshal.GetLastWin32Error());
            var bytes = new byte[needed];
            Check(GetKernelObjectSecurity(file.SafeFileHandle, 7, bytes, needed, out needed));
            var acl = new FileSecurity();
            acl.SetSecurityDescriptorBinaryForm(bytes);
            if (!acl.AreAccessRulesCanonical) throw new IOException("Noncanonical file ACL");
            return acl;
        }
        public static string[] Identity(FileStream file) {
            FileInformation info;
            BasicInformation basic;
            Check(GetFileInformationByHandle(file.SafeFileHandle, out info));
            Check(GetFileInformationByHandleEx(file.SafeFileHandle, 0, out basic,
                (uint)Marshal.SizeOf(typeof(BasicInformation))));
            return new string[] {
                info.VolumeSerial.ToString("X8"),
                info.IndexHigh.ToString("X8") + info.IndexLow.ToString("X8"),
                (((ulong)info.SizeHigh << 32) | info.SizeLow).ToString(System.Globalization.CultureInfo.InvariantCulture),
                DateTime.FromFileTimeUtc(basic.LastWriteTime).ToString("O"),
                ((basic.Attributes & 1) != 0).ToString(),
                Security(file).GetSecurityDescriptorSddlForm(AccessControlSections.Owner |
                    AccessControlSections.Group | AccessControlSections.Access)
            };
        }
        public static long ChangeTime(FileStream file) {
            BasicInformation basic;
            Check(GetFileInformationByHandleEx(file.SafeFileHandle, 0, out basic,
                (uint)Marshal.SizeOf(typeof(BasicInformation))));
            return basic.ChangeTime;
        }
    }
}
'@
}

function Get-RunnerParentIdentity([IO.FileStream]$Guard) {
    $values = [Motor.ParentFile]::Identity($Guard)
    # Hyper-V can rewrite an identical security descriptor at VM create/remove,
    # advancing ChangeTime without changing the parent content or its ACL.
    [ordered]@{volume_serial=$values[0]; file_index=$values[1]; size=$values[2]; last_write_utc=$values[3]; read_only=$values[4]; acl=$values[5]}
}

function Assert-RunnerFileAcl([IO.FileStream]$Guard, [switch]$ParentFile) {
    $acl = [Motor.ParentFile]::Security($Guard)
    $raw = [Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $raw.DiscretionaryAcl) { throw 'Null file DACL refused' }
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin @('S-1-5-18','S-1-5-32-544')) { throw 'File owner must be SYSTEM or Administrators' }
    if (-not $acl.AreAccessRulesProtected) { throw 'File ACL must disable inheritance' }
    $allowed = @('S-1-5-18','S-1-5-32-544')
    if ($ParentFile) {
        # Hyper-V's VM Bus Provider capability also receives access to VM disks.
        # go-winio documents it in the grantvmgroupaccess expected-ACL fixtures;
        # this sealed parent permits only read access, not their read/write case:
        # https://github.com/microsoft/go-winio/blob/main/pkg/security/grantvmgroupaccess_test.go
        $allowed += @('S-1-5-83-0','S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704')
    }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -ne 'Allow') { continue }
        if ($rule.IdentityReference.Value -notin $allowed) { throw 'Unexpected file ACL grant' }
        $readRights = [int][Security.AccessControl.FileSystemRights]::ReadAndExecute -bor [int][Security.AccessControl.FileSystemRights]::Synchronize
        if ($rule.IdentityReference.Value -eq 'S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704') {
            $readRights = [int][Security.AccessControl.FileSystemRights]::Read -bor [int][Security.AccessControl.FileSystemRights]::Synchronize
        }
        if ($ParentFile -and ($rule.FileSystemRights -band (-bnot $readRights))) { throw 'Parent ACL must grant read access only' }
    }
}

function Assert-RunnerPrivateDirectory([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    $item = Get-Item -LiteralPath $current -Force
    if (-not $item.PSIsContainer) { throw "Runner path is not a directory: $current" }
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse runner directory refused: $current" }
    $acl = Get-Acl -LiteralPath $current
    $raw = [Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $raw.DiscretionaryAcl) { throw "Null runner directory DACL refused: $current" }
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin @('S-1-5-18','S-1-5-32-544')) {
        throw "Runner directory owner must be SYSTEM or Administrators: $current"
    }
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.AccessControlType -ne 'Allow') { continue }
        $sid = $rule.IdentityReference.Value
        if ($sid -notin @('S-1-5-18','S-1-5-32-544')) {
            throw "Runner directory grants access outside SYSTEM/Administrators: $current ($sid)"
        }
    }
    for ($parent = [IO.Directory]::GetParent($current); $null -ne $parent; $parent = $parent.Parent) {
        if ((Get-Item -LiteralPath $parent.FullName -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Reparse ancestor refused for runner directory: $($parent.FullName)"
        }
    }
}

function Write-RunnerParentVerification([string]$Parent, [string]$ParentSha256, [string]$Record, [string]$HashEvidence) {
    $temporary = "$Record.$([Guid]::NewGuid()).tmp"
    $guard = $null
    try {
        # A failed digest must not leave a stale identity record usable.
        if (Test-Path -LiteralPath $Record) { Remove-Item -LiteralPath $Record -Force }
        if ($ParentSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw 'Invalid configured parent SHA-256' }
        $guard = [Motor.ParentFile]::Open($Parent)
        Assert-RunnerFileAcl $guard -ParentFile
        $identity = Get-RunnerParentIdentity $guard
        if ($identity.read_only -cne 'True') { throw 'Parent must be sealed read-only' }
        $changeTimeEvidence = [Motor.ParentFile]::ChangeTime($guard).ToString([Globalization.CultureInfo]::InvariantCulture)
        $sha = [Security.Cryptography.SHA256]::Create()
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $hash = $null
        try { $hash = -join ($sha.ComputeHash($guard) | ForEach-Object { $_.ToString('x2') }) } finally {
            $timer.Stop(); $sha.Dispose()
            if ($HashEvidence) {
                @{utc=[DateTime]::UtcNow.ToString('o'); event='parent-sha256'; hash_duration_ms=$timer.Elapsed.TotalMilliseconds; hash_matches_configuration=($hash -ieq $ParentSha256); change_time_filetime=$changeTimeEvidence} | ConvertTo-Json -Compress | Add-Content -LiteralPath $HashEvidence -Encoding UTF8
            }
        }
        if ($hash -ine $ParentSha256) { throw 'Hyper-V parent SHA-256 mismatch' }
        $changeTime = [Motor.ParentFile]::ChangeTime($guard).ToString([Globalization.CultureInfo]::InvariantCulture)
        $recordData = [ordered]@{schema=2; parent=[IO.Path]::GetFullPath($Parent); sha256=$hash; change_time_filetime=$changeTime; identity=$identity}
        # Create the temporary file with its final protected DACL, before publishing.
        $security = [Security.AccessControl.FileSecurity]::new()
        $security.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
        $output = [IO.FileStream]::new($temporary, [IO.FileMode]::CreateNew, [Security.AccessControl.FileSystemRights]::FullControl, [IO.FileShare]::None, 4096, [IO.FileOptions]::WriteThrough, $security)
        try {
            $bytes = [Text.UTF8Encoding]::new($false).GetBytes(($recordData | ConvertTo-Json -Depth 4 -Compress))
            $output.Write($bytes, 0, $bytes.Length)
            $output.Flush($true)
        } finally { $output.Dispose() }
        [IO.File]::Move($temporary, $Record)
        return @{cache_hit=$false; hash_duration_ms=$timer.Elapsed.TotalMilliseconds; change_time_filetime=$changeTime}
    } finally {
        if ($guard) { $guard.Dispose() }
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

function Open-RunnerVerifiedParent([string]$Parent, [string]$ParentSha256, [string]$Record) {
    $guard = $null
    $recordGuard = $null
    try {
        $recordGuard = [Motor.ParentFile]::Open($Record)
        Assert-RunnerFileAcl $recordGuard
        $reader = [IO.StreamReader]::new($recordGuard, [Text.Encoding]::UTF8)
        try { $verified = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        if ($verified.schema -ne 2 -or $verified.parent -cne [IO.Path]::GetFullPath($Parent) -or $verified.sha256 -ine $ParentSha256) { throw 'Verification record does not match configured parent' }
        $guard = [Motor.ParentFile]::Open($Parent)
        Assert-RunnerFileAcl $guard -ParentFile
        $identity = Get-RunnerParentIdentity $guard
        if ($identity.read_only -cne 'True') { throw 'Parent is no longer read-only' }
        foreach ($key in $identity.Keys) {
            if ($identity[$key] -cne $verified.identity.$key) { throw "Parent identity differs: $key" }
        }
        # Skipping the per-job digest is safe because the service retains a
        # read-sharing, write/delete-denying handle. ChangeTime is evidence only:
        # Hyper-V can rewrite an identical ACL and advance it. Each service start
        # hashes again before any job can boot.
        return $guard
    } catch {
        if ($guard) { $guard.Dispose() }
        throw "parent changed since verification; restart the service to re-verify: $_"
    } finally {
        if ($recordGuard) { $recordGuard.Dispose() }
    }
}

function Confirm-RunnerParentVerification([string]$Parent, [string]$ParentSha256, [string]$Record, [string]$HashEvidence) {
    # Every service start rehashes, even if the previous start left a record.
    return Write-RunnerParentVerification $Parent $ParentSha256 $Record $HashEvidence
}

function Reuse-RunnerParentVerification([string]$Parent, [string]$ParentSha256, [string]$Record) {
    $recordGuard = $null
    $parentGuard = $null
    try {
        $recordGuard = [Motor.ParentFile]::Open($Record)
        Assert-RunnerFileAcl $recordGuard
        $reader = [IO.StreamReader]::new($recordGuard, [Text.Encoding]::UTF8)
        try { $verified = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        if ($verified.schema -ne 2 -or $verified.parent -cne [IO.Path]::GetFullPath($Parent) -or $verified.sha256 -ine $ParentSha256) {
            throw 'Verification record does not match configured parent'
        }
        $parentGuard = [Motor.ParentFile]::Open($Parent)
        Assert-RunnerFileAcl $parentGuard -ParentFile
        $identity = Get-RunnerParentIdentity $parentGuard
        if ($identity.read_only -cne 'True') { throw 'Parent is no longer read-only' }
        foreach ($key in $identity.Keys) {
            if ($identity[$key] -cne $verified.identity.$key) { throw "Parent identity differs: $key" }
        }
        # Hyper-V may advance ChangeTime while leaving the parent contents and
        # security descriptor unchanged, so this guard uses the same stable file
        # identity as Prepare/Run. The caller holds its own no-write/delete handle.
        return @{cache_hit=$true; hash_duration_ms=0; record_reused=$true}
    } catch {
        throw "parent changed since verification; restart the service to re-verify: $_"
    } finally {
        if ($parentGuard) { $parentGuard.Dispose() }
        if ($recordGuard) { $recordGuard.Dispose() }
    }
}
