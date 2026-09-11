# 生成本地的 .cargo/config.toml（含本机 NDK 绝对路径，因此不入库）
#
# 用法（在仓库根目录执行）：
#   powershell -ExecutionPolicy Bypass -File scripts/setup-cargo-config.ps1
#   powershell -ExecutionPolicy Bypass -File scripts/setup-cargo-config.ps1 -NdkRoot <你的NDK根目录>
#
# 探测顺序：
#   1. -NdkRoot 参数
#   2. 环境变量 ANDROID_NDK_HOME
#   3. $env:ANDROID_HOME/ndk/<最新版本> 或 $env:ANDROID_SDK_ROOT/ndk/<最新版本>
#   4. 常见安装位置（$LOCALAPPDATA/Android/Sdk/ndk/*、各盘符下的 Android/ 等）

param(
    [string]$NdkRoot,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Resolve-NdkRoot {
    param([string]$Explicit)

    # 1) 显式参数
    if ($Explicit) { return $Explicit }

    # 2) 环境变量
    foreach ($v in @($env:ANDROID_NDK_HOME, $env:NDK_HOME, $env:ANDROID_NDK_ROOT)) {
        if ($v -and (Test-Path $v)) { return $v }
    }

    # 3) SDK 目录下的 ndk/<版本>，取版本号最大的
    foreach ($sdk in @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT, "$env:LOCALAPPDATA/Android/Sdk")) {
        if (-not $sdk) { continue }
        $ndkDir = Join-Path $sdk "ndk"
        if (Test-Path $ndkDir) {
            $latest = Get-ChildItem $ndkDir -Directory |
                Sort-Object { [version]($_.Name -replace '[^0-9.]', '') } -ErrorAction SilentlyContinue |
                Select-Object -Last 1
            if ($latest) { return $latest.FullName }
        }
    }

    # 4) 常见安装位置（按自己习惯增删即可）
    $candidates = @()
    foreach ($drive in @("C:", "D:", "E:")) {
        $candidates += "$drive/Android"
        $candidates += "$drive/android-ndk"
    }
    foreach ($dir in $candidates) {
        if (Test-Path $dir) {
            # 形如 .../android-ndk-r27d
            $cand = Get-ChildItem $dir -Directory | Where-Object { $_.Name -match 'ndk' } |
                Sort-Object Name | Select-Object -Last 1
            if ($cand) { return $cand.FullName }
            if (Test-Path (Join-Path $dir "toolchains/llvm")) { return $dir }
        }
    }
    return $null
}

function Get-HostTag {
    if ($IsMacOS) { return "darwin-x86_64" }
    if ($IsLinux) { return "linux-x86_64" }
    return "windows-x86_64"
}

# ---- 定位 NDK
$root = Resolve-NdkRoot -Explicit $NdkRoot
if (-not $root) {
    Write-Error @"
未找到 Android NDK。请任选其一：
  1) 设置环境变量 ANDROID_NDK_HOME 指向 NDK 根目录（形如 .../android-ndk-r27d）
  2) 显式指定：scripts/setup-cargo-config.ps1 -NdkRoot <路径>
"@
    exit 1
}
if (-not (Test-Path $root)) { Write-Error "NDK 路径不存在: $root"; exit 1 }

$tag = Get-HostTag
$binDir = Join-Path $root "toolchains/llvm/prebuilt/$tag/bin"
$sysroot = Join-Path $root "toolchains/llvm/prebuilt/$tag/sysroot"
if (-not (Test-Path $binDir)) { Write-Error "NDK 目录结构异常，未找到: $binDir`n（该 NDK 可能不含 $tag 预编译包）"; exit 1 }
if (-not (Test-Path $sysroot)) { Write-Error "未找到 sysroot: $sysroot"; exit 1 }

# Windows 上需要 .exe，其他平台不需要
$exe = if ($tag -eq "windows-x86_64") { ".exe" } else { "" }
$clang = Join-Path $binDir "clang$exe"
$llvmAr = Join-Path $binDir "llvm-ar$exe"
if (-not (Test-Path $clang)) { Write-Error "未找到 clang: $clang"; exit 1 }
if (-not (Test-Path $llvmAr)) { Write-Error "未找到 llvm-ar: $llvmAr"; exit 1 }

# ---- 生成配置（统一用正斜杠，Windows 也能识别）
$f = {
    param($p) $p.Replace('\', '/')
}
$clangF = & $f $clang
$arF = & $f $llvmAr
$sysF = & $f $sysroot

$targets = @(
    @{ triple = "aarch64-linux-android"; abi = "aarch64-linux-android21" },
    @{ triple = "armv7-linux-androideabi"; abi = "armv7-linux-androideabi21" },
    @{ triple = "x86_64-linux-android"; abi = "x86_64-linux-android21" }
)

$sb = [System.Text.StringBuilder]::new()
[void]$sb.AppendLine("# 由 scripts/setup-cargo-config.ps1 自动生成，请勿手工编辑（本机路径，不入库）")
[void]$sb.AppendLine("# NDK: $($root.Replace('\','/'))")
[void]$sb.AppendLine("# ABI 最低版本 21 = Android 5.0")
[void]$sb.AppendLine()
foreach ($t in $targets) {
    [void]$sb.AppendLine("[target.$($t.triple)]")
    [void]$sb.AppendLine("ar = `"$arF`"")
    [void]$sb.AppendLine("linker = `"$clangF`"")
    [void]$sb.AppendLine("rustflags = [")
    [void]$sb.AppendLine("  `"-C`", `"link-arg=--target=$($t.abi)`",")
    [void]$sb.AppendLine("  `"-C`", `"link-arg=--sysroot=$sysF`",")
    [void]$sb.AppendLine("  `"-C`", `"link-arg=-fuse-ld=lld`",")
    [void]$sb.AppendLine("]")
    [void]$sb.AppendLine()
}

$outDir = Join-Path $PSScriptRoot "../.cargo"
$outFile = Join-Path $outDir "config.toml"
if ((Test-Path $outFile) -and -not $Force) {
    Write-Host "已存在 .cargo/config.toml，未覆盖（要重新生成请加 -Force）" -ForegroundColor Yellow
    Write-Host "当前指向: $((Select-String -Path $outFile -Pattern '^# NDK:').Line)"
    exit 0
}

New-Item -ItemType Directory -Force -Path $outDir | Out-Null
[System.IO.File]::WriteAllText($outFile, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))

Write-Host "已生成 .cargo/config.toml" -ForegroundColor Green
Write-Host "  NDK    : $root"
Write-Host "  host   : $tag"
Write-Host "  现在可以执行: cargo build --target aarch64-linux-android --release"
