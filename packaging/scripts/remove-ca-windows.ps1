# Removes the Cider Offline local CA from the current user's Root store.
# Use after uninstalling if the CA was not removed from the app's Settings page.
# Windows asks for confirmation for each certificate removed.
$ErrorActionPreference = 'Stop'
# The current name first, then the one used before the rename to "Cider Offline".
$commonNames = @('CN=Cider Offline CA', 'CN=Cider Offline Proxy Local CA')
$certs = @(Get-ChildItem Cert:\CurrentUser\Root | Where-Object {
    $parts = $_.Subject -split ',\s*'
    @($commonNames | Where-Object { $parts -contains $_ }).Count -gt 0
})
if ($certs.Count -eq 0) {
    Write-Host 'No Cider Offline CA found in Cert:\CurrentUser\Root.'
    exit 0
}
foreach ($cert in $certs) {
    Write-Host "Removing $($cert.Thumbprint)"
    Remove-Item -LiteralPath "Cert:\CurrentUser\Root\$($cert.Thumbprint)"
}
