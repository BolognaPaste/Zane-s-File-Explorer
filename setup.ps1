# One-time toolchain install. Run from an administrator terminal in this folder:
#   powershell -ExecutionPolicy Bypass -File setup.ps1
winget install -e --id OpenJS.NodeJS.LTS
winget install -e --id Rustlang.Rustup
winget install -e --id Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
Write-Host "Done. Close this terminal, open a new one in this folder, then run: rustup default stable-msvc"
