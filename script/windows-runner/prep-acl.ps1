# One descriptor builder for production prep grants and creation-only fixtures.
# Raw ACE masks preserve icacls semantics, including stage X without SYNCHRONIZE.
function Get-RunnerPrepRule([ValidateSet('Stage','Read','Modify')][string]$Role) {
    switch ($Role) {
        'Stage' { return @{mask=0x00000020; flags=[Security.AccessControl.AceFlags]::None} }
        'Read' { return @{mask=0x001200a9; flags=([Security.AccessControl.AceFlags]::ObjectInherit -bor [Security.AccessControl.AceFlags]::ContainerInherit)} }
        'Modify' { return @{mask=0x001301bf; flags=([Security.AccessControl.AceFlags]::ObjectInherit -bor [Security.AccessControl.AceFlags]::ContainerInherit)} }
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
function New-RunnerPrepDirectory([string]$Path, $BaseAcl, [string]$Sid, [ValidateSet('Stage','Read','Modify')][string]$Role) {
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
    # Let Windows inherit the real parent's rules once, just as the production
    # create-then-grant path does. Copying the parent's already inherited ACEs
    # into the creation descriptor would duplicate them or copy non-inheritable rules.
    $creation = [Security.AccessControl.RawSecurityDescriptor]::new($BaseAcl.GetSecurityDescriptorBinaryForm(),0)
    $creation.DiscretionaryAcl = [Security.AccessControl.RawAcl]::new(2,0)
    $creation.SetFlags(($creation.ControlFlags -band (-bnot [Security.AccessControl.ControlFlags]::DiscretionaryAclProtected)) -bor [Security.AccessControl.ControlFlags]::DiscretionaryAclPresent)
    $bytes = [byte[]]::new($creation.BinaryLength)
    $creation.GetBinaryForm($bytes,0)
    $BaseAcl.SetSecurityDescriptorBinaryForm($bytes,[Security.AccessControl.AccessControlSections]::Access)
    $acl = New-RunnerPrepAcl $BaseAcl $Sid $Role
    $sddl = $acl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)
    [Motor.PrepDirectoryCreation]::Create($Path, $acl.GetSecurityDescriptorBinaryForm())
    return $sddl
}
