param(
  [string]$Worker = "playfull-cat",
  [switch]$Deploy
)

$ErrorActionPreference = "Stop"

function Read-Secret([string]$Prompt) {
  $secure = Read-Host -Prompt $Prompt -AsSecureString
  $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
  try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) }
  finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
}

$first = Read-Secret "New BubblePlay password"
$second = Read-Secret "Repeat BubblePlay password"
if ([string]::IsNullOrWhiteSpace($first) -or $first.Length -lt 8 -or $first.Length -gt 256) {
  throw "Password must be 8-256 characters."
}
if ($first -cne $second) { throw "Passwords do not match." }

try {
  $first | npx wrangler secret put APP_PASSWORD --name $Worker
  if ($LASTEXITCODE -ne 0) { throw "Wrangler failed to update APP_PASSWORD." }
  if ($Deploy) {
    npx wrangler deploy --name $Worker
    if ($LASTEXITCODE -ne 0) { throw "Wrangler failed to deploy the Worker." }
  }
  Write-Output "APP_PASSWORD updated for $Worker. Existing application sessions will expire on their next request."
}
finally {
  Remove-Variable first, second -ErrorAction SilentlyContinue
}
