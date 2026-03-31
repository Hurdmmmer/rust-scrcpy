<#
用途：
1. 生成 flutter_rust_bridge 代码
2. 编译 Rust 动态库（rust_scrcpy.dll）
3. 复制 DLL 到 Flutter third_sdk 目录

默认目录：
- Rust:    D:\FlutterProject\game_helper\rust-ws-scrcpy
- Flutter: D:\FlutterProject\game_helper\sw_game_helper

用法：
pwsh -File .\scripts\build_rust_flutter_bridge.ps1
#>

param(
    [string]$RustRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path,
    [string]$FlutterRoot = "D:\FlutterProject\game_helper\sw_game_helper",
    [string]$FrbConfigFile = ""
)

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true

function Write-Log {
    param(
        [string]$Message,
        [string]$Level = "INFO"
    )
    $ts = Get-Date -Format "yyyy-MM-dd HH:mm:ss.fff"
    Write-Host "[$ts] [$Level] $Message"
}

function Invoke-Step {
    param(
        [string]$Name,
        [scriptblock]$Action
    )
    Write-Log "开始：$Name"
    & $Action
    Write-Log "完成：$Name"
}

function Assert-LastExitCode {
    param([string]$CommandName)
    if ($LASTEXITCODE -ne 0) {
        throw "$CommandName 执行失败，退出码=$LASTEXITCODE"
    }
}

try {
    $RustRoot = (Resolve-Path $RustRoot).Path
    $FlutterRoot = (Resolve-Path $FlutterRoot).Path

    if ([string]::IsNullOrWhiteSpace($FrbConfigFile)) {
        $FrbConfigFile = Join-Path $RustRoot "flutter_rust_bridge.yaml"
    } else {
        $FrbConfigFile = (Resolve-Path $FrbConfigFile).Path
    }

    $BuiltDll = Join-Path $RustRoot "target\release\rust_scrcpy.dll"
    $DstDll = Join-Path $FlutterRoot "lib\platforms\windows\third_sdk\rust_scrcpy.dll"
    $vsWhere = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vsWhere)) {
        throw "vswhere.exe not found: $vsWhere"
    }
    $vsInstall = (& $vsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($vsInstall)) {
        throw "Visual Studio C++ Build Tools not found"
    }
    $msvcRoot = Join-Path $vsInstall "VC\Tools\MSVC"
    $msvcVerDir = Get-ChildItem $msvcRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if ($null -eq $msvcVerDir) {
        throw "MSVC tools directory not found: $msvcRoot"
    }
    $msvcInclude = Join-Path $msvcVerDir.FullName "include"
    $winKitRoot = "C:\Program Files (x86)\Windows Kits\10\Include"
    $winKitVerDir = Get-ChildItem $winKitRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if ($null -eq $winKitVerDir) {
        throw "Windows SDK include directory not found: $winKitRoot"
    }
    $sdkBase = $winKitVerDir.FullName
    $sdkIncludes = @(
        (Join-Path $sdkBase "ucrt"),
        (Join-Path $sdkBase "um"),
        (Join-Path $sdkBase "shared"),
        (Join-Path $sdkBase "winrt"),
        (Join-Path $sdkBase "cppwinrt")
    )
    $bindgenIncludeArgs = @("-isystem", "`"$msvcInclude`"")
    foreach ($inc in $sdkIncludes) {
        if (Test-Path $inc) {
            $bindgenIncludeArgs += @("-isystem", "`"$inc`"")
        }
    }
    $env:BINDGEN_EXTRA_CLANG_ARGS = ($bindgenIncludeArgs -join " ")

    Write-Log "Rust 工程目录：$RustRoot"
    Write-Log "Flutter 工程目录：$FlutterRoot"
    Write-Log "FRB 配置文件（仅记录，不参与本次命令）：$FrbConfigFile"
    $DartOutput = Join-Path $FlutterRoot "lib\platforms\windows\bridge_generated"

    Invoke-Step "FRB 代码生成" {
        Set-Location $RustRoot
        # 说明：
        # flutter_rust_bridge_codegen 2.11.1 在 Windows 上使用 config-file
        # 可能触发 auto_upgrade.rs panic（no entry found for key）。
        # 这里改为显式参数并关闭自动升级/自动修复，保证稳定生成。
        flutter_rust_bridge_codegen generate `
          --rust-root $RustRoot `
          --rust-input crate::gh_api `
          --dart-output $DartOutput `
          --dart-entrypoint-class-name RustLib `
          --no-build-runner `
          --no-web `
          --no-auto-upgrade-dependency `
          --no-dart-fix `
          --no-dart-format `
          --no-rust-format
        Assert-LastExitCode "flutter_rust_bridge_codegen generate (cli mode)"
    }

    Invoke-Step "Flutter freezed/build_runner 代码生成" {
        Set-Location $FlutterRoot
        # 说明：
        # FRB 生成的 gh_api.dart 使用了 freezed 注解，
        # 若不执行 build_runner，将缺少 gh_api.freezed.dart，
        # 从而出现 `SessionEvent_Running` 等符号未定义错误。
        flutter pub run build_runner build --delete-conflicting-outputs
        Assert-LastExitCode "flutter pub run build_runner build --delete-conflicting-outputs"
    }

    Invoke-Step "Rust 编译（release + lib + frb）" {
        Set-Location $RustRoot
        # 必须启用 frb feature，确保 src/frb_generated.rs 参与最终 DLL。
        # 注意：FRB 仅扫描 crate::gh_api::flutter_api，回调注册仍由 Runner 直接调用 C ABI 导出。
        cargo build --release --lib --features frb
        Assert-LastExitCode "cargo build --release --lib --features frb"
        if (-not (Test-Path $BuiltDll)) {
            throw "编译后未找到 DLL：$BuiltDll"
        }
    }

    Invoke-Step "复制 DLL 到 Flutter third_sdk" {
        $dstDir = Split-Path $DstDll -Parent
        New-Item -ItemType Directory -Path $dstDir -Force | Out-Null
        Copy-Item -Path $BuiltDll -Destination $DstDll -Force
    }

    Write-Log "全部完成：FRB 生成 + Rust 编译 + DLL 复制" "SUCCESS"
}
catch {
    Write-Log "执行失败：$($_.Exception.Message)" "ERROR"
    exit 1
}





