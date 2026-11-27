# 用法：把 PAT 和仓库地址填进去，然后运行
param(
  [Parameter(Mandatory=$true)][string]$Token,
  [Parameter(Mandatory=$true)][string]$Repo   # 例如 yourname/cockpit-plus
)

$ErrorActionPreference = 'Stop'
Set-Location 'D:\cockpit-plus'

Write-Host "[1/3] 配置 remote..." -ForegroundColor Cyan
git remote remove fork 2>$null
git remote add fork "https://$Token@github.com/$Repo.git"

Write-Host "[2/3] 推送 main 分支..." -ForegroundColor Cyan
git push fork main --force

Write-Host "[3/3] 推送 gateway 分支（触发构建）..." -ForegroundColor Cyan
git branch -f gateway main 2>$null
git push fork gateway --force

Write-Host ""
Write-Host "完成。去 GitHub 看构建：" -ForegroundColor Green
Write-Host "  https://github.com/$Repo/actions"
