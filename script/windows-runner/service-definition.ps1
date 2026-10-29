# Keep fresh installs and host-only updates on the same SCM command and environment.
function Get-RunnerServiceDefinition([string]$Root) {
    return [pscustomobject]@{
        binary = ('"{0}\bin\motor-runner.exe" service --instance ally --state-root "{0}\state" --backend windows-vm --vm-config "{0}\vm.toml"' -f $Root)
        display_name = 'Motor offline Hyper-V runner'
        start = 'auto'
        dependencies = @('vmms')
        environment = @("HOME=$Root\service-home", 'PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Program Files\Git\bin')
    }
}
