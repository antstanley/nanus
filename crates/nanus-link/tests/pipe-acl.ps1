$ErrorActionPreference = 'Stop'
$pipe = [System.IO.Pipes.NamedPipeClientStream]::new('.', $env:NANUS_TEST_PIPE,
    [System.IO.Pipes.PipeDirection]::InOut)
try {
    $pipe.Connect(2000)
    $acl = $pipe.GetAccessControl()
    $rules = $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
    $writers = @()
    $readers = @()
    foreach ($rule in $rules) {
        if ($rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow) {
            throw "Unexpected deny rule: $rule"
        }
        $sid = $rule.IdentityReference.Value
        # WriteData, CreateNewInstance, or generic write/all grants may drive or impersonate
        # an agent. Everyone/Anonymous must hold none of them.
        if (([int64]$rule.PipeAccessRights -band 0x50000006) -ne 0) {
            if ($sid -notin @($env:NANUS_TEST_SID, 'S-1-5-18', 'S-1-5-32-544')) {
                throw "Unexpected pipe writer: $sid ($($rule.PipeAccessRights))"
            }
            $writers += $sid
        }
        if (([int64]$rule.PipeAccessRights -band 1) -ne 0) { $readers += $sid }
    }
    foreach ($sid in @($env:NANUS_TEST_SID, 'S-1-5-18', 'S-1-5-32-544')) {
        if ($sid -notin $writers) { throw "Expected writer missing: $sid" }
    }
    foreach ($sid in @('S-1-1-0', 'S-1-5-7')) {
        if ($sid -notin $readers) { throw "Expected read-only principal missing: $sid" }
    }
    Write-Output 'descriptor verified'
} finally { $pipe.Dispose() }
