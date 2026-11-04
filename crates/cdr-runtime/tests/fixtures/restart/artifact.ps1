# These fixtures deliberately use non-executable bytes and fake child processes.
# Substitute only embedded metadata inspection; retain real file-hash pins and journal IO.
function Get-CdrRuntimeArtifact {
    param($Path,$ExpectedBinaryHash,$ExpectedSourceCommit)
    $hash=Get-CdrArtifactHash $Path
    if($ExpectedBinaryHash -and $hash -cne $ExpectedBinaryHash){throw 'Fixture artifact mismatch'}
    if($ExpectedSourceCommit){throw 'Fixture source unknown'}
    [pscustomobject]@{ArtifactHash=$hash;BuildInfo=$null;RequireStartupRecord=$false}
}
