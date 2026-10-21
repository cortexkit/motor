# One descriptor builder for production prep grants and creation-only fixtures.
# Raw ACE masks preserve icacls semantics, including stage X without SYNCHRONIZE.
function Get-RunnerPrepRule([ValidateSet('Stage','Read','Modify')][string]$Role) {
    # AceFlags is a byte-backed enum. Build it with Enum.ToObject from an int
    # rather than PowerShell's -bor or an [int] cast, so no PowerShell enum
    # arithmetic or unboxing is involved. 3 = ObjectInherit | ContainerInherit.
    $inherit = [Enum]::ToObject([Security.AccessControl.AceFlags], 3)
    switch ($Role) {
        'Stage' { return @{mask=0x00000020; flags=[Enum]::ToObject([Security.AccessControl.AceFlags], 0)} }
        'Read' { return @{mask=0x001200a9; flags=$inherit} }
        'Modify' { return @{mask=0x001301bf; flags=$inherit} }
    }
}
function New-RunnerPrepAcl($BaseAcl, [string]$Sid, [ValidateSet('Stage','Read','Modify')][string]$Role) {
    $identity = [Security.Principal.SecurityIdentifier]::new($Sid)
    $raw = [Security.AccessControl.RawSecurityDescriptor]::new($BaseAcl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $raw.DiscretionaryAcl -or -not $BaseAcl.AreAccessRulesCanonical) { throw 'Prep grants require a canonical, non-null DACL' }
    # Like /grant:r, replace explicit allow entries for this trustee, retaining
    # inherited rules and every other trustee. Never replace owner/group/SACL.
    for ($i = $raw.DiscretionaryAcl.Count - 1; $i -ge 0; $i--) {
        $ace = $raw.DiscretionaryAcl[$i]
        if ($ace -is [Security.AccessControl.CommonAce] -and -not $ace.IsInherited -and $ace.AceQualifier -eq [Security.AccessControl.AceQualifier]::AccessAllowed -and $ace.SecurityIdentifier -eq $identity) { $raw.DiscretionaryAcl.RemoveAce($i) }
    }
    $rule = Get-RunnerPrepRule $Role
    $added = [Security.AccessControl.CommonAce]::new($rule.flags, [Security.AccessControl.AceQualifier]::AccessAllowed, $rule.mask, $identity, $false, $null)
    $index = $raw.DiscretionaryAcl.Count
    for ($i = 0; $i -lt $raw.DiscretionaryAcl.Count; $i++) { if ($raw.DiscretionaryAcl[$i].IsInherited) { $index = $i; break } }
    $raw.DiscretionaryAcl.InsertAce($index, $added)
    $bytes = [byte[]]::new($raw.BinaryLength)
    $raw.GetBinaryForm($bytes, 0)
    $BaseAcl.SetSecurityDescriptorBinaryForm($bytes, [Security.AccessControl.AccessControlSections]::Access)
    return $BaseAcl
}
function Grant-RunnerPrepPath([string]$Path, [string]$Sid, [ValidateSet('Stage','Read','Modify')][string]$Role) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $acl = New-RunnerPrepAcl (Get-Acl -LiteralPath $Path -ErrorAction Stop) $Sid $Role
    Set-Acl -LiteralPath $Path -AclObject $acl -ErrorAction Stop
}
function Get-RunnerInheritedDirectoryAcl([string]$Path) {
    # The probe is a throwaway sibling created without a descriptor, so Windows
    # applies the parent's inheritable rules exactly as for a production job
    # directory. Its name marks it as removable by leftover sweeps.
    $probe = Join-Path (Split-Path -Parent $Path) ('.motor-acl-probe-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $probe -ErrorAction Stop | Out-Null
    try { return Get-Acl -LiteralPath $probe -ErrorAction Stop }
    finally { Remove-Item -LiteralPath $probe -Force -ErrorAction Stop }
}
function New-RunnerPrepDirectory([string]$Path, [string]$Sid, [ValidateSet('Stage','Read','Modify')][string]$Role) {
    if (-not ('Motor.PrepDirectoryCreation' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
namespace Motor {
    public static class PrepDirectoryCreation {
        [StructLayout(LayoutKind.Sequential)]
        struct Attributes { public int Length; public IntPtr Descriptor; public int Inherit; }
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
        static extern bool CreateDirectoryW(string path, ref Attributes attributes);
        public static void Create(string path, byte[] descriptor) {
            var pin = GCHandle.Alloc(descriptor, GCHandleType.Pinned);
            try {
                var attributes = new Attributes { Length=Marshal.SizeOf(typeof(Attributes)), Descriptor=pin.AddrOfPinnedObject(), Inherit=0 };
                if (!CreateDirectoryW(path, ref attributes)) throw new Win32Exception(Marshal.GetLastWin32Error());
            } finally { pin.Free(); }
        }
    }
}
'@
    }
    # A descriptor passed to CreateDirectoryW replaces the rules the directory
    # would inherit from its parent (the Ally fixture run showed a directory
    # holding only the prep rule, unreadable even to SYSTEM). Production instead
    # creates the directory normally, so it inherits, and then adds the prep
    # rule. To build that same final descriptor up front, start from what an
    # ordinarily created sibling actually received: its owner, group and
    # inherited rules. New-RunnerPrepAcl then adds the explicit prep rule ahead
    # of the inherited ones, as Set-Acl would leave it.
    $acl = New-RunnerPrepAcl (Get-RunnerInheritedDirectoryAcl $Path) $Sid $Role
    $sddl = $acl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)
    [Motor.PrepDirectoryCreation]::Create($Path, $acl.GetSecurityDescriptorBinaryForm())
    return $sddl
}
