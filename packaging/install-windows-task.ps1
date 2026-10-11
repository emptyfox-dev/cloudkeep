<#
.SYNOPSIS
    把 CloudKeep 注册为 Windows 计划任务（开机自启，常驻调度）。

.DESCRIPTION
    Windows 没有 systemd/launchd 那样的服务文件；CloudKeep 是个普通的控制台程序，
    最简单的常驻方式是计划任务：开机时以当前用户身份启动 `cloudkeep run`，
    进程内部自己按 CRON_SCHEDULE 调度，无需触发器。

    授权文件默认在 %USERPROFILE%\.cloudkeep\config.json；
    若希望改用独立目录，可修改下面的 WorkingDirectory 并设置 CLOUDKEEP_CONFIG_DIR。

.EXAMPLE
    # 注册（默认路径为 C:\cloudkeep\cloudkeep.exe）
    powershell -ExecutionPolicy Bypass -File .\install-windows-task.ps1

.EXAMPLE
    # 指定二进制与工作目录
    powershell -ExecutionPolicy Bypass -File .\install-windows-task.ps1 `
        -Binary "D:\tools\cloudkeep.exe" -WorkingDir "D:\cloudkeep"

.EXAMPLE
    # 卸载
    powershell -ExecutionPolicy Bypass -File .\install-windows-task.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [string]$Binary = "C:\cloudkeep\cloudkeep.exe",
    [string]$WorkingDir = "C:\cloudkeep",
    [string]$TaskName = "CloudKeep",
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

if ($Uninstall) {
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "已卸载计划任务: $TaskName"
    } else {
        Write-Host "计划任务不存在: $TaskName"
    }
    return
}

if (-not (Test-Path $Binary)) {
    throw "找不到二进制: $Binary（用 -Binary 指定实际路径）"
}
if (-not (Test-Path $WorkingDir)) {
    New-Item -ItemType Directory -Force -Path $WorkingDir | Out-Null
    Write-Host "已创建工作目录: $WorkingDir"
}

# 先用 --version 确认二进制可执行，避免注册一个必然失败的任务
& $Binary --version | Out-Null
Write-Host "二进制可用: $(& $Binary --version)"

# 计划任务自身不需要触发器：开机启动一次，进程内部按 cron 常驻调度
$action = New-ScheduledTaskAction -Execute $Binary -Argument 'run' -WorkingDirectory $WorkingDir

$trigger = New-ScheduledTaskTrigger -AtStartup

# 常驻任务不能设执行时长上限（默认 3 天会被强杀），因此显式设为 0 = 不限
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero)

# S4U：不需要保存密码，登录与否都能随开机启动；
# 程序用的是自己的授权文件，不依赖 Windows 的用户网络凭据
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType S4U -RunLevel Limited

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Principal $principal `
    -Description "CloudKeep 百度网盘定时备份服务（常驻调度）" `
    -Force | Out-Null

Write-Host ""
Write-Host "已注册计划任务: $TaskName"
Write-Host "  立即启动: Start-ScheduledTask -TaskName $TaskName"
Write-Host "  查看状态: Get-ScheduledTask -TaskName $TaskName | Get-ScheduledTaskInfo"
Write-Host "  卸载    : powershell -File $PSCommandPath -Uninstall"
