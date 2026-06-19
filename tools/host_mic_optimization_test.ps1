[CmdletBinding()]
param(
    [Parameter()]
    [string] $BinaryPath = "C:\code\mic_volume_fix_latest.exe",

    [Parameter()]
    [int] $BurstCount = 20,

    [Parameter()]
    [int] $PollMs = 25,

    [Parameter()]
    [int] $TimeoutMs = 3000,

    [Parameter()]
    [int] $IdleSampleSeconds = 15
)

$ErrorActionPreference = "Stop"

$logPath = "C:\code\mic_opt_test.log"

try {
    Remove-Item -Path $logPath -ErrorAction SilentlyContinue
} catch {
    # optional cleanup of prior host-side log
}

function Write-TestLog([string]$Message) {
    $line = "[{0:O}] {1}" -f (Get-Date), $Message
    Write-Host $line
    Add-Content -Path $logPath -Value $line
}

if (-not ("CoreAudioInterop.MicControl" -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

namespace CoreAudioInterop {
    [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IMMDeviceEnumerator {
        [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr ppDevices);
        [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IntPtr ppDevice);
        [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string pwstrId, out IntPtr ppDevice);
        [PreserveSig] int RegisterEndpointNotificationCallback(IntPtr pClient);
        [PreserveSig] int UnregisterEndpointNotificationCallback(IntPtr pClient);
    }

    [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IMMDevice {
        [PreserveSig] int Activate(ref Guid iid, int dwClsCtx, IntPtr pActivationParams, out IntPtr ppInterface);
        [PreserveSig] int OpenPropertyStore(int stgmAccess, out IntPtr ppProperties);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string ppstrId);
        [PreserveSig] int GetState(out uint pdwState);
    }

    [ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IAudioEndpointVolume {
        [PreserveSig] int RegisterControlChangeNotify(IntPtr pNotify);
        [PreserveSig] int UnregisterControlChangeNotify(IntPtr pNotify);
        [PreserveSig] int GetChannelCount(out uint pnChannelCount);
        [PreserveSig] int SetMasterVolumeLevel(float fLevelDB, ref Guid pguidEventContext);
        [PreserveSig] int SetMasterVolumeLevelScalar(float fLevel, ref Guid pguidEventContext);
        [PreserveSig] int GetMasterVolumeLevel(out float pfLevelDB);
        [PreserveSig] int GetMasterVolumeLevelScalar(out float pfLevel);
        [PreserveSig] int SetChannelVolumeLevel(uint nChannel, float fLevelDB, ref Guid pguidEventContext);
        [PreserveSig] int SetChannelVolumeLevelScalar(uint nChannel, float fLevel, ref Guid pguidEventContext);
        [PreserveSig] int GetChannelVolumeLevel(uint nChannel, out float pfLevelDB);
        [PreserveSig] int GetChannelVolumeLevelScalar(uint nChannel, out float pfLevel);
        [PreserveSig] int SetMute([MarshalAs(UnmanagedType.Bool)] bool bMute, ref Guid pguidEventContext);
        [PreserveSig] int GetMute([MarshalAs(UnmanagedType.Bool)] out bool pbMute);
        [PreserveSig] int GetVolumeStepInfo(out uint pnStep, out uint pnStepCount);
        [PreserveSig] int VolumeStepUp(ref Guid pguidEventContext);
        [PreserveSig] int VolumeStepDown(ref Guid pguidEventContext);
        [PreserveSig] int QueryHardwareSupport(out uint pdwHardwareSupportMask);
        [PreserveSig] int GetVolumeRange(out float pflVolMin, out float pflVolMax, out float pflVolIncrement);
    }

    [ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E"), ClassInterface(ClassInterfaceType.None)]
    public class MMDeviceEnumeratorCom {
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct MicState {
        public float Level;
        public bool Muted;
    }

    public static class MicControl {
        private const int DataFlowCapture = 1;
        private const int RoleConsole = 0;
        private const int ClsCtxAll = 23;
        private static readonly Guid EndpointVolumeGuid = new Guid("5CDF2C82-841E-4546-9722-0CF74078229A");

        private static void ThrowIfFailed(int hr) {
            if (hr < 0) {
                Marshal.ThrowExceptionForHR(hr);
            }
        }

        private static IAudioEndpointVolume GetDefaultEndpoint() {
            IMMDeviceEnumerator enumerator = (IMMDeviceEnumerator)(object)new MMDeviceEnumeratorCom();
            IntPtr devicePtr;
            int hr = enumerator.GetDefaultAudioEndpoint(DataFlowCapture, RoleConsole, out devicePtr);
            ThrowIfFailed(hr);

            var device = (IMMDevice)Marshal.GetObjectForIUnknown(devicePtr);
            IntPtr volumePtr;
            Guid volumeGuid = EndpointVolumeGuid;
            hr = device.Activate(ref volumeGuid, ClsCtxAll, IntPtr.Zero, out volumePtr);
            ThrowIfFailed(hr);

            return (IAudioEndpointVolume)Marshal.GetObjectForIUnknown(volumePtr);
        }

        public static MicState GetState() {
            var endpoint = GetDefaultEndpoint();
            float level;
            bool muted;
            ThrowIfFailed(endpoint.GetMasterVolumeLevelScalar(out level));
            ThrowIfFailed(endpoint.GetMute(out muted));
            return new MicState { Level = level, Muted = muted };
        }

        public static void SetLevel(float level) {
            level = Math.Max(0f, Math.Min(1f, level));
            var endpoint = GetDefaultEndpoint();
            Guid ctx = Guid.NewGuid();
            ThrowIfFailed(endpoint.SetMasterVolumeLevelScalar(level, ref ctx));
        }

        public static void SetMute(bool muted) {
            var endpoint = GetDefaultEndpoint();
            Guid ctx = Guid.NewGuid();
            ThrowIfFailed(endpoint.SetMute(muted, ref ctx));
        }
    }
}
"@ -ErrorAction Stop
}

function Get-MicState {
    return [CoreAudioInterop.MicControl]::GetState()
}

function Set-MicVolume {
    param([int]$Percent)
    $level = [Math]::Max(0, [Math]::Min(100, $Percent)) / 100.0
    [CoreAudioInterop.MicControl]::SetLevel([float]$level)
}

function Set-MicMute {
    param([bool]$Mute)
    [CoreAudioInterop.MicControl]::SetMute([bool]$Mute)
}

function Wait-ForRestored {
    param(
        [bool]$RequireUnmuted = $true,
        [double]$ExpectedMinLevel = 0.99,
        [int]$TimeoutMs = 3000
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        $state = Get-MicState
        if ($state.Level -ge $ExpectedMinLevel -and (-not $RequireUnmuted -or (-not $state.Muted))) {
            return @{
                ElapsedMs = [int][math]::Max(0, ((Get-Date) - $global:testStart).TotalMilliseconds)
                State = $state
            }
        }
        Start-Sleep -Milliseconds $PollMs
    }
    return $null
}

function Test-Step {
    param(
        [string]$Name,
        [scriptblock]$Setup,
        [bool]$RequireUnmuted = $true,
        [double]$ExpectedMinLevel = 0.99
    )

    $global:testStart = Get-Date
    & $Setup
    Start-Sleep -Milliseconds 50
    $result = Wait-ForRestored -RequireUnmuted $RequireUnmuted -ExpectedMinLevel $ExpectedMinLevel -TimeoutMs $TimeoutMs
    $final = Get-MicState
    if ($null -ne $result) {
        Write-TestLog ("{0}: PASS in {1}ms (level={2}%, muted={3})" -f $Name, $result.ElapsedMs, [Math]::Round($final.Level * 100, 2), $final.Muted)
        return [PSCustomObject]@{
            Name = $Name
            Success = $true
            LatencyMs = $result.ElapsedMs
            FinalPercent = [Math]::Round($final.Level * 100, 2)
            FinalMuted = $final.Muted
        }
    }

    Write-TestLog ("{0}: FAIL (level={1}%, muted={2})" -f $Name, [Math]::Round($final.Level * 100, 2), $final.Muted)
    return [PSCustomObject]@{
        Name = $Name
        Success = $false
        LatencyMs = $TimeoutMs
        FinalPercent = [Math]::Round($final.Level * 100, 2)
        FinalMuted = $final.Muted
    }
}

function Get-AppMetrics {
    param([System.Diagnostics.Process]$Proc)

    $fresh = Get-Process -Id $Proc.Id -ErrorAction Stop
    return [PSCustomObject]@{
        WorkingSetMB = [Math]::Round($fresh.WorkingSet64 / 1MB, 2)
        PrivateMB = [Math]::Round($fresh.PrivateMemorySize64 / 1MB, 2)
        CpuSeconds = [Math]::Round([double]$fresh.CPU, 3)
    }
}

function Test-IdleResource {
    param(
        [System.Diagnostics.Process]$Proc,
        [int]$Seconds
    )

    $start = Get-AppMetrics -Proc $Proc
    Start-Sleep -Seconds $Seconds
    $end = Get-AppMetrics -Proc $Proc
    $cpuDelta = [Math]::Max(0, $end.CpuSeconds - $start.CpuSeconds)
    $cpuPercentOneCore = [Math]::Round(($cpuDelta / [Math]::Max(1, $Seconds)) * 100, 3)

    Write-TestLog ("idle-resource: working-set={0}MB private={1}MB cpu-delta={2}s cpu-one-core={3}% over {4}s" -f $end.WorkingSetMB, $end.PrivateMB, $cpuDelta, $cpuPercentOneCore, $Seconds)
    return [PSCustomObject]@{
        Name = "idle-resource"
        Success = $true
        WorkingSetMB = $end.WorkingSetMB
        PrivateMB = $end.PrivateMB
        CpuDeltaSeconds = $cpuDelta
        CpuOneCorePercent = $cpuPercentOneCore
        SampleSeconds = $Seconds
    }
}

function Restart-App {
    param([string]$Path)
    $processName = [System.IO.Path]::GetFileNameWithoutExtension($Path)
    Get-Process -Name $processName -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400

    $proc = Start-Process -FilePath $Path -PassThru
    Write-TestLog ("Started app PID={0}" -f $proc.Id)
    Start-Sleep -Milliseconds 1200
    return $proc
}

Write-TestLog "Host optimization test start"
Write-TestLog "Binary: $BinaryPath"
$appProcess = Restart-App -Path $BinaryPath

try {
    $results = @()
    $resource = Test-IdleResource -Proc $appProcess -Seconds $IdleSampleSeconds

    $results += Test-Step -Name "baseline-volume-11" -Setup {
        Set-MicVolume -Percent 11
    } -RequireUnmuted $true -ExpectedMinLevel 0.99

    $results += Test-Step -Name "baseline-volume-35" -Setup {
        Set-MicVolume -Percent 35
    } -RequireUnmuted $true -ExpectedMinLevel 0.99

    $results += Test-Step -Name "baseline-volume-80" -Setup {
        Set-MicVolume -Percent 80
    } -RequireUnmuted $true -ExpectedMinLevel 0.99

    $results += Test-Step -Name "baseline-mute" -Setup {
        Set-MicMute -Mute $true
    } -RequireUnmuted $true -ExpectedMinLevel 0.99

    Write-TestLog "burst-start"
    for ($i = 1; $i -le $BurstCount; $i++) {
        Set-MicVolume -Percent (Get-Random -Minimum 0 -Maximum 90)
        if (($i % 4) -eq 0) {
            Set-MicMute -Mute $true
        }
        Start-Sleep -Milliseconds 75
    }
    Start-Sleep -Milliseconds 250
    $results += Test-Step -Name "burst-final" -Setup {
        return
    } -RequireUnmuted $true -ExpectedMinLevel 0.99

    $pass = ($results | Where-Object { -not $_.Success }).Count -eq 0
    $latencies = $results | Where-Object { $_.Success } | Select-Object -ExpandProperty LatencyMs
    $avgLatency = 0
    if ($latencies.Count -gt 0) {
        $avgLatency = [int]($latencies | Measure-Object -Average | Select-Object -ExpandProperty Average)
    }

    Write-TestLog ("Summary pass={0}, avg-latency={1}ms, total={2}, fail={3}, idle-private={4}MB, idle-cpu-one-core={5}%" -f $pass, $avgLatency, $results.Count, ($results.Count - $latencies.Count), $resource.PrivateMB, $resource.CpuOneCorePercent)
    [PSCustomObject]@{
        Results = $results
        Resource = $resource
    } | ConvertTo-Json -Compress | Write-Output

    if (-not $pass) {
        throw "One or more host optimization checks failed"
    }
} finally {
    $processName = [System.IO.Path]::GetFileNameWithoutExtension($BinaryPath)
    Get-Process -Name $processName -ErrorAction SilentlyContinue | Stop-Process -Force
    Write-TestLog "App stopped"
}
