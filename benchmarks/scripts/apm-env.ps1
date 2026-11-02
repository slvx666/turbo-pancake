# Runs a command in the environment the WebRTC APM build (`--features apm`) needs on
# Windows: VS x64 developer environment, clang-cl as C/C++ compiler, meson + ninja,
# `nm` and `cp`. See tools/apm-eval/README.md and ADR 0006.
#
#   powershell -File benchmarks/scripts/apm-env.ps1 cargo test -p turbo-pancake-media-audio --features apm
#
# TURBO_PANCAKE_TOOLS (default D:\turbo-pancake-tools): folder with venv\Scripts (meson,
# ninja) and bin\nm.exe (a copy of llvm-nm.exe). Also needs
# `rustup component add llvm-tools` (rust-objcopy). All arguments form the command (no
# param block, so cargo flags such as -p pass through untouched). Call it with
# `-File`: invoked directly, PowerShell drops a literal `--` from the arguments.
$ToolsDir = if ($env:TURBO_PANCAKE_TOOLS) { $env:TURBO_PANCAKE_TOOLS } else { "D:\turbo-pancake-tools" }
$Command = $args
$ErrorActionPreference = "Stop"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio with C++ tools not found" }
$llvm = Join-Path $vs "VC\Tools\Llvm\x64\bin"
# CI images may ship LLVM separately instead of the VS clang component.
if (-not (Test-Path (Join-Path $llvm "clang-cl.exe"))) { $llvm = "$env:ProgramFiles\LLVM\bin" }
$git = Split-Path (Split-Path (Get-Command git).Source)
$paths = @("$ToolsDir\bin", "$ToolsDir\venv\Scripts", $llvm, (Join-Path $git "usr\bin"), "$env:USERPROFILE\.cargo\bin")
$env:PATH = (($paths | Where-Object { Test-Path $_ }) -join ";") + ";" + $env:PATH
# Full path: vcvars puts the VS clang's own headers on INCLUDE, and a separately
# installed (older) clang-cl found on PATH fails on their builtins (seen on CI images).
$env:CC = Join-Path $llvm "clang-cl.exe"
$env:CXX = $env:CC
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
$line = ($Command | ForEach-Object { if ($_ -match '\s') { "`"$_`"" } else { $_ } }) -join " "
cmd /c "call `"$vcvars`" >nul && $line"
exit $LASTEXITCODE
