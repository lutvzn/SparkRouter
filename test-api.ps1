<#
.SYNOPSIS
Smoke-tests the SparkRouter OpenAI-compatible API with one models call and
one non-streaming chat completion.

.DESCRIPTION
Lists model IDs from GET {BaseUrl}/models, then sends POST
{BaseUrl}/chat/completions with the given model, prompt, and
"stream": false, and prints the assistant reply text. Only the gateway's
HTTP API is used: the script does not read .env files, does not touch the
SQLite database, and does not test streaming.

A nonempty ApiKey (default: $env:ROUTER_API_KEY) is sent as a bearer token.
No Authorization header is sent when it is empty, which only works while
neither ROUTER_API_KEY nor dashboard-generated API keys are configured.

.PARAMETER BaseUrl
Gateway base URL including the /v1 prefix. Default: http://127.0.0.1:8080/v1

.PARAMETER Model
Model ID to call: 'combo/<name>' or an upstream model ID listed by a
refreshed connection. Default: combo/coding

.PARAMETER ApiKey
Bearer key for /v1/*. Default: $env:ROUTER_API_KEY

.PARAMETER Prompt
User message content sent to the model. Default: 'Reply with a short hello.'

.PARAMETER TimeoutSec
Per-request timeout in seconds. The gateway caps the whole pre-commit
fallback phase at 180 seconds, so the default leaves a little headroom.

.EXAMPLE
.\test-api.ps1 -Model combo/coding

Lists models, then calls the 'coding' combo with the default prompt and
$env:ROUTER_API_KEY as the bearer key.

.EXAMPLE
$env:ROUTER_API_KEY = 'a-long-random-gateway-key'
.\test-api.ps1 -BaseUrl 'http://127.0.0.1:8080/v1' -Model 'combo/coding'

Sets the key through the process environment and the address through
-BaseUrl. .env files are never loaded automatically.

.EXAMPLE
.\test-api.ps1 -Model 'your-upstream-model-id'

Calls a directly routed upstream model ID instead of a combo.

.NOTES
Requires PowerShell 5.1 or newer. The chat request reaches whichever
upstream the gateway routes to: a local mock is free, while a real provider
connection can incur billable upstream requests. Failures print a concise
status line only; keys and raw upstream error bodies are never echoed.
#>
[CmdletBinding()]
param(
    [string]$BaseUrl = 'http://127.0.0.1:8080/v1',
    [string]$Model = 'combo/ZDR-Agent',
    [string]$ApiKey = 'sk-N0cqBfn2DU0acThxII7EDed4CIgMli7VpcUfXK8uLeg',
    [string]$Prompt = 'Reply with a short hello.',
    [ValidateRange(1, 3600)]
    [int]$TimeoutSec = 185
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Get-ErrorStatusCode {
    param($ErrorRecord)

    try {
        if ($null -ne $ErrorRecord.Exception.Response -and
            $null -ne $ErrorRecord.Exception.Response.StatusCode) {
            return [int]$ErrorRecord.Exception.Response.StatusCode
        }
    }
    catch {
        return $null
    }
    return $null
}

$base = $BaseUrl.TrimEnd('/')
$headers = @{}
if (-not [string]::IsNullOrEmpty($ApiKey)) {
    $headers['Authorization'] = "Bearer $ApiKey"
    $authNote = 'bearer key (value not shown)'
}
else {
    $authNote = 'none (empty ApiKey)'
}

Write-Host "SparkRouter API test: $base"
Write-Host "Authorization: $authNote"

$call = 'GET /models'
try {
    $listArgs = @{
        Method     = 'Get'
        Uri        = "$base/models"
        Headers    = $headers
        TimeoutSec = $TimeoutSec
    }
    $models = Invoke-RestMethod @listArgs

    $modelIds = @()
    if ($null -ne $models.data) {
        $modelIds = @($models.data | ForEach-Object { $_.id })
    }

    Write-Host ''
    if ($modelIds.Count -eq 0) {
        Write-Host 'Models: (none listed)'
    }
    else {
        Write-Host "Models ($($modelIds.Count)):"
        foreach ($id in $modelIds) {
            Write-Host "  - $id"
        }
    }

    $call = 'POST /chat/completions'
    $requestJson = @{
        model    = $Model
        stream   = $false
        messages = @(
            @{ role = 'user'; content = $Prompt }
        )
    } | ConvertTo-Json -Depth 6

    $chatArgs = @{
        Method      = 'Post'
        Uri         = "$base/chat/completions"
        Headers     = $headers
        ContentType = 'application/json'
        Body        = [System.Text.Encoding]::UTF8.GetBytes($requestJson)
        TimeoutSec  = $TimeoutSec
    }
    $response = Invoke-RestMethod @chatArgs

    $reply = $null
    if ($null -ne $response.choices -and @($response.choices).Count -gt 0) {
        $reply = @($response.choices)[0].message.content
    }

    Write-Host ''
    Write-Host "Chat request model: $Model"
    if ([string]::IsNullOrEmpty($reply)) {
        Write-Warning 'The gateway replied without choices[0].message.content.'
    }
    else {
        Write-Host "Reply: $reply"
    }
    Write-Host ''
    Write-Host 'API test passed.' -ForegroundColor Green
    exit 0
}
catch {
    $statusCode = Get-ErrorStatusCode -ErrorRecord $_
    Write-Host ''
    if ($null -ne $statusCode) {
        Write-Host "API test failed: $call returned HTTP $statusCode." -ForegroundColor Red
    }
    else {
        Write-Host "API test failed during $call." -ForegroundColor Red
    }
    Write-Host 'Check that the gateway is running, the API key matches, and the model exists. Raw error bodies and secrets are not shown.' -ForegroundColor Red
    exit 1
}
