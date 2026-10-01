$ErrorActionPreference = 'Stop'
$pipe = [System.IO.Pipes.NamedPipeClientStream]::new('.', $env:NANUS_TEST_PIPE,
    [System.IO.Pipes.PipeDirection]::InOut)
try {
    $pipe.Connect(2000)
    $acl = $pipe.GetAccessControl()
    $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
    # An elevated token may create objects owned by Administrators rather than TokenUser.
    # Validate that identity too: an arbitrary owner would be able to change the DACL.
    if ($owner -notin @($env:NANUS_TEST_SID, 'S-1-5-32-544')) {
        throw "Unexpected pipe owner: $owner"
    }
    $rules = $acl.GetAccessRules($true, $true, [System.Security.Principal.SecurityIdentifier])
    $writers = @()
    $readers = @()
    foreach ($rule in $rules) {
        if ($rule.AccessControlType -ne [System.Security.AccessControl.AccessControlType]::Allow) {
            throw "Unexpected deny rule: $rule"
        }
        $sid = $rule.IdentityReference.Value
        # Data/instance writes, metadata writes, deletion, DACL/owner changes, and generic
        # write/all must be confined to the same principals. Everyone/Anonymous get none.
        if (([int64]$rule.PipeAccessRights -band 0x500d0116) -ne 0) {
            if ($sid -notin @($env:NANUS_TEST_SID, 'S-1-5-18', 'S-1-5-32-544')) {
                throw "Unexpected pipe writer: $sid ($($rule.PipeAccessRights))"
            }
            $writers += $sid
        }
        if (([int64]$rule.PipeAccessRights -band 1) -ne 0) { $readers += $sid }
    }
    foreach ($sid in @($owner, 'S-1-5-18', 'S-1-5-32-544')) {
        if ($sid -notin $writers) { throw "Expected writer missing: $sid" }
    }
    foreach ($sid in @('S-1-1-0', 'S-1-5-7')) {
        if ($sid -notin $readers) { throw "Expected read-only principal missing: $sid" }
    }
    Write-Output "descriptor verified; owner=$owner; writers=$($writers -join ',')"
} finally { $pipe.Dispose() }
