# CI-equivalent local gate: fmt + clippy + full test suite (plan T15).
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot\..

Write-Host "==> cargo fmt --all -- --check" -ForegroundColor Cyan
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { Write-Host "fmt check FAILED" -ForegroundColor Red; exit 1 }

Write-Host "==> cargo clippy --workspace --all-targets -- -D warnings" -ForegroundColor Cyan
cargo clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { Write-Host "clippy FAILED" -ForegroundColor Red; exit 1 }

Write-Host "==> cargo test --workspace" -ForegroundColor Cyan
cargo test --workspace
if ($LASTEXITCODE -ne 0) { Write-Host "tests FAILED" -ForegroundColor Red; exit 1 }

Write-Host "All checks passed." -ForegroundColor Green
