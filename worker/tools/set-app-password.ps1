param(
  [string]$Worker,
  [switch]$Deploy
)

$ErrorActionPreference = "Stop"

function Assert-WorkerName([string]$Name) {
  if ([string]::IsNullOrWhiteSpace($Name) -or $Name -notmatch '^[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}$') {
    throw 'Pass -Worker with the name of your deployed Worker (1-63 letters, digits, underscores or hyphens).'
  }
}

function Assert-AppPassword([string]$Password, [string]$Repeated) {
  # Match worker/src/index.js MIN_SECRET_LENGTHS.APP_PASSWORD.
  if ([string]::IsNullOrWhiteSpace($Password) -or $Password.Length -lt 12 -or $Password.Length -gt 256) {
    throw 'Password must be 12-256 characters.'
  }
  if ($Password -cne $Repeated) { throw 'Passwords do not match.' }
}

function Read-Secret([string]$Prompt) {
  $secure = Read-Host -Prompt $Prompt -AsSecureString
  $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
  try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) }
  finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
}

Assert-WorkerName $Worker
try {
  $first = Read-Secret "New BubblePlay password"
  $second = Read-Secret "Repeat BubblePlay password"
  Assert-AppPassword $first $second
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
