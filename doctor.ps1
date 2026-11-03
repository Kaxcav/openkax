# doctor.ps1 — valida as chaves do .env com uma chamada GET /models por provider.
# Uso: .\doctor.ps1   (na raiz do repo, com .env preenchido)
# Não imprime chaves. Protótipo do futuro `openkax doctor`.

$ErrorActionPreference = 'Continue'
$envFile = Join-Path $PSScriptRoot '.env'
if (-not (Test-Path $envFile)) {
  Write-Host "Sem .env. Rode: Copy-Item .env.example .env ; notepad .env" -ForegroundColor Red
  exit 1
}

Get-Content $envFile | ForEach-Object {
  if ($_ -notmatch '^\s*#' -and $_ -match '^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$') {
    [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
  }
}

function Check([string]$name, [string]$url, [hashtable]$headers) {
  try {
    $r = Invoke-WebRequest -Uri $url -Headers $headers -Method GET -TimeoutSec 25 -UseBasicParsing
    # 2xx que não é JSON (ex.: placeholder "OK" text/plain de serviço aposentado) não vale.
    $ct = "$($r.Headers['Content-Type'])"
    if ($ct -notmatch 'json') {
      Write-Host ("[FAIL] {0,-11} HTTP {1} mas {2} (nao e JSON)" -f $name, $r.StatusCode, $ct) -ForegroundColor Red
      return $null
    }
    Write-Host ("[OK]   {0,-11} HTTP {1}" -f $name, $r.StatusCode) -ForegroundColor Green
    return $r.Content
  } catch {
    $code = $null
    if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
    $msg = $_.ErrorDetails.Message
    if (-not $msg) { $msg = $_.Exception.Message }
    if ($msg.Length -gt 160) { $msg = $msg.Substring(0, 160) + '...' }
    Write-Host ("[FAIL] {0,-11} HTTP {1}  {2}" -f $name, $code, $msg) -ForegroundColor Red
    return $null
  }
}

function CountModels($json) {
  try { $d = ($json | ConvertFrom-Json).data; if ($d) { return $d.Count } } catch {}
  return '?'
}

Write-Host "`nOpenKax doctor — $(Get-Date -Format 'yyyy-MM-dd HH:mm')`n"

if ($env:GEMINI_API_KEY) {
  $c = Check 'gemini' 'https://generativelanguage.googleapis.com/v1beta/openai/models' @{ Authorization = "Bearer $env:GEMINI_API_KEY" }
  if ($c) { Write-Host "       modelos visiveis: $(CountModels $c)  (endpoint OpenAI-compat OK)" }
} else { Write-Host "[SKIP] gemini      sem GEMINI_API_KEY" -ForegroundColor DarkGray }

if ($env:MISTRAL_API_KEY) {
  $c = Check 'mistral' 'https://api.mistral.ai/v1/models' @{ Authorization = "Bearer $env:MISTRAL_API_KEY" }
  if ($c) { Write-Host "       modelos visiveis: $(CountModels $c)" }
} else { Write-Host "[SKIP] mistral     sem MISTRAL_API_KEY" -ForegroundColor DarkGray }

if ($env:GROQ_API_KEY) {
  $c = Check 'groq' 'https://api.groq.com/openai/v1/models' @{ Authorization = "Bearer $env:GROQ_API_KEY" }
  if ($c) { Write-Host "       modelos visiveis: $(CountModels $c)" }
} else { Write-Host "[SKIP] groq        sem GROQ_API_KEY" -ForegroundColor DarkGray }

if ($env:OPENROUTER_API_KEY) {
  $c = Check 'openrouter' 'https://openrouter.ai/api/v1/key' @{ Authorization = "Bearer $env:OPENROUTER_API_KEY" }
  if ($c) {
    try {
      $d = ($c | ConvertFrom-Json).data
      $f = $d.free_model_daily_requests
      Write-Host ("       free_model_daily_requests: limit={0} used={1} remaining={2}" -f $f.limit, $f.used, $f.remaining)
      if ($f.limit -ge 1000) { Write-Host "       desbloqueio US`$10 confirmado (1000 RPD)" -ForegroundColor Green }
      else { Write-Host "       ainda em 50 RPD — credito de US`$10 nao reconhecido ainda" -ForegroundColor Yellow }
    } catch { Write-Host "       nao consegui ler free_model_daily_requests" -ForegroundColor Yellow }
  }
} else { Write-Host "[SKIP] openrouter  sem OPENROUTER_API_KEY" -ForegroundColor DarkGray }

if ($env:DEEPSEEK_API_KEY) {
  $c = Check 'deepseek' 'https://api.deepseek.com/v1/models' @{ Authorization = "Bearer $env:DEEPSEEK_API_KEY" }
} else { Write-Host "[SKIP] deepseek    sem DEEPSEEK_API_KEY" -ForegroundColor DarkGray }

Write-Host ""
