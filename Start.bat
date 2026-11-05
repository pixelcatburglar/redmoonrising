@echo off
setlocal DisableDelayedExpansion
rem Native launcher derived from the Trinity project; PowerShell payload is embedded below.
set "RMR_LAUNCHER=%~f0"
set "RMR_INVOCATION_DIRECTORY=%CD%"
set "RMR_MODE=ui"
set "RMR_UI_PID="
set "RMR_RUN_ID="
:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--help" goto help
if /i "%~1"=="-h" goto help
if /i "%~1"=="--headless" goto headless
if /i "%~1"=="--stop" goto stop
if /i "%~1"=="--check" goto check
if /i "%~1"=="--server-only" goto serveronly
if /i "%~1"=="--no-auto-client" goto noautoclient
if /i "%~1"=="--client" goto client
if /i "%~1"=="--game-port" goto gameport
if /i "%~1"=="--web-port" goto webport
if /i "%~1"=="--database-port" goto databaseport
echo Unknown option. Use --help for usage. 1>&2
exit /b 2
:headless
set "RMR_MODE=headless"
shift
goto parse
:stop
set "RMR_MODE=stop"
shift
goto parse
:check
set "RMR_CHECK=1"
set "RMR_NO_CLIENT=1"
set "RMR_MODE=headless"
shift
goto parse
:serveronly
set "RMR_NO_CLIENT=1"
set "RMR_MODE=headless"
shift
goto parse
:noautoclient
set "RMR_NO_AUTO_CLIENT=1"
shift
goto parse
:client
if "%~2"=="" goto missing
set "RMR_VALUE=%~2"
if "%RMR_VALUE:~0,2%"=="--" goto missing
set "RMR_CLIENT_DIRECTORY=%RMR_VALUE%"
shift
shift
goto parse
:gameport
if "%~2"=="" goto missing
set "RMR_VALUE=%~2"
if "%RMR_VALUE:~0,2%"=="--" goto missing
set "RMR_GAME_PORT=%RMR_VALUE%"
shift
shift
goto parse
:webport
if "%~2"=="" goto missing
set "RMR_VALUE=%~2"
if "%RMR_VALUE:~0,2%"=="--" goto missing
set "RMR_WEB_PORT=%RMR_VALUE%"
shift
shift
goto parse
:databaseport
if "%~2"=="" goto missing
set "RMR_VALUE=%~2"
if "%RMR_VALUE:~0,2%"=="--" goto missing
set "RMR_DATABASE_PORT=%RMR_VALUE%"
shift
shift
goto parse
:missing
echo An option is missing its value. Use --help for usage. 1>&2
exit /b 2
:help
echo Red Moon Rising launcher
 echo Usage: Start.bat [--headless ^| --server-only ^| --check ^| --stop]
echo                  [--no-auto-client] [--client "folder"]
echo                  [--game-port N] [--web-port N] [--database-port N]
echo No options opens the launcher window. --check tests the server then stops it.
echo --server-only runs without selecting or starting a game client.
echo EVE starts only when you click Launch EVE in the launcher window.
echo Default ports: game 26000, web 18080, database 5432; busy ports advance automatically.
echo Explicit ports must be available. --help displays this text without setup.
exit /b 0
:parsed
set "RMR_PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if exist "%SystemRoot%\Sysnative\WindowsPowerShell\v1.0\powershell.exe" set "RMR_PS=%SystemRoot%\Sysnative\WindowsPowerShell\v1.0\powershell.exe"
if not "%RMR_MODE%"=="ui" goto console
start "" "%RMR_PS%" -NoLogo -NoProfile -ExecutionPolicy Bypass -STA -WindowStyle Hidden -Command "try { [AppContext]::SetSwitch('Switch.System.IO.UseLegacyPathHandling',$false); [AppContext]::SetSwitch('Switch.System.IO.BlockLongPaths',$false); $parts = [IO.File]::ReadAllText($env:RMR_LAUNCHER) -split '(?m)^#==RMR_POWERSHELL==\r?$', 2; if ($parts.Count -ne 2) { throw 'Launcher payload is missing.' }; & ([scriptblock]::Create($parts[1])) } catch { Add-Type -AssemblyName System.Windows.Forms; [Windows.Forms.MessageBox]::Show($_.Exception.Message,'Red Moon Rising could not open this folder','OK','Error') | Out-Null; exit 1 }"
exit /b
:console
"%RMR_PS%" -NoLogo -NoProfile -ExecutionPolicy Bypass -STA -Command "try { [AppContext]::SetSwitch('Switch.System.IO.UseLegacyPathHandling',$false); [AppContext]::SetSwitch('Switch.System.IO.BlockLongPaths',$false); $parts = [IO.File]::ReadAllText($env:RMR_LAUNCHER) -split '(?m)^#==RMR_POWERSHELL==\r?$', 2; if ($parts.Count -ne 2) { throw 'Launcher payload is missing.' }; & ([scriptblock]::Create($parts[1])) } catch { Write-Error $_; exit 1 }"
exit /b %errorlevel%
#==RMR_POWERSHELL==

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Load Windows PowerShell's checksum cmdlet even if cmd inherited PowerShell 7's module path.
Import-Module ([IO.Path]::Combine($PSHOME, 'Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1'))
$Self = [IO.Path]::GetFullPath($env:RMR_LAUNCHER)
$Root = [IO.Path]::GetDirectoryName($Self)
$SourceRoot = $Root
Set-Location -LiteralPath $Root
[Environment]::CurrentDirectory = $Root
$Runtime = Join-Path $Root '.rmr'
$Logs = Join-Path $Root 'logs'
$Utf8 = New-Object Text.UTF8Encoding($false)
[IO.Directory]::CreateDirectory($Runtime) | Out-Null
[IO.Directory]::CreateDirectory($Logs) | Out-Null
foreach ($ignoreDir in @($Runtime, $Logs)) {
    $ignoreFile = Join-Path $ignoreDir '.gitignore'
    if (-not [IO.File]::Exists($ignoreFile)) { [IO.File]::WriteAllText($ignoreFile, "*`n", $Utf8) }
}
$LogPath = Join-Path $Logs 'launcher.log'
$StopPath = Join-Path $Runtime 'stop.request'
$StatusPath = Join-Path $Runtime 'status.json'
if (-not $env:RMR_RUN_ID) { $env:RMR_RUN_ID = [Guid]::NewGuid().ToString('N') }
$script:Status = [ordered]@{ runId = $env:RMR_RUN_ID; stage = 'Preparing'; message = 'Checking local setup'; progress = 0; ready = $false; error = $false; stopped = $false; gamePort = 0; webPort = 0 }
$script:Stopping = $false

function Write-Log([string]$Message) {
    $Message = $Message -replace '\x1B\[[0-9;]*[a-zA-Z]', ''
    [IO.File]::AppendAllText($LogPath, $Message + [Environment]::NewLine, $Utf8)
    if ($env:RMR_MODE -ne 'worker') { Write-Host $Message }
}
function Set-Status([string]$Stage, [string]$Message, [int]$Progress) {
    $script:Status['downloadPercent'] = $null
    $script:Status['downloadName'] = ''
    $script:Status['phasePercent'] = $null
    $script:Status['phaseLabel'] = ''
    $script:Status.stage = $Stage
    $script:Status.message = $Message
    $script:Status.progress = $Progress
    [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
    Write-Log ('[{0}] {1}' -f $Stage, $Message)
}
function Assert-Running {
    if ($script:Stopping) { return }
    if (Test-Path -LiteralPath $StopPath) {
        $request = [IO.File]::ReadAllText($StopPath).Trim()
        if ($request -eq $env:RMR_RUN_ID -or $request -eq 'stop') { throw [OperationCanceledException]::new('Stop requested.') }
    }
    if ($env:RMR_UI_PID -and -not (Get-Process -Id ([int]$env:RMR_UI_PID) -ErrorAction SilentlyContinue)) {
        throw [OperationCanceledException]::new('Launcher window closed.')
    }
}
function Quote-Argument([string]$Value) {
    '"' + ([regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1')) + '"'
}
function Initialize-NativePathAliasType {
    if ('Rmr.NativePathAlias' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace Rmr {
    public static class NativePathAlias {
        [DllImport("kernel32.dll", EntryPoint = "DefineDosDeviceW", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool DefineDosDevice(uint flags, string device, string target);

        [DllImport("kernel32.dll", EntryPoint = "QueryDosDeviceW", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern uint QueryDosDevice(string device, StringBuilder target, int capacity);

        public static string Query(string device) {
            var target = new StringBuilder(32768);
            if (QueryDosDevice(device, target, target.Capacity) != 0)
                return target.ToString();
            int error = Marshal.GetLastWin32Error();
            if (error == 2) return null;
            throw new Win32Exception(error, "Cannot inspect drive " + device);
        }

        public static void Create(string device, string target) {
            // DDD_RAW_TARGET_PATH | DDD_NO_BROADCAST_SYSTEM.
            if (!DefineDosDevice(0x00000009, device, target))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot create temporary drive " + device);
        }

        public static void RemoveExact(string device, string target) {
            // RAW_TARGET_PATH | REMOVE_DEFINITION | EXACT_MATCH_ON_REMOVE | NO_BROADCAST_SYSTEM.
            if (!DefineDosDevice(0x0000000f, device, target)) {
                int error = Marshal.GetLastWin32Error();
                if (error != 2)
                    throw new Win32Exception(error, "Cannot remove temporary drive " + device);
            }
        }
    }
}
'@
}
function New-NativePathAlias {
    param([Parameter(Mandatory = $true)][string]$Path)
    Initialize-NativePathAliasType
    $source = [IO.Path]::GetFullPath($Path)
    if ($source.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        $target = '\??\UNC\' + $source.Substring(8)
    } elseif ($source.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        $target = '\??\' + $source.Substring(4)
    } elseif ($source.StartsWith('\\', [StringComparison]::Ordinal)) {
        $target = '\??\UNC\' + $source.Substring(2)
    } else {
        $target = '\??\' + $source
    }
    if ($source.Length -gt [IO.Path]::GetPathRoot($source).Length) { $target = $target.TrimEnd('\') }

    # Reuse exact existing aliases first, so a surviving private database retains
    # the same path after an interrupted launch. Existing mappings are never removed.
    foreach ($reuse in @($true, $false)) {
        for ($letter = 90; $letter -ge 68; $letter--) {
            $drive = ([char]$letter).ToString() + ':'
            $mutex = [Threading.Mutex]::new($false, ('Local\Trinity.NativePathAlias.' + [char]$letter))
            $held = $false; $created = $false; $keep = $false
            try {
                try { $held = $mutex.WaitOne(0) }
                catch [Threading.AbandonedMutexException] { $held = $true }
                if (-not $held) { continue }
                $existing = [Rmr.NativePathAlias]::Query($drive)
                if ($reuse) {
                    if (-not [string]::Equals($existing, $target, [StringComparison]::OrdinalIgnoreCase)) { continue }
                } else {
                    if ($null -ne $existing) { continue }
                    [Rmr.NativePathAlias]::Create($drive, $target)
                    $created = $true
                }
                if (-not [string]::Equals([Rmr.NativePathAlias]::Query($drive), $target, [StringComparison]::OrdinalIgnoreCase)) {
                    throw "Temporary drive $drive changed while preparing Rmr. Run start.bat again."
                }
                # Register the new OS drive with this PowerShell session too.
                # Sessions started before the mapping may not have mounted it yet.
                $driveName = ([char]$letter).ToString()
                if (-not (Get-PSDrive -Name $driveName -PSProvider FileSystem -ErrorAction SilentlyContinue)) {
                    New-PSDrive -Name $driveName -PSProvider FileSystem -Root ($drive + '\') -Scope Script | Out-Null
                }
                $alias = [pscustomobject]@{
                    Root = $drive + '\'; Drive = $drive; Target = $target
                    SourcePath = $source; Mutex = $mutex; Created = $created; Removed = $false
                }
                $keep = $true
                return $alias
            } finally {
                if (-not $keep) {
                    try { if ($created) { [Rmr.NativePathAlias]::RemoveExact($drive, $target) } }
                    finally {
                        if ($held) { $mutex.ReleaseMutex() }
                        $mutex.Dispose()
                    }
                }
            }
        }
    }
    throw 'No free drive letter is available for Rmr. Free one drive letter from D: through Z: and run start.bat again.'
}
function Remove-NativePathAlias {
    param($Alias)
    if ($null -eq $Alias -or $Alias.Removed) { return }
    try {
        if ($Alias.Created) { [Rmr.NativePathAlias]::RemoveExact($Alias.Drive, $Alias.Target) }
        $Alias.Removed = $true
    } finally {
        if ($null -ne $Alias.Mutex) {
            try { $Alias.Mutex.ReleaseMutex() }
            finally { $Alias.Mutex.Dispose(); $Alias.Mutex = $null }
        }
    }
}
function New-NativeProcess([string]$File, [string[]]$Arguments) {
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = $File
    $startInfo.Arguments = ($Arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
    $startInfo.WorkingDirectory = $Root
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $p = New-Object Diagnostics.Process
    $p.StartInfo = $startInfo
    [void]$p.Start()
    return @{ Process = $p; Out = $p.StandardOutput.ReadLineAsync(); Err = $p.StandardError.ReadLineAsync(); Lines = (New-Object 'Collections.Generic.List[string]') }
}
function Read-NativeOutput($Job, [switch]$Capture) {
    foreach ($key in @('Out', 'Err')) {
        $limit = 200
        while ($null -ne $Job[$key] -and $Job[$key].IsCompleted -and $limit-- -gt 0) {
            $line = $Job[$key].GetAwaiter().GetResult()
            if ($null -eq $line) { $Job[$key] = $null; break }
            if ($Capture -and $key -eq 'Out') { $Job.Lines.Add($line) }
            else { Write-Log $line }
            if ($key -eq 'Out') { $Job[$key] = $Job.Process.StandardOutput.ReadLineAsync() }
            else { $Job[$key] = $Job.Process.StandardError.ReadLineAsync() }
        }
    }
}
function Stop-NativeProcess([Diagnostics.Process]$NativeProcess) {
    # Keep the original process handle open while targeting its PID, so it cannot
    # be recycled for an unrelated process. /T also stops cargo/rustup children.
    if ($NativeProcess.HasExited) { return }
    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = Join-Path $env:SystemRoot 'System32\taskkill.exe'
    $startInfo.Arguments = '/PID ' + $NativeProcess.Id + ' /T /F'
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $killer = New-Object Diagnostics.Process
    $killer.StartInfo = $startInfo
    try {
        [void]$killer.Start()
        $stdout = $killer.StandardOutput.ReadToEndAsync()
        $stderr = $killer.StandardError.ReadToEndAsync()
        if (-not $killer.WaitForExit(15000)) {
            $killer.Kill()
            Write-Log 'Process-tree shutdown timed out; stopping the owned parent process.'
        } elseif ($killer.ExitCode -ne 0 -and -not $NativeProcess.HasExited) {
            Write-Log ('Process-tree shutdown reported: ' + $stderr.GetAwaiter().GetResult().Trim())
        }
    } catch { Write-Log ('Process-tree shutdown reported: ' + $_.Exception.Message) }
    finally { $killer.Dispose() }
    if (-not $NativeProcess.HasExited) { $NativeProcess.Kill() }
    [void]$NativeProcess.WaitForExit(10000)
}
function Invoke-Native([string]$File, [string[]]$Arguments, [switch]$Capture, [int]$Timeout = 1800) {
    $job = New-NativeProcess $File $Arguments
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $exitedAt = $null
        while ($true) {
            Assert-Running
            Read-NativeOutput $job -Capture:$Capture
            if ($job.Process.HasExited) {
                if ($null -eq $exitedAt) { $exitedAt = $watch.Elapsed.TotalMilliseconds }
                # Daemons started by pg_ctl can inherit a pipe handle. Never wait
                # for their EOF after the actual command has already completed.
                if (($null -eq $job.Out -and $null -eq $job.Err) -or ($watch.Elapsed.TotalMilliseconds - $exitedAt -gt 250)) { break }
            }
            if ($watch.Elapsed.TotalSeconds -gt $Timeout) { throw "Timed out running $(Split-Path -Leaf $File). See logs/launcher.log." }
            Start-Sleep -Milliseconds 40
        }
        if ($job.Process.ExitCode -ne 0) { throw "$(Split-Path -Leaf $File) exited with code $($job.Process.ExitCode). See logs/launcher.log." }
        if ($Capture) { return ($job.Lines -join "`n").Trim() }
    } finally {
        Stop-NativeProcess $job.Process
        $job.Process.Dispose()
    }
}
function Initialize-DownloadProgressType {
    if ('Rmr.LiveDownload' -as [type]) { return }
    # Progress callbacks run on .NET threads, without a PowerShell runspace.
    # Keep their counters in C# and poll them from the worker's own thread.
    Add-Type -TypeDefinition @'
using System;
using System.Net;
using System.Threading;
using System.Threading.Tasks;
namespace Rmr {
    public sealed class LiveDownload : IDisposable {
        private readonly WebClient client = new WebClient();
        private long received;
        private long total = -1;
        private int percent = -1;
        public long Received { get { return Interlocked.Read(ref received); } }
        public long Total { get { return Interlocked.Read(ref total); } }
        public int Percent { get { return Interlocked.CompareExchange(ref percent, 0, 0); } }
        public LiveDownload() { client.DownloadProgressChanged += OnProgress; }
        private void OnProgress(object sender, DownloadProgressChangedEventArgs e) {
            Interlocked.Exchange(ref received, e.BytesReceived);
            Interlocked.Exchange(ref total, e.TotalBytesToReceive);
            Interlocked.Exchange(ref percent, e.TotalBytesToReceive > 0 ? e.ProgressPercentage : -1);
        }
        public Task Start(string url, string path) { return client.DownloadFileTaskAsync(new Uri(url), path); }
        public void Cancel() { client.CancelAsync(); }
        public void Dispose() { client.Dispose(); }
    }
}
'@
}
function Set-PhaseProgress([string]$Label, [string]$Message, [double]$Percent) {
    $script:Status['downloadPercent'] = $null
    $script:Status['phaseLabel'] = $Label
    $script:Status['phasePercent'] = [Math]::Max(0, [Math]::Min(100, $Percent))
    $script:Status.message = $Message
    [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
}
function Get-FileHashWithProgress([string]$Path, [string]$DisplayName) {
    Write-Log ('Verifying ' + $DisplayName)
    Set-PhaseProgress 'VERIFYING FILE' ('Verifying {0} - 0%' -f $DisplayName) 0
    $stream = [IO.File]::OpenRead($Path)
    $hash = [Security.Cryptography.SHA256]::Create()
    $buffer = New-Object byte[] (1MB)
    $processed = [long]0
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $total = $stream.Length
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Assert-Running
            [void]$hash.TransformBlock($buffer, 0, $count, $buffer, 0)
            $processed += $count
            if ($watch.ElapsedMilliseconds -ge 150 -or $processed -eq $total) {
                $percent = if ($total -gt 0) { [Math]::Floor(100.0 * $processed / $total) } else { 100 }
                Set-PhaseProgress 'VERIFYING FILE' ('Verifying {0} - {1}% ({2:N1} / {3:N1} MB)' -f $DisplayName,$percent,($processed / 1MB),($total / 1MB)) $percent
                $watch.Restart()
            }
        }
        [void]$hash.TransformFinalBlock([byte[]]@(), 0, 0)
        Set-PhaseProgress 'VERIFYING FILE' ('Verified {0} - 100%' -f $DisplayName) 100
        return ([BitConverter]::ToString($hash.Hash)).Replace('-', '').ToLowerInvariant()
    } finally { $stream.Dispose(); $hash.Dispose() }
}
function Get-Download([string]$Url, [string]$Name, [string]$Sha256) {
    if ([string]::IsNullOrWhiteSpace($Name) -or [IO.Path]::GetFileName($Name) -ne $Name) { throw 'Invalid download filename.' }
    $directory = Join-Path $Runtime 'downloads'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $destination = Join-Path $directory $Name
    if ([IO.File]::Exists($destination) -and (Get-FileHashWithProgress $destination $Name) -eq $Sha256) { return $destination }
    Initialize-DownloadProgressType
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $partial = $destination + '.part'
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        Assert-Running
        Write-Log "Downloading $Name (attempt $attempt/3)"
        $client = [Rmr.LiveDownload]::new()
        $script:Status['downloadName'] = $Name
        $script:Status['downloadPercent'] = 0
        $script:Status['phasePercent'] = $null
        $script:Status.message = 'Downloading {0} - 0%' -f $Name
        [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
        try {
            $task = $client.Start($Url, $partial)
            while (-not $task.IsCompleted) {
                Assert-Running
                $received = $client.Received; $total = $client.Total; $percent = $client.Percent
                $script:Status['downloadReceived'] = $received
                $script:Status['downloadTotal'] = $total
                if ($total -gt 0) {
                    $script:Status['downloadPercent'] = [Math]::Max(0, [Math]::Min(100, $percent))
                    $script:Status.message = 'Downloading {0} - {1}% ({2:N1} / {3:N1} MB)' -f $Name,$script:Status.downloadPercent,($received / 1MB),($total / 1MB)
                } else {
                    $script:Status['downloadPercent'] = $null
                    $script:Status.message = 'Downloading {0} - {1:N1} MB received (waiting for total size)' -f $Name,($received / 1MB)
                }
                [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8)
                Start-Sleep -Milliseconds 250
            }
            # WebClient's runtime Task can return a null completion object.
            # Suppress it so callers receive exactly one string: the file path.
            [void]$task.GetAwaiter().GetResult()
            if ((Get-FileHashWithProgress $partial $Name) -ne $Sha256) { throw "Checksum mismatch for $Name. Download refused." }
            if ([IO.File]::Exists($destination)) { [IO.File]::Replace($partial, $destination, [NullString]::Value) }
            else { [IO.File]::Move($partial, $destination) }
            return [string]$destination
        } catch [OperationCanceledException] { throw }
        catch { if ($attempt -eq 3) { throw }; Write-Log $_.Exception.Message }
        finally { $client.Cancel(); $client.Dispose() }
    }
}
function Expand-PortableZip([string]$Archive, [string]$Destination) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Directory]::CreateDirectory($Destination) | Out-Null
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    $base = [IO.Path]::GetFullPath($Destination).TrimEnd('\') + '\'
    try {
        $entries = @($zip.Entries | Where-Object { $_.FullName -match '^pgsql/(bin|lib|share)/' -and $_.Name -ne '' })
        $completed = 0
        $watch = [Diagnostics.Stopwatch]::StartNew()
        Set-PhaseProgress 'EXTRACTING POSTGRESQL' 'Download verified. Extracting PostgreSQL - 0%' 0
        foreach ($entry in $entries) {
            Assert-Running
            # pgAdmin is not needed and contains paths too long for PowerShell 5.1.
            if ($entry.FullName -notmatch '^pgsql/(bin|lib|share)/') { continue }
            $output = [IO.Path]::GetFullPath((Join-Path $Destination $entry.FullName))
            if (-not $output.StartsWith($base, [StringComparison]::OrdinalIgnoreCase)) { throw 'Archive contains an invalid path.' }
            if ($entry.Name -eq '') { [IO.Directory]::CreateDirectory($output) | Out-Null; continue }
            [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($output)) | Out-Null
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $output, $true)
            $completed++
            if ($watch.ElapsedMilliseconds -ge 150 -or $completed -eq $entries.Count) {
                $percent = [Math]::Floor(100.0 * $completed / $entries.Count)
                Set-PhaseProgress 'EXTRACTING POSTGRESQL' ('Extracting PostgreSQL - {0}% ({1} / {2} files)' -f $percent,$completed,$entries.Count) $percent
                $watch.Restart()
            }
        }
    } finally { $zip.Dispose() }
}
function Test-FreePort([int]$Port) {
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $Port)
    $listener.ExclusiveAddressUse = $true
    try { $listener.Start(); return $true } catch { return $false } finally { $listener.Stop() }
}
function Test-Listening([int]$Port) {
    $tcp = New-Object Net.Sockets.TcpClient
    try { $task = $tcp.ConnectAsync('127.0.0.1', $Port); return ($task.Wait(200) -and $tcp.Connected) } catch { return $false } finally { $tcp.Dispose() }
}
function Test-OwnedPostgres([string]$DataDirectory, [string]$Executable) {
    $pidFile = Join-Path $DataDirectory 'postmaster.pid'
    if (-not (Test-Path -LiteralPath $pidFile)) { return $false }
    $pidLines = [IO.File]::ReadAllLines($pidFile)
    $postgresId = 0
    $startSeconds = [long]0
    if ($pidLines.Length -lt 3 -or -not [int]::TryParse($pidLines[0], [ref]$postgresId) -or $postgresId -le 0) {
        throw 'Cannot verify PostgreSQL ownership: invalid postmaster.pid. No process was stopped.'
    }
    $pgProcess = Get-Process -Id $postgresId -ErrorAction SilentlyContinue
    if (-not $pgProcess) { return $false } # PostgreSQL can recover its stale PID file.
    try {
        $recordedData = [IO.Path]::GetFullPath($pidLines[1]).TrimEnd('\')
        $expectedData = [IO.Path]::GetFullPath($DataDirectory).TrimEnd('\')
        $expectedExecutable = [IO.Path]::GetFullPath($Executable)
        if (-not [string]::Equals($recordedData, $expectedData, [StringComparison]::OrdinalIgnoreCase) -or
            -not [string]::Equals($pgProcess.Path, $expectedExecutable, [StringComparison]::OrdinalIgnoreCase) -or
            -not [long]::TryParse($pidLines[2], [ref]$startSeconds)) {
            throw 'Cannot verify PostgreSQL ownership: the live PID does not match this database. No process was stopped.'
        }
        $epoch = [DateTime]::SpecifyKind([DateTime]'1970-01-01', [DateTimeKind]::Utc)
        $recordedStart = $epoch.AddSeconds($startSeconds)
        if ([Math]::Abs(($pgProcess.StartTime.ToUniversalTime() - $recordedStart).TotalSeconds) -gt 3) {
            throw 'Cannot verify PostgreSQL ownership: the recorded PID has been reused. No process was stopped.'
        }
        return $true
    } finally { $pgProcess.Dispose() }
}
function Select-RmrPort([string]$Variable, $Saved, [int]$Default, [int[]]$Taken) {
    $explicit = [Environment]::GetEnvironmentVariable($Variable, 'Process')
    $chosen = 0
    if (-not [string]::IsNullOrWhiteSpace($explicit)) {
        if (-not [int]::TryParse($explicit, [ref]$chosen) -or $chosen -lt 1 -or $chosen -gt 65535) { throw "$Variable must be a port from 1 to 65535." }
        if ($Taken -contains $chosen -or -not (Test-FreePort $chosen)) { throw "$Variable requests port $chosen, which is already in use." }
        return $chosen
    }
    $candidate = 0
    if (-not [int]::TryParse([string]$Saved, [ref]$candidate) -or $candidate -lt 1 -or $candidate -gt 65535) { $candidate = $Default }
    for ($port = $candidate; $port -le 65535; $port++) {
        Assert-Running
        if ($Taken -notcontains $port -and (Test-FreePort $port)) { return $port }
    }
    throw "No free port at or above $candidate."
}
function Test-RmrOwnedPort([int]$Port, [int]$ProcessId) {
    $listeners = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)
    return @($listeners | Where-Object { $_.OwningProcess -eq $ProcessId }).Count -gt 0
}
function Get-RmrBuildFingerprint([string]$Checkout) {
    $files = @((Join-Path $Checkout 'Cargo.toml'), (Join-Path $Checkout 'Cargo.lock'))
    $files += [IO.Directory]::GetFiles((Join-Path $Checkout 'crates'), '*', [IO.SearchOption]::AllDirectories)
    $cargoConfig = Join-Path $Checkout '.cargo\config.toml'
    if (Test-Path -LiteralPath $cargoConfig -PathType Leaf) { $files += $cargoConfig }
    [string[]]$files = $files
    [Array]::Sort($files, [StringComparer]::Ordinal)
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        foreach ($file in $files) {
            Assert-Running
            $name = [Text.Encoding]::UTF8.GetBytes($file.Substring($Checkout.Length) + "`0")
            [void]$hash.TransformBlock($name,0,$name.Length,$null,0)
            $stream = [IO.File]::OpenRead($file)
            try {
                $buffer = New-Object byte[] 65536
                while (($n = $stream.Read($buffer,0,$buffer.Length)) -gt 0) { [void]$hash.TransformBlock($buffer,0,$n,$null,0) }
            } finally { $stream.Dispose() }
        }
        [void]$hash.TransformFinalBlock([byte[]]@(),0,0)
        return ([BitConverter]::ToString($hash.Hash)).Replace('-','').ToLowerInvariant()
    } finally { $hash.Dispose() }
}

function Get-RmrDumpFingerprint([string[]]$Paths) {
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        foreach ($path in $Paths) {
            Assert-Running
            $itemHash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
            $data = [Text.Encoding]::UTF8.GetBytes(([IO.Path]::GetFileName($path) + ':' + $itemHash + "`n"))
            [void]$hash.TransformBlock($data, 0, $data.Length, $null, 0)
        }
        [void]$hash.TransformFinalBlock([byte[]]@(), 0, 0)
        return ([BitConverter]::ToString($hash.Hash)).Replace('-', '').ToLowerInvariant()
    } finally { $hash.Dispose() }
}

function Get-RmrExpectedNamesDigest {
    # Regenerate count and digest after upstream dump-name changes: python -c "import hashlib,pathlib; n=sorted(p.name for p in pathlib.Path('db').glob('*.sql.gz')); print(len(n),hashlib.sha256(('\n'.join(n)+'\n').encode()).hexdigest())"
    return 'e2f70a52416e3d4fc29bb8291e518b3d8eaae44e002fc173aa386b66cbc7a5bb'
}

function Get-RmrImportPaths([string]$Checkout) {
    $paths = [IO.Directory]::GetFiles((Join-Path $Checkout 'db'), '*.sql.gz')
    [Array]::Sort($paths, [StringComparer]::Ordinal)
    $expected = Get-RmrExpectedNamesDigest
    $names = (($paths | ForEach-Object { [IO.Path]::GetFileName($_) }) -join "`n") + "`n"
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $digest = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($names)))).Replace('-','').ToLowerInvariant() }
    finally { $sha.Dispose() }
    if ($paths.Length -ne 212 -or $digest -ne $expected) {
        throw 'Database dump files are incomplete or belong to a newer schema. Update the RMR launcher for this checkout before importing; no game data was changed.'
    }
    return $paths
}

function Install-PostgresRuntime([string]$Bin) {
    $runtimeDlls = @('concrt140.dll','msvcp140.dll','msvcp140_1.dll','msvcp140_2.dll','msvcp140_atomic_wait.dll','msvcp140_codecvt_ids.dll','vcruntime140.dll','vcruntime140_1.dll')
    if (Test-Path -LiteralPath (Join-Path $Bin 'vclibs-installed.ok')) { return }
    $archive = Get-Download 'https://download.microsoft.com/download/4/7/c/47c6134b-d61f-4024-83bd-b9c9ea951c25/Microsoft.VCLibs.x64.14.00.Desktop.appx' 'Microsoft.VCLibs.x64.14.00.Desktop.appx' 'b56a9101f706f9d95f815f5b7fa6efbac972e86573d378b96a07cff5540c5961'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($name in $runtimeDlls) {
            $entry = $zip.GetEntry($name)
            if (-not $entry) { throw "Microsoft runtime archive is missing $name." }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $Bin $name), $true)
        }
    } finally { $zip.Dispose() }
    [IO.File]::WriteAllText((Join-Path $Bin 'vclibs-installed.ok'), '14.0.33321.0', $Utf8)
}

function Get-RmrBinutils {
    # Rust's GNU toolchain ships dlltool but omits the assembler it runs beside itself.
    # Use the matched portable pair so Cargo does not depend on the user's PATH.
    $directory = Join-Path $Runtime 'tools\w64devkit-2.11.0'
    $bin = Join-Path $directory 'w64devkit\bin'
    $files = [ordered]@{
        'dlltool.exe' = 'ce23d0f48c797fb36e7e4e547fffc7a4d58739d39917fab06ce2ff077b9e6ec2'
        'as.exe' = '44b6f3bda1cae50b8774d990b1eef9d65e41637970482c2c1f2ca2be4fdcf183'
    }
    $invalid = @($files.Keys | Where-Object {
        $file = Join-Path $bin $_
        -not [IO.File]::Exists($file) -or (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $files[$_]
    })
    if ($invalid.Count -eq 0) { return $bin }

    Set-Status 'Build tools' 'Preparing portable Windows assembler and DLL tools' 34
    $name = 'w64devkit-x64-2.11.0.7z.exe'
    $url = 'https://github.com/skeeto/w64devkit/releases/download/v2.11.0/' + $name
    $archive = Get-Download $url $name '54c2bb9962e8a30c8270e34c5efc3d5fee579f53087fa7d435b95fbba2af7b2b'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    # Treat the release as an archive. Only extract the two tools we use.
    $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    Set-Status 'Build tools' 'Extracting verified Windows build tools' 34
    Invoke-Native $tar @('-xf',$archive,'-C',$directory,'w64devkit/bin/dlltool.exe','w64devkit/bin/as.exe')
    foreach ($name in $files.Keys) {
        $file = Join-Path $bin $name
        if (-not [IO.File]::Exists($file) -or (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ne $files[$name]) {
            throw "The portable build tool $name failed verification."
        }
    }
    return $bin
}

function Start-RmrWorker {
    $lock = $null; $server = $null; $databaseOwned = $false; $exitCode = 0
    $gamePort = 26000; $webPort = 18080; $databasePort = 5432
    $checkoutAlias = $null
    $originalTemp = $env:TEMP; $originalTmp = $env:TMP
    $previousOriginalTemp = $env:RMR_ORIGINAL_TEMP; $previousOriginalTmp = $env:RMR_ORIGINAL_TMP
    $pgBin = Join-Path $Runtime 'postgres-16.15\pgsql\bin'
    $data = Join-Path $Runtime 'data'
    try {
        try { $lock = [IO.File]::Open((Join-Path $Runtime 'launcher.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
        catch { Write-Host 'RMR is already running from this folder. Use its launcher window.'; return 2 }
        # Native PostgreSQL subprocesses and compiler tools receive short ASCII
        # paths even when the checkout lives under Unicode names or a UNC share.
        $checkoutAlias = New-NativePathAlias -Path $SourceRoot
        $script:Root = $checkoutAlias.Root
        $script:Runtime = Join-Path $Root '.rmr'
        $script:Logs = Join-Path $Root 'logs'
        $script:LogPath = Join-Path $Logs 'launcher.log'
        $script:StopPath = Join-Path $Runtime 'stop.request'
        $script:StatusPath = Join-Path $Runtime 'status.json'
        # Keep worker-local values as well: the embedded payload is invoked as a
        # scriptblock, whose parent locals can otherwise shadow script scope.
        $Root = $checkoutAlias.Root
        $Runtime = Join-Path $Root '.rmr'; $Logs = Join-Path $Root 'logs'
        $LogPath = Join-Path $Logs 'launcher.log'
        $StopPath = Join-Path $Runtime 'stop.request'; $StatusPath = Join-Path $Runtime 'status.json'
        $script:Runtime = $Runtime; $script:Logs = $Logs; $script:LogPath = $LogPath
        $script:StopPath = $StopPath; $script:StatusPath = $StatusPath
        Set-Location -LiteralPath $Root
        [Environment]::CurrentDirectory = $Root
        $temporaryDirectory = Join-Path $Runtime 'tmp'
        [IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
        $env:RMR_ORIGINAL_TEMP = $originalTemp; $env:RMR_ORIGINAL_TMP = $originalTmp
        $env:TEMP = $temporaryDirectory; $env:TMP = $temporaryDirectory
        $pgBin = Join-Path $Runtime 'postgres-16.15\pgsql\bin'
        $data = Join-Path $Runtime 'data'
        if (Test-Path -LiteralPath $StopPath) {
            $request = [IO.File]::ReadAllText($StopPath).Trim()
            $started = [Diagnostics.Process]::GetCurrentProcess().StartTime.ToUniversalTime()
            if ($request -ne $env:RMR_RUN_ID -and (Get-Item -LiteralPath $StopPath).LastWriteTimeUtc -lt $started) { Remove-Item -LiteralPath $StopPath }
        }
        [IO.File]::WriteAllText($LogPath, ('RMR launcher - ' + (Get-Date -Format s) + "`r`n"), $Utf8)
        Set-Status 'Preparing' 'Checking Windows and preparing your local server' 2
        if (-not [Environment]::Is64BitOperatingSystem -or -not [Environment]::Is64BitProcess) { throw 'RMR requires 64-bit Windows and 64-bit Windows PowerShell.' }
        foreach ($required in @('Cargo.lock','Cargo.toml','rmr.yml','db\00_shared.sql.gz','crates\eve-server\Cargo.toml')) {
            if (-not (Test-Path -LiteralPath (Join-Path $Root $required))) { throw "Missing $required. Keep start.bat in the extracted RMR repository." }
        }
        $settingsPath = Join-Path $Runtime 'settings.json'
        $settings = [pscustomobject]@{ clientPath = ''; gamePort = 26000; webPort = 18080; databasePort = 5432; webPortDefaultVersion = 0 }
        if (Test-Path -LiteralPath $settingsPath) {
            $savedSettings = Get-Content -Raw -LiteralPath $settingsPath | ConvertFrom-Json
            foreach ($key in @('clientPath','gamePort','webPort','databasePort','webPortDefaultVersion')) {
                if ($savedSettings.PSObject.Properties[$key]) { $settings.$key = $savedSettings.$key }
            }
        }
        # Move the previous default once; later explicitly selected ports stay put.
        if ([int]$settings.webPortDefaultVersion -lt 2 -and [int]$settings.webPort -eq 8080 -and -not $env:RMR_WEB_PORT) {
            $settings.webPort = 18080
        }
        $selectedPorts = @()
        $gamePort = Select-RmrPort 'RMR_GAME_PORT' $settings.gamePort 26000 $selectedPorts
        $selectedPorts += $gamePort
        $webPort = Select-RmrPort 'RMR_WEB_PORT' $settings.webPort 18080 $selectedPorts
        $selectedPorts += $webPort
        $databasePort = Select-RmrPort 'RMR_DATABASE_PORT' $settings.databasePort 5432 $selectedPorts
        $settings.gamePort = $gamePort; $settings.webPort = $webPort; $settings.databasePort = $databasePort
        $settings.webPortDefaultVersion = 2
        [IO.File]::WriteAllText($settingsPath, ($settings | ConvertTo-Json), $Utf8)
        $script:Status.gamePort = $gamePort; $script:Status.webPort = $webPort
        $script:Status['databasePort'] = $databasePort

        $binaryDir = Join-Path $Root 'target\x86_64-pc-windows-gnu\release'
        $serverExe = Join-Path $binaryDir 'eve-server.exe'
        $fakeClientExe = Join-Path $binaryDir 'fakeclient.exe'
        $buildStamp = Join-Path $Runtime 'build.json'
        $fingerprint = Get-RmrBuildFingerprint $Root
        $needsBuild = $true
        if ((Test-Path -LiteralPath $buildStamp) -and (Test-Path -LiteralPath $serverExe) -and (Test-Path -LiteralPath $fakeClientExe)) {
            try {
                $old = Get-Content -LiteralPath $buildStamp -Raw | ConvertFrom-Json
                if ($old.fingerprint -eq $fingerprint -and $old.serverHash -eq (Get-FileHash -LiteralPath $serverExe -Algorithm SHA256).Hash -and $old.clientHash -eq (Get-FileHash -LiteralPath $fakeClientExe -Algorithm SHA256).Hash) { $needsBuild = $false }
            } catch { Write-Log 'Build cache could not be verified; rebuilding.' }
        }
        if ($needsBuild) {
        Set-Status 'Toolchain' 'Preparing portable Rust 1.97 (first run downloads the compiler)' 8
        $env:CARGO_HOME = Join-Path $Runtime 'cargo'
        $env:RUSTUP_HOME = Join-Path $Runtime 'rustup'
        $env:RUSTUP_TOOLCHAIN = '1.97.0-x86_64-pc-windows-gnu'
        $env:CARGO_TARGET_DIR = Join-Path $Root 'target'
        $env:CARGO_BUILD_TARGET = 'x86_64-pc-windows-gnu'
        $env:CARGO_TERM_COLOR = 'never'
        $toolDir = Join-Path $Runtime 'tools'
        [IO.Directory]::CreateDirectory($toolDir) | Out-Null
        $rustup = Join-Path $toolDir 'rustup.exe'
        if (-not (Test-Path -LiteralPath $rustup)) {
            $bootstrap = Get-Download 'https://static.rust-lang.org/rustup/archive/1.28.2/x86_64-pc-windows-gnu/rustup-init.exe' 'rustup-init.exe' 'ccbfd951d8024856043b3a0c3903a59f39937bce8d3074768b0d3da55f21e817'
            if ([string]::IsNullOrWhiteSpace($bootstrap) -or -not [IO.File]::Exists($bootstrap)) { throw 'The Rust download did not return a valid file path. Run start.bat again to retry.' }
            [IO.File]::Copy($bootstrap, $rustup, $true)
        }
        $toolchainReady = Join-Path $Runtime 'rust-toolchain-installed.ok'
        if (-not (Test-Path -LiteralPath $toolchainReady) -or -not (Test-Path -LiteralPath (Join-Path $env:RUSTUP_HOME 'toolchains\1.97.0-x86_64-pc-windows-gnu\bin\rustc.exe'))) {
            # Pin the minimal profile archives from Rust's official 1.97.0 manifest.
            # Prefill rustup's verified cache so every large download uses the live
            # byte-based progress display, even though rustup runs without a console.
            $components = @(
                @{ name = 'rustc'; hash = 'c5b717977488eb02b56fa89c3629a7b7ebdaf1c6aecc3a6897cb924aa128c138' }
                @{ name = 'cargo'; hash = '8ef34ae3c90879860103fdb00a0120940f20f3c77b8647372c54aa3a6de21b3d' }
                @{ name = 'rust-std'; hash = '98e6ef703b003b1f7d75c3249a831f4ac07f0deb41b0e1dfe41054847ac2816f' }
                @{ name = 'rust-mingw'; hash = '5d0bb89c6652df6dd759247c3c5c92d8d681f0fe1d7650bc5db9f31f179844ee' }
            )
            $rustupDownloads = Join-Path $env:RUSTUP_HOME 'downloads'
            [IO.Directory]::CreateDirectory($rustupDownloads) | Out-Null
            $componentIndex = 0
            foreach ($component in $components) {
                Assert-Running
                $componentIndex++
                Set-Status 'Downloading Rust' ("Compiler component {0} of {1}: {2}" -f $componentIndex,$components.Count,$component.name) (8 + 2 * $componentIndex)
                $cachedArchive = Join-Path $rustupDownloads $component.hash
                if ([IO.File]::Exists($cachedArchive) -and (Get-FileHash -LiteralPath $cachedArchive -Algorithm SHA256).Hash -eq $component.hash) { continue }
                $archiveName = $component.name + '-1.97.0-x86_64-pc-windows-gnu.tar.xz'
                $componentArchive = Get-Download ('https://static.rust-lang.org/dist/2026-07-09/' + $archiveName) $archiveName $component.hash
                [IO.File]::Copy($componentArchive, $cachedArchive, $true)
            }
            Set-Status 'Installing Rust' 'Downloads complete. Installing the portable compiler...' 18
            Invoke-Native $rustup @('toolchain','install','1.97.0-x86_64-pc-windows-gnu','--profile','minimal','--no-self-update')
            [IO.File]::WriteAllText($toolchainReady, '1.97.0-x86_64-pc-windows-gnu', $Utf8)
        }
        } else { Write-Log 'Using the verified existing server build.' }
        Set-Status 'PostgreSQL' 'Preparing portable PostgreSQL 16.15' 20
        $pgDir = Join-Path $Runtime 'postgres-16.15'
        if (-not (Test-Path -LiteralPath (Join-Path $pgDir 'installed.ok'))) {
            $archive = Get-Download 'https://get.enterprisedb.com/postgresql/postgresql-16.15-1-windows-x64-binaries.zip' 'postgresql-16.15-1-windows-x64-binaries.zip' '25e6fcdfb8caec38691bf461125e7564508760666f7b8e5dc6a5f0818f58f81e'
            Set-Status 'Extracting PostgreSQL' 'Download verified. Preparing the database tools...' 22
            Expand-PortableZip $archive $pgDir
            [IO.File]::WriteAllText((Join-Path $pgDir 'installed.ok'), '16.15', $Utf8)
        }
        # Install app-local Microsoft runtime DLLs here; no machine-wide installer.
        Install-PostgresRuntime $pgBin
        Invoke-Native (Join-Path $pgBin 'postgres.exe') @('--version')
        if ($needsBuild) {
            $binutils = Get-RmrBinutils
            Set-Status 'Building' 'Compiling RMR; later launches reuse the verified build' 35
            $previousPath = $env:Path
            try {
                $env:Path = $binutils + ';' + $previousPath
                Invoke-Native $rustup @('run','1.97.0-x86_64-pc-windows-gnu','cargo','build','--release','--locked','-p','eve-server','--bins','--target','x86_64-pc-windows-gnu')
            } finally { $env:Path = $previousPath }
            if (-not (Test-Path -LiteralPath $serverExe) -or -not (Test-Path -LiteralPath $fakeClientExe)) { throw 'The build did not produce both server and fakeclient executables.' }
            $buildRecord = [pscustomobject]@{ fingerprint = $fingerprint; serverHash = (Get-FileHash -LiteralPath $serverExe -Algorithm SHA256).Hash; clientHash = (Get-FileHash -LiteralPath $fakeClientExe -Algorithm SHA256).Hash }
            [IO.File]::WriteAllText($buildStamp, ($buildRecord | ConvertTo-Json), $Utf8)
        }

        Set-Status 'Database' 'Preparing your private PostgreSQL database' 50
        $passwordPath = Join-Path $Runtime 'database-password.txt'
        if (-not (Test-Path -LiteralPath $passwordPath)) {
            if (Test-Path -LiteralPath (Join-Path $data 'PG_VERSION')) { throw 'Database password file is missing. Restore .rmr/database-password.txt from your backup.' }
            $random = New-Object byte[] 32
            $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
            try { $rng.GetBytes($random) } finally { $rng.Dispose() }
            [IO.File]::WriteAllText($passwordPath, ([BitConverter]::ToString($random).Replace('-','').ToLowerInvariant()), $Utf8)
        }
        $password = [IO.File]::ReadAllText($passwordPath).Trim()
        # Existing PostgreSQL client/service settings must not redirect or alter
        # the private cluster. These changes affect only this worker and its children.
        foreach ($variable in @(Get-ChildItem Env: | Where-Object { $_.Name -like 'PG*' })) {
            [Environment]::SetEnvironmentVariable($variable.Name, $null, 'Process')
        }
        $env:PGPASSWORD = $password
        $env:PGCLIENTENCODING = 'UTF8'
        $env:PGCONNECT_TIMEOUT = '5'
        if (-not (Test-Path -LiteralPath (Join-Path $data 'PG_VERSION'))) {
            if (Test-Path -LiteralPath $data) { throw 'Incomplete data folder found. Preserve it and move it aside before retrying.' }
            $staging = Join-Path $Runtime ('data-init-' + [Guid]::NewGuid().ToString('N'))
            Invoke-Native (Join-Path $pgBin 'initdb.exe') @('-D',$staging,'-U','rmr','--pwfile',$passwordPath,'--encoding=UTF8','--locale=C','--auth-host=scram-sha-256','--auth-local=scram-sha-256')
            # Both absolute paths are fixed children of this launcher's private runtime.
            [IO.Directory]::Move($staging, $data)
        }
        if ([IO.File]::ReadAllText((Join-Path $data 'PG_VERSION')).Trim() -ne '16') { throw 'This launcher requires a PostgreSQL 16 data directory.' }
        $pgCtl = Join-Path $pgBin 'pg_ctl.exe'
        if (Test-OwnedPostgres $data (Join-Path $pgBin 'postgres.exe')) {
            Invoke-Native $pgCtl @('status','-D',$data)
            $databaseOwned = $true
            Invoke-Native $pgCtl @('stop','-D',$data,'-m','fast','-w','-t','60') -Timeout 90
            $databaseOwned = $false
        }
        if (-not (Test-FreePort $databasePort)) { throw "PostgreSQL port $databasePort became busy; retry to choose another port." }
        $databaseOwned = $true
        $pgOptions = '-h 127.0.0.1 -p ' + $databasePort
        Invoke-Native $pgCtl @('start','-D',$data,'-l',(Join-Path $Logs 'postgres.log'),'-o',$pgOptions,'-w','-t','60') -Timeout 90
        $psql = Join-Path $pgBin 'psql.exe'
        $pgArgs = @('-X','-w','-h','127.0.0.1','-p',[string]$databasePort,'-U','rmr','-v','ON_ERROR_STOP=1')
        $exists = Invoke-Native $psql ($pgArgs + @('-d','postgres','-Atc',"SELECT 1 FROM pg_database WHERE datname='rmr'")) -Capture
        if ($exists -ne '1') { Invoke-Native (Join-Path $pgBin 'createdb.exe') @('-h','127.0.0.1','-p',[string]$databasePort,'-U','rmr','-E','UTF8','-T','template0','rmr') }
        $dbArgs = $pgArgs + @('-d','rmr')
        $dumpPaths = $null
        $marker = Invoke-Native $psql ($dbArgs + @('-Atc',"SELECT to_regclass('public.rmr_launcher_imports') IS NOT NULL")) -Capture
        if ($marker -ne 't') {
            $dumpPaths = @(Get-RmrImportPaths $Root)
            $objects = Invoke-Native $psql ($dbArgs + @('-Atc',"SELECT (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public') + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public')")) -Capture
            if ($objects -ne '0') { throw 'Existing game database has no RMR launcher checkpoint; it was left untouched.' }
            Invoke-Native $psql ($dbArgs + @('-q','-c','CREATE TABLE public.rmr_launcher_imports (name text PRIMARY KEY, imported_at timestamptz NOT NULL DEFAULT now());'))
        }
        $done = Invoke-Native $psql ($dbArgs + @('-Atc','SELECT name FROM public.rmr_launcher_imports ORDER BY name;')) -Capture
        $completed = @{}
        foreach ($name in ($done -split "`n")) { if ($name.Trim()) { $completed[$name.Trim()] = $true } }
        if ($completed.ContainsKey('__rmr_complete__')) { $dumps = @() }
        else {
            if ($null -eq $dumpPaths) { $dumpPaths = @(Get-RmrImportPaths $Root) }
            $dumps = @($dumpPaths | ForEach-Object { Get-Item -LiteralPath $_ })
            Set-Status 'Database' 'Verifying the database snapshot before importing' 50
            $manifestName = '__rmr_manifest__:' + (Get-RmrDumpFingerprint $dumpPaths)
            $namesCheckpoint = '__rmr_names__:' + (Get-RmrExpectedNamesDigest)
            $priorManifests = @($completed.Keys | Where-Object { $_ -like '__rmr_manifest__:*' })
            if ($priorManifests.Count -gt 1 -or ($priorManifests.Count -eq 1 -and $priorManifests[0] -ne $manifestName)) {
                throw 'The database dumps changed during an incomplete import. Restore the original dump snapshot before retrying; existing data was left untouched.'
            }
            if ($priorManifests.Count -eq 0) {
                if ($completed.Count -ne 0) { throw 'An incomplete import has no snapshot checkpoint; existing data was left untouched.' }
                Invoke-Native $psql ($dbArgs + @('-q','-c',"INSERT INTO public.rmr_launcher_imports(name) VALUES ('$manifestName'), ('$namesCheckpoint')"))
            } elseif (-not $completed.ContainsKey($namesCheckpoint)) {
                throw 'The incomplete import has an unexpected dump-name checkpoint; existing data was left untouched.'
            }
        }
        $index = 0
        foreach ($dump in $dumps) {
            Assert-Running
            $index++
            if ($completed.ContainsKey($dump.Name)) { continue }
            Set-Status 'Importing universe' ("{0}/{1} - {2}" -f $index,$dumps.Count,$dump.Name) (50 + [int](35 * $index / $dumps.Count))
            $sqlPath = Join-Path $Runtime 'import.sql'
            $inputFile = [IO.File]::OpenRead($dump.FullName)
            try {
                if ($inputFile.Length -lt 18) { throw "Invalid gzip dump: $($dump.Name)" }
                $header = New-Object byte[] 2
                if ($inputFile.Read($header,0,2) -ne 2 -or $header[0] -ne 0x1f -or $header[1] -ne 0x8b) { throw "Invalid gzip header: $($dump.Name)" }
                $footer = New-Object byte[] 4
                [void]$inputFile.Seek(-4, [IO.SeekOrigin]::End)
                if ($inputFile.Read($footer,0,4) -ne 4) { throw "Missing gzip footer: $($dump.Name)" }
                $expectedSize = [BitConverter]::ToUInt32($footer,0)
                $inputFile.Position = 0
                $gzip = New-Object IO.Compression.GzipStream($inputFile, [IO.Compression.CompressionMode]::Decompress)
                try {
                    $outputFile = [IO.File]::Create($sqlPath)
                    try {
                        $gzip.CopyTo($outputFile)
                        if ($outputFile.Length -eq 0 -or ($outputFile.Length % 4294967296) -ne $expectedSize) { throw "Truncated gzip dump: $($dump.Name)" }
                    } finally { $outputFile.Dispose() }
                } finally { $gzip.Dispose() }
            } finally { $inputFile.Dispose() }
            $safeName = $dump.Name.Replace("'", "''")
            [IO.File]::AppendAllText($sqlPath, "`nINSERT INTO public.rmr_launcher_imports(name) VALUES ('$safeName');`n", $Utf8)
            # Each dump and its checkpoint commit together, so interrupted setup resumes safely.
            Invoke-Native $psql ($dbArgs + @('-q','-o','NUL','--single-transaction','-f',$sqlPath)) -Timeout 900
        }
        if (-not $completed.ContainsKey('__rmr_complete__')) { Invoke-Native $psql ($dbArgs + @('-q','-c',"INSERT INTO public.rmr_launcher_imports(name) VALUES ('__rmr_complete__')")) }
        $sqlPath = Join-Path $Runtime 'import.sql'
        if (Test-Path -LiteralPath $sqlPath) { Remove-Item -LiteralPath $sqlPath }

        if ($env:RMR_CHECK -ne '1' -and $env:RMR_NO_CLIENT -ne '1') {
            Set-Status 'Client' 'Finding and preparing the RMR game client' 87
            Prepare-RmrGameClient $settings
        }

        Set-Status 'Starting' 'Loading the universe and starting the game server' 92
        $env:RMR_CONFIG = Join-Path $Root 'rmr.yml'
        if ($env:RMR_CHECK -eq '1') { $env:RMR_BOTS = '0' }
        $env:RMR_STRINGS = Join-Path $Root 'crates\eve-marshal\tests\data\strings.txt'
        $env:RMR_DATABASE = 'postgres://rmr:' + $password + '@127.0.0.1:' + $databasePort + '/rmr'
        $env:RMR_BIND = '127.0.0.1:' + $gamePort
        $env:RMR_WEB = '127.0.0.1:' + $webPort
        $env:RMR_LOG_FILE = Join-Path $Logs 'server.log'
        $env:RMR_DUMP_DIR = Join-Path $Logs 'dumps'
        [IO.Directory]::CreateDirectory($env:RMR_DUMP_DIR) | Out-Null
        $binaryDir = Join-Path $Root 'target\x86_64-pc-windows-gnu\release'
        $server = New-NativeProcess (Join-Path $binaryDir 'eve-server.exe') @()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-RmrOwnedPort $gamePort $server.Process.Id) -or -not (Test-RmrOwnedPort $webPort $server.Process.Id)) {
            Assert-Running; Read-NativeOutput $server
            if ($server.Process.HasExited) { Read-NativeOutput $server; throw "Server exited with code $($server.Process.ExitCode). See logs/launcher.log." }
            if ($watch.Elapsed.TotalSeconds -gt 180) { throw 'Server startup timed out. See logs/server.log.' }
            Start-Sleep -Milliseconds 200
        }
        Assert-Running
        if ($server.Process.HasExited) { throw "The server stopped before setup finished. See logs/server.log." }
        [IO.File]::WriteAllText((Join-Path $Runtime 'setup-complete'), (Get-Date -Format s), $Utf8)
        $script:Status.ready = $true
        Set-Status 'Online' ("Client: 127.0.0.1:{0}   |   Dashboard: http://127.0.0.1:{1}" -f $gamePort,$webPort) 100
        Assert-Running
        if ($env:RMR_CHECK -eq '1') {
            Set-Status 'Checking' 'Testing the server handshake and login' 100
            $checkOutput = Invoke-Native $fakeClientExe @("127.0.0.1:$gamePort",'--no-cache') -Capture -Timeout 90
            if ($checkOutput -notmatch '(?i)session change' -or $checkOutput -match '(?im)(FAILED|\bFAIL\b|-> refused:|-> no reply|-> no usable reply)') { throw 'Protocol/login check did not complete successfully. See logs/launcher.log.' }
            Write-Log 'Protocol/login check passed.'
        } else {
            # EVE is launched only by the Launch EVE button, never on server startup.
            while (-not $server.Process.HasExited) { Assert-Running; Read-NativeOutput $server; Start-Sleep -Milliseconds 100 }
            Read-NativeOutput $server
            throw "Server exited unexpectedly with code $($server.Process.ExitCode). See logs/server.log."
        }

    } catch [OperationCanceledException] { Write-Log $_.Exception.Message }
    catch {
        $exitCode = 1
        $script:Status.error = $true
        Set-Status 'Needs attention' $_.Exception.Message $script:Status.progress
        Write-Log $_.ScriptStackTrace
    } finally {
        if ($lock) {
            try {
                $script:Stopping = $true
                $script:Status.ready = $false
                if ($server) {
                    Stop-NativeProcess $server.Process
                    Read-NativeOutput $server
                    $server.Process.Dispose()
                }
                if ($databaseOwned) {
                    Write-Log 'Stopping this launcher''s PostgreSQL database...'
                    try {
                        if (Test-OwnedPostgres $data (Join-Path $pgBin 'postgres.exe')) {
                            Invoke-Native (Join-Path $pgBin 'pg_ctl.exe') @('stop','-D',$data,'-m','fast','-w','-t','60') -Timeout 90
                        }
                        $databaseOwned = $false
                    }
                    catch { $exitCode = 1; $script:Status.error = $true; Set-Status 'Needs attention' ('Database shutdown failed: ' + $_.Exception.Message) 100 }
                }
                $env:PGPASSWORD = $null; $env:RMR_DATABASE = $null
                $script:Status.stopped = $true
                if (-not $script:Status.error) { Set-Status 'Stopped' 'Server stopped. Your universe is saved for the next launch.' 100 }
                else { [IO.File]::WriteAllText($StatusPath, ($script:Status | ConvertTo-Json -Compress), $Utf8) }
            } finally {
                # Keep the path available if PostgreSQL could not be stopped.
                # A later launcher can reuse it and recover the private cluster.
                if ($databaseOwned -and $checkoutAlias) { $checkoutAlias.Created = $false }
                try {
                    try {
                        Set-Location -LiteralPath $SourceRoot
                        [Environment]::CurrentDirectory = $SourceRoot
                    } catch {
                        Set-Location -LiteralPath $env:SystemRoot
                        [Environment]::CurrentDirectory = $env:SystemRoot
                    }
                    $env:TEMP = $originalTemp; $env:TMP = $originalTmp
                    $env:RMR_ORIGINAL_TEMP = $previousOriginalTemp; $env:RMR_ORIGINAL_TMP = $previousOriginalTmp
                    # UI status uses the original files; remove only the exact
                    # mapping we created, after all native tools have finished.
                    Remove-NativePathAlias $checkoutAlias
                } catch {
                    $exitCode = 1
                    Write-Host ('Temporary drive cleanup needs attention: ' + $_.Exception.Message)
                } finally {
                    $Root = $SourceRoot
                    $Runtime = Join-Path $Root '.rmr'; $Logs = Join-Path $Root 'logs'
                    $LogPath = Join-Path $Logs 'launcher.log'
                    $StopPath = Join-Path $Runtime 'stop.request'; $StatusPath = Join-Path $Runtime 'status.json'
                    $script:Root = $SourceRoot
                    $script:Runtime = Join-Path $Root '.rmr'
                    $script:Logs = Join-Path $Root 'logs'
                    $script:LogPath = Join-Path $Logs 'launcher.log'
                    $script:StopPath = Join-Path $Runtime 'stop.request'
                    $script:StatusPath = Join-Path $Runtime 'status.json'
                    $lock.Dispose()
                }
            }
        }
    }
    return $exitCode
}


# Client preparation payload. PowerShell 5.1; no runtime dependency on the source tree.
function Test-RmrClient {
    param([string]$Path)
    try {
        if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
        $dir = [IO.Path]::GetFullPath($Path)
        foreach ($file in @('boot.ini', 'bin\exefile.exe', 'script\sys\autoexec.py', 'script\compiled.code', 'res.stuff')) {
            if (-not (Test-Path -LiteralPath (Join-Path $dir $file) -PathType Leaf)) { return $false }
        }
        if (-not (Test-Path -LiteralPath (Join-Path $dir 'lib') -PathType Container)) { return $false }
        $boot = [IO.File]::ReadAllText((Join-Path $dir 'boot.ini'))
        $main = [regex]::Match($boot, '(?ims)^\s*\[main\]\s*$([\s\S]*?)(?=^\s*\[|\z)')
        if (-not $main.Success) { return $false }
        return ($main.Groups[1].Value -match '(?im)^\s*version\s*=\s*2\.10\s*$' -and
                $main.Groups[1].Value -match '(?im)^\s*build\s*=\s*3913\s*$' -and
                $main.Groups[1].Value -match '(?im)^\s*role\s*=\s*client\s*$')
    } catch { return $false }
}

function Find-RmrClients {
    param([string]$InvocationDirectory, [string]$RepositoryRoot, [string]$SavedClientDirectory)
    if ($SavedClientDirectory -and (Test-RmrClient $SavedClientDirectory)) {
        return @([IO.Path]::GetFullPath($SavedClientDirectory))
    }
    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    $found = New-Object 'System.Collections.Generic.List[string]'
    $queue = New-Object 'System.Collections.Generic.Queue[object]'
    $roots = @($InvocationDirectory, $RepositoryRoot)
    if ($RepositoryRoot) { $roots += (Split-Path -Parent $RepositoryRoot) }
    foreach ($root in $roots) {
        if (-not $root) { continue }
        try {
            $full = [IO.Path]::GetFullPath($root).TrimEnd('\')
            # Never enumerate a drive root or a user profile as a search root.
            if ($full -eq [IO.Path]::GetPathRoot($full).TrimEnd('\') -or ($env:USERPROFILE -and $full -eq $env:USERPROFILE.TrimEnd('\'))) { continue }
            if (Test-Path -LiteralPath $full -PathType Container) { $queue.Enqueue(@($full, 0)) }
        } catch { }
    }
    $count = 0; $entries = 0
    while ($queue.Count -gt 0 -and $count -lt 3000 -and $entries -lt 3000) {
        $item = $queue.Dequeue(); $dir = [string]$item[0]; $depth = [int]$item[1]
        if (-not $seen.Add($dir)) { continue }
        $count++
        try {
            $info = Get-Item -LiteralPath $dir -Force -ErrorAction Stop
            if (($info.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
            if (Test-RmrClient $dir) { $found.Add($dir); continue }
            if ($depth -ge 3) { continue }
            foreach ($childPath in [IO.Directory]::EnumerateDirectories($dir)) {
                $entries++
                if ($entries -gt 3000) { break }
                $child = Get-Item -LiteralPath $childPath -Force -ErrorAction Stop
                if ($child.Name -in @('.git', '.local', '.rmr', '.build', 'runtime', 'builds', 'trinity', 'node_modules', 'target', 'cache', 'logs', 'capture', '.rmr-backup')) { continue }
                if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
                if ($queue.Count -ge 3000) { break }
                $queue.Enqueue(@($child.FullName, ($depth + 1)))
            }
        } catch { continue }
    }
    return $found.ToArray()
}

function Select-RmrClientFolder {
    param([string]$InvocationDirectory, [string]$Description)
    Add-Type -AssemblyName System.Windows.Forms
    $dialog = New-Object Windows.Forms.FolderBrowserDialog
    try {
        $dialog.Description = $Description
        $dialog.ShowNewFolderButton = $false
        if (Test-Path -LiteralPath $InvocationDirectory -PathType Container) { $dialog.SelectedPath = $InvocationDirectory }
        if ($dialog.ShowDialog() -eq [Windows.Forms.DialogResult]::OK) { return $dialog.SelectedPath }
        return $null
    } finally { $dialog.Dispose() }
}

function Resolve-RmrClient {
    param([string]$InvocationDirectory, [string]$RepositoryRoot, [string]$SavedClientDirectory, [string]$ClientDirectory)
    if ($ClientDirectory) {
        if (-not (Test-RmrClient $ClientDirectory)) { throw "Client directory is not an EVE 2.10 build 3913 client with its required files: $ClientDirectory" }
        return [IO.Path]::GetFullPath($ClientDirectory)
    }
    $matches = @(Find-RmrClients -InvocationDirectory $InvocationDirectory -RepositoryRoot $RepositoryRoot -SavedClientDirectory $SavedClientDirectory)
    if ($matches.Count -eq 1) { return $matches[0] }
    $description = if ($matches.Count -gt 1) { 'Multiple compatible EVE clients were found. Select the client folder to prepare. Cancel to return to server setup.' } else { 'Select your EVE 2.10 build 3913 client folder. Cancel to return to server setup.' }
    $selected = Select-RmrClientFolder -InvocationDirectory $InvocationDirectory -Description $description
    if (-not $selected) { return $null }
    if (-not (Test-RmrClient $selected)) { throw "Selected folder is not an EVE 2.10 build 3913 client with its required files: $selected" }
    return [IO.Path]::GetFullPath($selected)
}

function Save-RmrOriginal {
    param([string]$ClientDirectory, [string]$RelativePath)
    $source = Join-Path $ClientDirectory $RelativePath
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { return }
    $base = Join-Path (Join-Path $ClientDirectory '.rmr-backup') $RelativePath
    $parent = Split-Path -Parent $base
    if (-not (Test-Path -LiteralPath $parent)) { [void](New-Item -ItemType Directory -Path $parent -Force) }
    $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $destination = $base
    if (Test-Path -LiteralPath $base) {
        if ((Get-FileHash -LiteralPath $base -Algorithm SHA256).Hash -eq $sourceHash) { return }
        $destination = "$base.$sourceHash"
        if (Test-Path -LiteralPath $destination) { return }
    }
    [IO.File]::Copy($source, $destination, $false)
}

function Write-RmrFile {
    param([string]$ClientDirectory, [string]$RelativePath, [byte[]]$Bytes)
    $path = Join-Path $ClientDirectory $RelativePath
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $existing = [IO.File]::ReadAllBytes($path)
        if ($existing.Length -eq $Bytes.Length -and [Convert]::ToBase64String($existing) -eq [Convert]::ToBase64String($Bytes)) { return }
        Save-RmrOriginal -ClientDirectory $ClientDirectory -RelativePath $RelativePath
    }
    $parent = Split-Path -Parent $path
    if (-not (Test-Path -LiteralPath $parent)) { [void](New-Item -ItemType Directory -Path $parent -Force) }
    $temporary = Join-Path $parent ('.rmr-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        if (Test-Path -LiteralPath $path -PathType Leaf) { [IO.File]::Replace($temporary, $path, [System.Management.Automation.Language.NullString]::Value) }
        else { [IO.File]::Move($temporary, $path) }
    } finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force } }
}

function Prepare-RmrClient {
    param([Parameter(Mandatory=$true)][string]$ClientDirectory, [string]$ServerBind = '127.0.0.1', [int]$ServerPort = 26000)
    if (-not (Test-RmrClient $ClientDirectory)) { throw "Invalid EVE 2.10 build 3913 client: $ClientDirectory" }
    if ($ServerPort -lt 1 -or $ServerPort -gt 65535) { throw 'Server port must be between 1 and 65535.' }
    if ($ServerBind -in @('0.0.0.0', '::', '[::]', '*', '')) { $ServerBind = '127.0.0.1' }
    if ($ServerBind -notmatch '^[A-Za-z0-9.:-]+$') { throw 'Server bind address cannot be used safely in the client launcher.' }
    $ClientDirectory = [IO.Path]::GetFullPath($ClientDirectory)
    $autoPath = Join-Path $ClientDirectory 'script\sys\autoexec.py'
    # Latin-1 round-trips every original byte, including legacy non-ASCII text.
    $encoding = [Text.Encoding]::GetEncoding(28591)
    $auto = $encoding.GetString([IO.File]::ReadAllBytes($autoPath))
    $nl = if ($auto.Contains("`r`n")) { "`r`n" } else { "`n" }
    $anchor = '        if Hiword(d3d9) == 5 and d3d9 >= Long(5, 3, 1, 904): ver = (9, 0, "c") #NT'
    $marker = '        if Hiword(d3d9) >= 6: ver = (9, 0, "c")'
    $knownBlock = @(
        '    d3d9 = GetFileVersion("d3d9.dll")',
        '    if (d3d9):',
        '        ver = (9,0,"")',
        '        if Hiword(d3d9) == 4 and d3d9 >= Long(4, 9, 0, 902): ver = (9, 0, "b") #win9x',
        '        if Hiword(d3d9) == 5 and d3d9 >= Long(5, 3, 1, 902): ver = (9, 0, "b") #NT',
        '',
        '        if Hiword(d3d9) == 4 and d3d9 >= Long(4, 9, 0, 904): ver = (9, 0, "c") #win9x',
        $anchor
    ) -join $nl
    if ([regex]::Matches($auto, [regex]::Escape($knownBlock)).Count -ne 1 -or [regex]::Matches($auto, [regex]::Escape($anchor)).Count -ne 1) { throw 'The DirectX detector in autoexec.py is unfamiliar; no client files were changed.' }
    $markers = [regex]::Matches($auto, [regex]::Escape($marker)).Count
    $patchText = $anchor + $nl + '        # RMR: Modern Windows uses OS DLL versions 6.x / 10.x. Its d3d9' + $nl + '        # runtime supports 9.0c; the old tests misidentified it as 9.0.' + $nl + $marker
    if ($markers -gt 1 -or ($markers -eq 1 -and -not $auto.Contains($patchText))) { throw 'The DirectX detector has an unfamiliar RMR patch; no client files were changed.' }
    if ($markers -eq 0) {
        $auto = $auto.Replace($anchor, $patchText)
    }
    $start = $script:RmrClientStart.Replace('set "RMR_SERVER_HOST=127.0.0.1"', ('set "RMR_SERVER_HOST=' + $ServerBind + '"')).Replace('set "RMR_SERVER_PORT=26000"', ('set "RMR_SERVER_PORT=' + $ServerPort + '"'))
    Write-RmrFile $ClientDirectory 'script\sys\autoexec.py' ($encoding.GetBytes($auto))
    Write-RmrFile $ClientDirectory 'script\rmr_connection.py' ([Text.Encoding]::ASCII.GetBytes($script:RmrClientConnection))
    Write-RmrFile $ClientDirectory 'script\rmr_login.py' ([Text.Encoding]::ASCII.GetBytes($script:RmrClientLogin))
    Write-RmrFile $ClientDirectory 'Start.bat' ([Text.Encoding]::ASCII.GetBytes($start))
    return [pscustomobject]@{ ClientDirectory = $ClientDirectory; Launcher = (Join-Path $ClientDirectory 'Start.bat') }
}

function Prepare-RmrGameClient($Settings) {
    $saved = if ($Settings.PSObject.Properties['clientPath']) { [string]$Settings.clientPath } else { '' }
    if ($env:RMR_CLIENT_DIRECTORY) {
        if (-not (Test-RmrClient $env:RMR_CLIENT_DIRECTORY)) { throw 'The --client path is not a complete EVE 2.10 build 3913 client.' }
        $saved = [IO.Path]::GetFullPath($env:RMR_CLIENT_DIRECTORY)
    }
    # Search the real checkout and invocation folder, not the temporary drive alias.
    $invocation = $env:RMR_INVOCATION_DIRECTORY
    if (-not $invocation) { $invocation = $SourceRoot }
    $matches = @(Find-RmrClients -InvocationDirectory $invocation -RepositoryRoot $SourceRoot -SavedClientDirectory $saved)
    $clientPath = if ($matches.Count -eq 1) { $matches[0] } else { $null }
    while (-not $clientPath) {
        Assert-Running
        if ($env:RMR_MODE -eq 'worker') {
            $selection = Join-Path $Runtime 'client-selection.txt'
            if (Test-Path -LiteralPath $selection) { Remove-Item -LiteralPath $selection -Force }
            $script:Status['needsClient'] = $true
            $script:Status['clientRequestId'] = [Guid]::NewGuid().ToString('N')
            $detail = if ($matches.Count -gt 1) { 'Multiple clients found. Select the EVE 2.10 build 3913 client folder.' } else { 'Select your EVE 2.10 build 3913 client folder.' }
            Set-Status 'Choose your client' $detail 87
            while (-not (Test-Path -LiteralPath $selection)) { Assert-Running; Start-Sleep -Milliseconds 200 }
            $selected = [IO.File]::ReadAllText($selection).Trim()
            $script:Status['needsClient'] = $false
            if (-not $selected) { throw [OperationCanceledException]::new('Client selection cancelled. Run start.bat when your client is ready.') }
        } else {
            $selected = Read-Host 'Path to EVE 2.10 build 3913 (client folder)'
            if (-not $selected) { throw [OperationCanceledException]::new('Client selection cancelled.') }
        }
        if (Test-RmrClient $selected) { $clientPath = [IO.Path]::GetFullPath($selected) }
        else { Write-Log 'That folder is not a complete EVE 2.10 build 3913 client. Please select another folder.' }
    }
    Set-Status 'Preparing client' 'Backing up and configuring your EVE client' 89
    $port = [int]$script:Status['gamePort']
    if ($port -lt 1 -or $port -gt 65535) { throw 'The server game port is not ready; cannot configure the client.' }
    $prepared = Prepare-RmrClient -ClientDirectory $clientPath -ServerBind '127.0.0.1' -ServerPort $port
    $Settings | Add-Member -NotePropertyName clientPath -NotePropertyValue $prepared.ClientDirectory -Force
    [IO.File]::WriteAllText((Join-Path $Runtime 'settings.json'), ($Settings | ConvertTo-Json), $Utf8)
    $script:Status['clientPath'] = $prepared.ClientDirectory
    Write-Log ('Client ready: ' + $prepared.ClientDirectory + ' (EVE build 3913; 127.0.0.1:' + $port + ')')
}

function Start-RmrGameClient {
    param([string]$ClientPath, [int]$GamePort = 0)
    # Self-contained: the UI stores this function body for its callback.
    $exe = Join-Path $ClientPath 'bin\exefile.exe'
    $launcher = Join-Path $ClientPath 'Start.bat'
    foreach ($path in @($exe, $launcher, (Join-Path $ClientPath 'script\rmr_connection.py'), (Join-Path $ClientPath 'script\rmr_login.py'))) {
        if (-not [IO.File]::Exists($path)) { throw 'The prepared EVE client is no longer present. Restart setup.' }
    }
    if ($GamePort -eq 0) {
        $match = [regex]::Match([IO.File]::ReadAllText($launcher), '(?im)^if not defined RMR_SERVER_PORT set "RMR_SERVER_PORT=(\d+)"\s*$')
        if (-not $match.Success) { throw 'The prepared client launcher has no recognized server port. Restart setup.' }
        $GamePort = [int]$match.Groups[1].Value
    }
    if ($GamePort -lt 1 -or $GamePort -gt 65535) { throw 'Invalid server game port.' }
    $logs = Join-Path $ClientPath 'logs'
    if (-not [IO.Directory]::Exists($logs)) { [void][IO.Directory]::CreateDirectory($logs) }
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $exe
    $start.WorkingDirectory = $ClientPath
    $start.Arguments = '/nosplash /noconsole "script:/rmr_connection.py" "script:/rmr_login.py"'
    $start.UseShellExecute = $false
    $start.EnvironmentVariables['RMR_LOGIN_SCALE'] = '200'
    $start.EnvironmentVariables['RMR_SERVER_HOST'] = '127.0.0.1'
    $start.EnvironmentVariables['RMR_SERVER_PORT'] = [string]$GamePort
    $start.EnvironmentVariables['__COMPAT_LAYER'] = 'HIGHDPIAWARE'
    if ($env:RMR_ORIGINAL_TEMP) { $start.EnvironmentVariables['TEMP'] = $env:RMR_ORIGINAL_TEMP }
    if ($env:RMR_ORIGINAL_TMP) { $start.EnvironmentVariables['TMP'] = $env:RMR_ORIGINAL_TMP }
    if ($env:RMR_ORIGINAL_PATH) { $start.EnvironmentVariables['PATH'] = $env:RMR_ORIGINAL_PATH }
    [Diagnostics.Process]::Start($start) | Out-Null
}

$script:RmrClientStart = @'
@echo off
setlocal
rem Double-click for a 200%% login UI (1440 x 920 on a large enough display).
rem Optional: Start.bat [scale-percent] [server-address] [port]
rem Example for this machine's alternate server: Start.bat 200 127.0.0.1 26004
set "RMR_LOGIN_SCALE=200"
if not "%~1"=="" set "RMR_LOGIN_SCALE=%~1"
rem Connection overrides are inherited by this game process only.
if not defined RMR_SERVER_HOST set "RMR_SERVER_HOST=127.0.0.1"
if not defined RMR_SERVER_PORT set "RMR_SERVER_PORT=26000"
if not "%~2"=="" set "RMR_SERVER_HOST=%~2"
if not "%~3"=="" set "RMR_SERVER_PORT=%~3"

if not exist "%~dp0bin\exefile.exe" (
    echo Cannot find bin\exefile.exe beside Start.bat.
    pause
    exit /b 1
)
if not exist "%~dp0script\rmr_login.py" (
    echo Cannot find script\rmr_login.py beside Start.bat.
    pause
    exit /b 1
)
if not exist "%~dp0script\rmr_connection.py" (
    echo Cannot find script\rmr_connection.py beside Start.bat.
    pause
    exit /b 1
)

rem A clean extracted client has no logs directory until its first launch.
if not exist "%~dp0logs" mkdir "%~dp0logs"

rem Fonts are rendered at the larger size by the game, without bitmap DPI blur.
rem This compatibility setting applies only to this launch and its child process.
set "__COMPAT_LAYER=HIGHDPIAWARE"
rem The old eve.exe discards command-line arguments. Start the game directly.
start "" /D "%~dp0" "%~dp0bin\exefile.exe" /nosplash /noconsole "script:/rmr_connection.py" "script:/rmr_login.py"
exit /b %errorlevel%

'@

$script:RmrClientConnection = @'
# Launch-only connection settings and real LLV status for Python 2.3 / build 3913.
# Loaded explicitly by Start.bat. No connection defaults are written to disk.
import blue
import form
import sys
import uthread


def _rmrconn_log(message):
    try:
        handle = file(blue.os.rootpath + 'logs/rmr-connection.txt', 'a')
        try:
            handle.write(str(message) + '\n')
        finally:
            handle.close()
    except:
        sys.exc_clear()


def _rmrconn_overlay(obj, read_name, write_name, initial):
    # Patch this instance so aliases to it see the same session-only settings.
    # The real IniFile.lines / Section.__values remain untouched, including when
    # the game saves other preferences or the process exits without cleanup.
    if '_rmrconn_values' in obj.__dict__:
        return obj.__dict__['_rmrconn_values']
    values = initial.copy()
    protected = initial.copy()
    read_original = getattr(obj, read_name)
    write_original = getattr(obj, write_name)

    def read(key, *args, **kwargs):
        if key in protected:
            if key in values:
                return values[key]
            if args:
                default = args[0]
            elif 'default' in kwargs:
                default = kwargs['default']
            else:
                raise KeyError(key)
            if kwargs.get('flushDef') or (len(args) > 1 and args[1]):
                values[key] = default
            return default
        return read_original(key, *args, **kwargs)

    def write(key, value, *args, **kwargs):
        if key in protected:
            values[key] = value
            return
        return write_original(key, value, *args, **kwargs)

    obj.__dict__[read_name] = read
    obj.__dict__[write_name] = write
    obj.__dict__['_rmrconn_values'] = values
    if read_name == 'GetValue':
        has_original = obj.HasKey
        delete_original = obj.DeleteValue
        keys_original = obj.GetKeys

        def has(key):
            if key in protected:
                return key in values
            return has_original(key)

        def delete(key):
            if key in protected:
                if key in values:
                    del values[key]
                return
            return delete_original(key)

        def keys(beginWith=None):
            result = [key for key in keys_original() if key not in protected]
            result.extend(values.keys())
            if beginWith is not None:
                result = [key for key in result if key.startswith(beginWith)]
            return result

        obj.__dict__['HasKey'] = has
        obj.__dict__['DeleteValue'] = delete
        obj.__dict__['GetKeys'] = keys
    return values


def _rmrconn_endpoint(self):
    if not getattr(self, 'server', None) or not getattr(self, 'portedit', None):
        return None
    host = self.GetServerIP(self.server.GetValue()).strip()
    port = int(self.portedit.GetValue())
    if not host or port < 1 or port > 65535:
        raise ValueError('Invalid server address or port')
    return (host, port)


def _rmrconn_status_current(self, serial, endpoint):
    if getattr(self, '_rmrconn_serial', None) != serial:
        return False
    if not getattr(self, 'serverStatusText', None):
        return False
    try:
        return _rmrconn_endpoint(self) == endpoint
    except:
        sys.exc_clear()
        return False


def _rmrconn_status(self, server_name=None):
    if not getattr(self, 'serverStatusText', None):
        return
    serial = getattr(self, '_rmrconn_serial', 0) + 1
    self._rmrconn_serial = serial
    try:
        endpoint = _rmrconn_endpoint(self)
    except (ValueError, TypeError):
        self.serverStatusText.text = 'INVALID SERVER ADDRESS OR PORT'
        return
    if endpoint is None:
        return
    host, port = endpoint
    self.lastServerStatus = self.server.GetValue()
    self._rmrconn_last_endpoint = endpoint
    self.serverStatus[host] = None
    self.serverStatusText.text = 'CHECKING %s:%s...' % endpoint
    try:
        state, details = eve.LocalSvc('machoNet').GetServerStatus('%s:%s' % endpoint)
    except:
        sys.exc_clear()
        state, details = 'OFFLINE', None
    if not _rmrconn_status_current(self, serial, endpoint):
        return
    if state == 'OK' and details and len(details) >= 4:
        count, version, build, codename = details[:4]
        self.serverStatus[host] = (count, version, build, str(codename))
        # This server currently sends a fixed user count, so do not present it
        # as a live population. Connectivity/version come from the real probe.
        self.serverStatusText.text = 'ONLINE %s:%s<br>VERSION %.2f.%s' % (host, port, version, build)
    elif state == 'Incompatible':
        self.serverStatusText.text = 'INCOMPATIBLE SERVER %s:%s' % endpoint
    else:
        self.serverStatusText.text = 'OFFLINE / UNAVAILABLE<br>%s:%s' % endpoint
    _rmrconn_log('Status %s:%s: %s' % (host, port, state))


def _rmrconn_focus(self, *args):
    if not getattr(self, 'serverStatusText', None):
        return
    try:
        endpoint = _rmrconn_endpoint(self)
    except:
        sys.exc_clear()
        endpoint = None
    if endpoint != getattr(self, '_rmrconn_last_endpoint', None) or not self.serverStatusText.text:
        uthread.new(self.GetServerStatus)


def _rmrconn_poll(self):
    # One polling tasklet per login view, also refreshing after server restarts.
    while getattr(self, 'serverStatusText', None) is not None:
        blue.pyos.synchro.Sleep(10000)
        if getattr(self, 'serverStatusText', None) is None:
            return
        if not getattr(self, 'connecting', 0):
            self.GetServerStatus()


def _rmrconn_open(self, *args, **kwargs):
    # gameui creates settings.ui after the command-line helper is loaded.
    _rmrconn_overlay(settings.ui, 'Get', 'Set',
                     {'server': _rmrconn_host, 'usedServers': [_rmrconn_host]})
    result = _rmrconn_original_open(self, *args, **kwargs)
    self.portedit.OnFocusLost = self.OnServerFocusLost
    self.portedit.OnAnyChar = self.ClearServerStatus
    uthread.new(_rmrconn_poll, self)
    _rmrconn_log('Launch-only connection: %s:%s; UI %s:%s; advanced=%s' %
                 (_rmrconn_host, _rmrconn_port, self.server.GetValue(),
                  self.portedit.GetValue(), prefs.networkAdvanced))
    return result


def _rmrconn_install():
    global _rmrconn_host, _rmrconn_port, _rmrconn_original_open
    if getattr(form.Login, '_rmrconn_installed', False):
        return
    environment = blue.pyos.GetEnv()
    _rmrconn_host = environment['RMR_SERVER_HOST'].strip()
    _rmrconn_port = int(environment.get('RMR_SERVER_PORT', '26000'))
    if not _rmrconn_host or _rmrconn_port < 1 or _rmrconn_port > 65535:
        raise ValueError('Start.bat has an invalid RMR_SERVER_HOST / RMR_SERVER_PORT')
    _rmrconn_overlay(prefs.__dict__['ini'], 'GetValue', 'SetValue',
                     {'networkAdvanced': 1, 'port': _rmrconn_port})
    _rmrconn_original_open = form.Login.OnOpenView
    form.Login.OnOpenView = _rmrconn_open
    form.Login.GetServerStatus = _rmrconn_status
    form.Login.OnServerFocusLost = _rmrconn_focus
    form.Login._rmrconn_installed = True
    _rmrconn_log('Launch-only connection hook installed.')


if blue.pyos.GetEnv().get('RMR_SERVER_HOST'):
    _rmrconn_install()

'@

$script:RmrClientLogin = @'
# Launch-only login scaling and session-wide pacing for Python 2.3 / build 3913.
# Loaded explicitly by Start.bat; normal eve.exe startup remains available.
# No original resources, compiled code, preferences, or server settings are edited.
import blue
import form
import types
import sys
import traceback


def _rmr_log(message):
    try:
        handle = file(blue.os.rootpath + 'logs/rmr-login.txt', 'a')
        handle.write(str(message) + '\n')
        handle.close()
    except:
        sys.exc_clear()


def _rmr_windowed_settings(device, requested):
    # Get a driver-supported windowed presentation for the saved adapter and
    # size, then retain settings that are independent of fullscreen mode.
    adapter = requested.Adapter
    if adapter not in device.adapters:
        adapter = device.adapters[0]
    width = max(1, min(requested.BackBufferWidth, device.desktopMode.Width - 64))
    height = max(1, min(requested.BackBufferHeight, device.desktopMode.Height - 96))
    creation = device.GetFailsafeMode(adapter, 1, (width, height))
    if creation is None:
        return None
    safe = device.CreationToSettings(creation)
    values = requested.__dict__.copy()
    for key in ('Adapter', 'Windowed', 'BackBufferWidth', 'BackBufferHeight',
                'BackBufferFormat', 'AutoDepthStencilFormat',
                'FullScreen_RefreshRateInHz'):
        values[key] = safe.__dict__[key]
    safe.__dict__.update(values)
    return safe


class _RmrDeviceSettings:
    pass


def _rmr_paced_set(device, requested, set_device, *args, **kwargs):
    # SetDevice otherwise replaces the interval with IMMEDIATE when advanced
    # device settings are disabled. Override only its in-memory read, retaining
    # any existing connection overlay and restoring it even after an error.
    ini = prefs.__dict__['ini']
    old_get = ini.GetValue
    advanced = bool(old_get('advancedDevice', 0))
    had_get = 'GetValue' in ini.__dict__
    instance_get = ini.__dict__.get('GetValue')
    had_set = 'SetDevice' in device.__dict__
    instance_set = device.__dict__.get('SetDevice')
    original_fixup = device.FixupPresentation
    had_fixup = 'FixupPresentation' in device.__dict__
    instance_fixup = device.__dict__.get('FixupPresentation')
    attempts = [0]
    active_request = [None]
    if len(args) > 3:
        startup_hide_title = args[3]
    else:
        startup_hide_title = kwargs.get('hideTitle', 1)
    if len(args) > 2:
        requested_keep = args[2]
    else:
        requested_keep = kwargs.get('keepSettings', 1)
    section = None
    old_section_set = None
    had_section_set = False
    instance_section_set = None
    old_interval_present = False
    old_interval = None
    if requested_keep:
        try:
            section = settings.user.ui
            previous_settings = section.Get('DeviceSettings', None)
            if previous_settings is not None:
                old_interval_present = 'PresentationInterval' in previous_settings
                if old_interval_present:
                    old_interval = previous_settings['PresentationInterval']
            old_section_set = section.Set
            had_section_set = 'Set' in section.__dict__
            instance_section_set = section.__dict__.get('Set')
        except:
            # Never persist the forced interval when settings are unavailable.
            section = None
            requested_keep = 0
            sys.exc_clear()

    def save_without_pacing(key, value, *save_args, **save_kwargs):
        if key == 'DeviceSettings':
            value = value.copy()
            if old_interval_present:
                value['PresentationInterval'] = old_interval
            else:
                value.pop('PresentationInterval', None)
        return old_section_set(key, value, *save_args, **save_kwargs)

    def get_value(key, *get_args, **get_kwargs):
        if key == 'advancedDevice':
            return 1
        return old_get(key, *get_args, **get_kwargs)

    def invoke(settings, call_args, call_kwargs):
        attempts[0] += 1
        if attempts[0] > 4:
            _rmr_log('Windowed device retry limit reached.')
            return
        paced = _RmrDeviceSettings()
        paced.__dict__.update(settings.__dict__)
        paced.PresentationInterval = device.d3dconst.D3DPRESENT_INTERVAL_ONE
        if not advanced:
            paced.MultiSampleType = 0
            paced.MultiSampleQuality = 0
        names = ('tryAgain', 'fallback', 'keepSettings', 'hideTitle')
        defaults = (1, 0, 1, 1)
        options = list(call_args)
        call_kwargs = call_kwargs.copy()
        while len(options) < 4:
            index = len(options)
            options.append(call_kwargs.pop(names[index], defaults[index]))
        # Native retries must retain the caller's persistence choice.
        options[2] = requested_keep
        call_kwargs.pop('keepSettings', None)
        previous = active_request[0]
        active_request[0] = paced
        try:
            return set_device(paced, *options, **call_kwargs)
        finally:
            active_request[0] = previous

    def fixup(adapter, presentation):
        result = original_fixup(adapter, presentation)
        # FixupPresentation may substitute a new failsafe presentation whose
        # default interval is IMMEDIATE, or fall back to fullscreen.
        if active_request[0].Windowed and not result['Windowed']:
            raise RuntimeError('Windowed presentation unavailable')
        result['PresentationInterval'] = device.d3dconst.D3DPRESENT_INTERVAL_ONE
        return result

    def retry(settings, *retry_args, **retry_kwargs):
        # Native fallback can request fullscreen; keep this startup attempt
        # windowed and never let it persist the fallback settings.
        if not settings.Windowed:
            settings = _rmr_windowed_settings(device, settings)
            if settings is None:
                _rmr_log('Windowed device retry unavailable.')
                return
        retry_args = list(retry_args)
        if len(retry_args) > 3:
            retry_args[3] = startup_hide_title
        else:
            retry_kwargs['hideTitle'] = startup_hide_title
        return invoke(settings, retry_args, retry_kwargs)

    ini.__dict__['GetValue'] = get_value
    device.__dict__['SetDevice'] = retry
    device.__dict__['FixupPresentation'] = fixup
    if section is not None:
        section.__dict__['Set'] = save_without_pacing
    try:
        result = invoke(requested, args, kwargs)
    finally:
        if section is not None:
            if had_section_set:
                section.__dict__['Set'] = instance_section_set
            else:
                del section.__dict__['Set']
        if had_fixup:
            device.__dict__['FixupPresentation'] = instance_fixup
        else:
            del device.__dict__['FixupPresentation']
        if had_set:
            device.__dict__['SetDevice'] = instance_set
        else:
            del device.__dict__['SetDevice']
        if had_get:
            ini.__dict__['GetValue'] = instance_get
        else:
            del ini.__dict__['GetValue']
    try:
        _rmr_log('Actual presentation interval after device change: %s' %
                 device.GetSettings().PresentationInterval)
    except:
        sys.exc_clear()
    return result


def _rmr_install_pacing(device, state):
    original_set = device.SetDevice

    def paced_set(requested, *args, **kwargs):
        before = None
        if not state['preparing']:
            try:
                before = device.GetSettings().Windowed
            except:
                sys.exc_clear()
        result = _rmr_paced_set(device, requested, original_set, *args, **kwargs)
        if not state['preparing'] and before is not None and before != requested.Windowed:
            try:
                if device.GetSettings().Windowed == requested.Windowed:
                    state['manual_windowed'] = requested.Windowed
            except:
                sys.exc_clear()
        return result

    device.__dict__['SetDevice'] = paced_set


def _rmr_install_windowed(device, state):
    # PrepareMain is called by the intro or character selection after login.
    # It normally restores and saves the account's fullscreen DeviceSettings.
    original_prepare = device.PrepareMain
    def prepare_windowed(*args, **kwargs):
        original_set = device.SetDevice
        had_set = 'SetDevice' in device.__dict__
        old_set = device.__dict__.get('SetDevice')

        def set_windowed(requested, *set_args, **set_kwargs):
            try:
                if state['manual_windowed'] is None or state['manual_windowed']:
                    safe = _rmr_windowed_settings(device, requested)
                else:
                    safe = requested
            except:
                _rmr_log('Could not prepare windowed mode: ' +
                         ''.join(traceback.format_exception(*sys.exc_info())))
                sys.exc_clear()
                return
            if safe is None:
                _rmr_log('Windowed mode unavailable; retaining the login device.')
                return
            # SetDevice's positional defaults are tryAgain, fallback,
            # keepSettings, hideTitle. Replace those two even on retries.
            options = list(set_args)
            names = ('tryAgain', 'fallback', 'keepSettings', 'hideTitle')
            defaults = (1, 0, 1, 1)
            while len(options) < 4:
                index = len(options)
                options.append(set_kwargs.pop(names[index], defaults[index]))
            options[2] = 0
            options[3] = not safe.Windowed
            set_kwargs.pop('keepSettings', None)
            set_kwargs.pop('hideTitle', None)
            return original_set(safe, *options, **set_kwargs)

        device.__dict__['SetDevice'] = set_windowed
        state['preparing'] += 1
        try:
            return original_prepare(*args, **kwargs)
        finally:
            state['preparing'] -= 1
            if had_set:
                device.__dict__['SetDevice'] = old_set
            else:
                del device.__dict__['SetDevice']

    device.__dict__['PrepareMain'] = prepare_windowed


def _rmr_number(value):
    return type(value) in (types.IntType, types.LongType, types.FloatType)


def _rmr_snapshot(node, result):
    geometry = {}
    for key in ('left', 'top', 'width', 'height', 'setWidth', 'resetTop',
                'fixedWidth', 'fixedHeight', 'tabMargin', 'specialIndent'):
        value = getattr(node, key, None)
        if _rmr_number(value):
            geometry[key] = value
    font = None
    if hasattr(node, 'GetParams') and hasattr(node, 'Layout'):
        params = node.GetParams()
        font = (params.fontSize or 9, getattr(node, 'letterSpacing', 0) or 0,
                getattr(node, 'linespace', 0) or 0)
    result.append((node, geometry, font))
    for child in list(getattr(node, 'children', [])):
        _rmr_snapshot(child, result)


def _rmr_apply(records, scale):
    # Parent geometry first; glyphs are freshly rasterized by ULabel's font API.
    labels = []
    for node, geometry, font in records:
        if font is not None:
            node.busy = 1
        for key, value in geometry.items():
            setattr(node, key, int(round(value * scale)))
        if font is not None:
            node.fontSize = max(1, int(round(font[0] * scale)))
            node.letterSpacing = int(round(font[1] * scale))
            node.linespace = int(round(font[2] * scale))
            node.busy = 0
            labels.append(node)
    for node in labels:
        node.Layout()
    return len(labels)


_rmr_original_open = form.Login.OnOpenView
_rmr_original_motd = form.Login.ShowMotd


def _rmr_open(self, *args, **kwargs):
    _rmr_log('Opening scaled login.')
    self._rmr_scale = 1.0
    try:
        result = _rmr_original_open(self, *args, **kwargs)
        _rmr_log('Original login constructed.')
        return _rmr_resize(self, result)
    except:
        _rmr_log(''.join(traceback.format_exception(*sys.exc_info())))
        sys.exc_clear()
        return


def _rmr_resize(self, result):
    records = []
    try:
        requested = float(blue.pyos.GetEnv().get('RMR_LOGIN_SCALE', '200')) / 100.0
        requested = max(1.0, min(3.0, requested))
    except (ValueError, TypeError):
        requested = 2.0
    # LocalSvc returns a remote-call wrapper, including for attribute accesses.
    # Startup's service manager returns the actual local device service.
    device = sm.StartService('device')
    _rmr_log('Device ready.')
    old = device.GetSettings()
    if not old.Windowed:
        try:
            safe = _rmr_windowed_settings(device, old)
            if safe is not None:
                device.SetDevice(safe, keepSettings=0, hideTitle=0)
                old = device.GetSettings()
        except:
            _rmr_log('Login windowed mode failed: ' +
                     ''.join(traceback.format_exception(*sys.exc_info())))
            sys.exc_clear()
    _rmr_log('Current backbuffer: %sx%s' % (old.BackBufferWidth, old.BackBufferHeight))
    base_width = old.BackBufferWidth
    base_height = old.BackBufferHeight
    # Reserve room for the taskbar; retain the original login's aspect ratio.
    scale = min(requested,
                max(1.0, (device.desktopMode.Width - 64.0) / base_width),
                max(1.0, (device.desktopMode.Height - 96.0) / base_height))
    width = int(round(base_width * scale))
    height = int(round(base_height * scale))
    _rmr_log('Requested login: %sx%s, scale %.3f' % (width, height, scale))
    if scale <= 1.0:
        try:
            device.SetDevice(old, keepSettings=0, hideTitle=1)
        except:
            _rmr_log('Login pacing failed: ' +
                     ''.join(traceback.format_exception(*sys.exc_info())))
            sys.exc_clear()
        return result
    try:
        # Capture the original layout before the resize can move anchored items.
        for child in list(self.layer.uiwindow.children):
            _rmr_snapshot(child, records)
        creation = device.GetFailsafeMode(old.Adapter, 1, (width, height))
        if creation is None:
            _rmr_log('Windowed mode unavailable; retaining original login.')
            device.SetDevice(old, keepSettings=0, hideTitle=1)
            return result
        device.SetDevice(device.CreationToSettings(creation), keepSettings=0, hideTitle=1)
        actual = device.GetSettings()
        if (actual.BackBufferWidth, actual.BackBufferHeight) != (width, height):
            _rmr_log('Device rejected resize; retaining original layout.')
            device.SetDevice(old, keepSettings=0, hideTitle=1)
            return result
        count = _rmr_apply(records, scale)
        self._rmr_scale = scale
        eve.triapp.uilib.desktop.Update()
        _rmr_log('Applied login: %sx%s; %s native text labels.' %
                 (actual.BackBufferWidth, actual.BackBufferHeight, count))
    except:
        self._rmr_scale = 1.0
        _rmr_log('Scaling failed: ' + ''.join(traceback.format_exception(*sys.exc_info())))
        # Keep a usable original login if a driver or UI object rejects scaling.
        sys.exc_clear()
        try:
            device.SetDevice(old, keepSettings=0, hideTitle=1)
            _rmr_apply(records, 1.0)
            eve.triapp.uilib.desktop.Update()
        except:
            _rmr_log('Original-layout recovery failed: ' + str(sys.exc_info()[1]))
            sys.exc_clear()
    return result


def _rmr_motd(self, *args, **kwargs):
    result = _rmr_original_motd(self, *args, **kwargs)
    scale = getattr(self, '_rmr_scale', 1.0)
    if scale > 1.0:
        records = []
        for child in list(self.step2[0].children):
            if child.name.startswith('servernote'):
                _rmr_snapshot(child, records)
        _rmr_apply(records, scale)
    return result


form.Login.OnOpenView = _rmr_open
form.Login.ShowMotd = _rmr_motd
_rmr_device = sm.StartService('device')
_rmr_device_state = {'preparing': 0, 'manual_windowed': None}
_rmr_install_pacing(_rmr_device, _rmr_device_state)
_rmr_install_windowed(_rmr_device, _rmr_device_state)
_rmr_log('Session-wide presentation pacing installed.')
_rmr_log('Start.bat login scaling hook loaded.')

'@

function Start-LauncherUi {
    # Copy these into the function scope so event-handler closures retain them.
    $Root = [string]$Root
    $Runtime = [string]$Runtime
    $Logs = [string]$Logs
    $Self = [string]$Self
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    [System.Windows.Forms.Application]::EnableVisualStyles()
    if (-not ('Rmr.LauncherSkyForm' -as [type])) {
        Add-Type -ReferencedAssemblies @('System.dll', 'System.Windows.Forms.dll', 'System.Drawing.dll') -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Windows.Forms;

namespace Rmr {
    internal static class LauncherSky {
        public static void Paint(Graphics graphics, Rectangle bounds, double time, int seed, int count) {
            if (bounds.Width < 1 || bounds.Height < 1) return;
            graphics.SmoothingMode = SmoothingMode.AntiAlias;
            using (var background = new LinearGradientBrush(bounds,
                Color.FromArgb(8, 15, 27), Color.FromArgb(12, 23, 37), 28f))
                graphics.FillRectangle(background, bounds);
            var random = new Random(seed);
            for (int i = 0; i < count; i++) {
                float x = (float)random.NextDouble() * bounds.Width;
                float y = (float)random.NextDouble() * bounds.Height;
                double phase = random.NextDouble() * Math.PI * 2;
                double speed = 0.35 + random.NextDouble() * 0.45;
                float radius = i % 11 == 0 ? 1.5f : 0.7f;
                int alpha = (int)(35 + 105 * (0.5 + 0.5 * Math.Sin(time * speed + phase)));
                using (var star = new SolidBrush(Color.FromArgb(alpha, 165, 216, 230)))
                    graphics.FillEllipse(star, x, y, radius * 2, radius * 2);
                if (i % 19 == 0) {
                    using (var halo = new SolidBrush(Color.FromArgb(alpha / 7, 100, 220, 225)))
                        graphics.FillEllipse(halo, x - 3, y - 3, 8, 8);
                }
            }
        }
    }

    public sealed class LauncherSkyForm : Form {
        private readonly Timer animation;
        private readonly DateTime started = DateTime.UtcNow;
        public LauncherSkyForm() {
            SetStyle(ControlStyles.AllPaintingInWmPaint | ControlStyles.OptimizedDoubleBuffer |
                     ControlStyles.ResizeRedraw, true);
            animation = new Timer();
            animation.Interval = 100;
            animation.Tick += delegate { if (Visible && WindowState != FormWindowState.Minimized) Invalidate(); };
            animation.Start();
        }
        protected override void OnPaintBackground(PaintEventArgs e) {
            LauncherSky.Paint(e.Graphics, ClientRectangle, (DateTime.UtcNow - started).TotalSeconds, 45017, 140);
        }
        protected override void Dispose(bool disposing) {
            if (disposing) { animation.Stop(); animation.Dispose(); }
            base.Dispose(disposing);
        }
    }

    public sealed class LauncherSkyHeader : Panel {
        private readonly Timer animation;
        private readonly DateTime started = DateTime.UtcNow;
        public LauncherSkyHeader() {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint |
                     ControlStyles.OptimizedDoubleBuffer | ControlStyles.ResizeRedraw, true);
            animation = new Timer();
            animation.Interval = 100;
            animation.Tick += delegate { if (Visible) Invalidate(true); };
            animation.Start();
        }
        protected override void OnPaintBackground(PaintEventArgs e) {
            LauncherSky.Paint(e.Graphics, ClientRectangle, (DateTime.UtcNow - started).TotalSeconds, 182, 48);
        }
        protected override void Dispose(bool disposing) {
            if (disposing) { animation.Stop(); animation.Dispose(); }
            base.Dispose(disposing);
        }
    }

    public sealed class LauncherProgress : Control {
        private float value;
        public float Value {
            get { return value; }
            set { this.value = Math.Max(0, Math.Min(100, value)); Invalidate(); }
        }
        public LauncherProgress() {
            SetStyle(ControlStyles.UserPaint | ControlStyles.AllPaintingInWmPaint |
                     ControlStyles.OptimizedDoubleBuffer | ControlStyles.ResizeRedraw, true);
            BackColor = Color.FromArgb(18, 29, 43);
            Height = 10;
        }
        protected override void OnPaint(PaintEventArgs e) {
            if (Width < 1 || Height < 1) return;
            e.Graphics.SmoothingMode = SmoothingMode.AntiAlias;
            using (var track = new SolidBrush(Color.FromArgb(35, 51, 67)))
                e.Graphics.FillRectangle(track, ClientRectangle);
            float fill = Width * value / 100f;
            if (fill > 0) {
                using (var accent = new LinearGradientBrush(ClientRectangle,
                    Color.FromArgb(40, 151, 170), Color.FromArgb(112, 237, 213), 0f))
                    e.Graphics.FillRectangle(accent, 0, 0, fill, Height);
                using (var edge = new Pen(Color.FromArgb(120, 209, 255, 246)))
                    e.Graphics.DrawLine(edge, 0, 0, fill, 0);
            }
        }
    }
}
'@
    }

    $firstRun = -not [IO.File]::Exists((Join-Path $Runtime 'setup-complete'))
    $background = [System.Drawing.Color]::FromArgb(8, 15, 27)
    $panelColor = [System.Drawing.Color]::FromArgb(18, 29, 43)
    $muted = [System.Drawing.Color]::FromArgb(161, 180, 198)
    $bright = [System.Drawing.Color]::FromArgb(236, 244, 250)
    $cyan = [System.Drawing.Color]::FromArgb(112, 237, 213)
    $font = New-Object System.Drawing.Font('Segoe UI', 12.5)
    $form = New-Object Rmr.LauncherSkyForm
    $form.Text = 'Red Moon Rising | Preparing your universe'
    $form.StartPosition = 'CenterScreen'
    $workArea = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $form.ClientSize = New-Object System.Drawing.Size(([Math]::Min(1240, $workArea.Width - 64)), ([Math]::Min(900, $workArea.Height - 88)))
    $form.MinimumSize = New-Object System.Drawing.Size(([Math]::Min(1040, $workArea.Width - 64)), ([Math]::Min(770, $workArea.Height - 64)))
    $form.AutoScaleMode = 'Dpi'
    $form.BackColor = $background
    $form.ForeColor = $bright
    $form.Font = $font
    $form.Padding = New-Object System.Windows.Forms.Padding(28)

    # Small displays can scroll the complete layout instead of clipping controls.
    $viewport = New-Object System.Windows.Forms.Panel
    $viewport.Dock = 'Fill'
    $viewport.AutoScroll = $true
    $viewport.AutoScrollMinSize = New-Object System.Drawing.Size(930, 730)
    $viewport.BackColor = [System.Drawing.Color]::Transparent
    $form.Controls.Add($viewport)
    $layout = New-Object System.Windows.Forms.TableLayoutPanel
    $layout.Dock = 'Fill'
    $layout.MinimumSize = New-Object System.Drawing.Size(930, 730)
    $layout.ColumnCount = 1
    $layout.RowCount = 6
    $layout.BackColor = [System.Drawing.Color]::Transparent
    $layout.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    foreach ($height in @(112, 250, 42)) {
        $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', $height))) | Out-Null
    }
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Percent', 100))) | Out-Null
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', 78))) | Out-Null
    $layout.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', 36))) | Out-Null
    $viewport.Controls.Add($layout)

    $header = New-Object Rmr.LauncherSkyHeader
    $header.Dock = 'Fill'
    $header.Margin = New-Object System.Windows.Forms.Padding(0, 0, 0, 12)
    $brand = New-Object System.Windows.Forms.Label
    $brand.Text = 'RED MOON RISING'
    $brand.Font = New-Object System.Drawing.Font('Segoe UI Light', 34)
    $brand.ForeColor = $bright
    $brand.BackColor = [System.Drawing.Color]::Transparent
    $brand.AutoSize = $true
    $brand.Location = New-Object System.Drawing.Point(0, 1)
    $subtitle = New-Object System.Windows.Forms.Label
    $subtitle.Text = 'YOUR OWN CORNER OF NEW EDEN'
    $subtitle.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $subtitle.ForeColor = $muted
    $subtitle.BackColor = [System.Drawing.Color]::Transparent
    $subtitle.AutoSize = $true
    $subtitle.Location = New-Object System.Drawing.Point(4, 68)
    $editionLabel = New-Object System.Windows.Forms.Label
    $editionLabel.Text = 'EVE 2.10  /  3913'
    $editionLabel.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $editionLabel.ForeColor = $cyan
    $editionLabel.BackColor = [System.Drawing.Color]::Transparent
    $editionLabel.TextAlign = 'MiddleRight'
    $editionLabel.Dock = 'Right'
    $editionLabel.Width = 265
    $header.Controls.AddRange(@($brand, $subtitle, $editionLabel))
    $layout.Controls.Add($header, 0, 0)

    $statusPanel = New-Object System.Windows.Forms.TableLayoutPanel
    $statusPanel.Dock = 'Fill'
    $statusPanel.BackColor = $panelColor
    $statusPanel.Padding = New-Object System.Windows.Forms.Padding(24, 18, 24, 16)
    $statusPanel.Margin = New-Object System.Windows.Forms.Padding(0)
    $statusPanel.RowCount = 6
    $statusPanel.ColumnCount = 1
    $statusPanel.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    foreach ($height in @(24, 45, 57, 33, 10)) {
        $statusPanel.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Absolute', $height))) | Out-Null
    }
    $statusPanel.RowStyles.Add((New-Object System.Windows.Forms.RowStyle('Percent', 100))) | Out-Null
    $statusEyebrow = New-Object System.Windows.Forms.Label
    $statusEyebrow.Text = if ($firstRun) { 'FIRST LAUNCH' } else { 'WELCOME BACK' }
    $statusEyebrow.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $statusEyebrow.ForeColor = $cyan
    $statusEyebrow.Dock = 'Fill'
    $statusEyebrow.Margin = New-Object System.Windows.Forms.Padding(0)
    $stageLabel = New-Object System.Windows.Forms.Label
    $stageLabel.Text = 'Preparing your universe'
    $stageLabel.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 23)
    $stageLabel.Dock = 'Fill'
    $stageLabel.AutoEllipsis = $true
    $stageLabel.Margin = New-Object System.Windows.Forms.Padding(0)
    $messageLabel = New-Object System.Windows.Forms.Label
    $messageLabel.Text = if ($firstRun) { 'We will download the tools, build your server and prepare its database automatically.' } else { 'Checking your saved setup and getting everything ready for launch.' }
    $messageLabel.ForeColor = $muted
    $messageLabel.Dock = 'Fill'
    $messageLabel.AutoEllipsis = $true
    $messageLabel.Margin = New-Object System.Windows.Forms.Padding(0, 5, 0, 0)
    $progressHeading = New-Object System.Windows.Forms.TableLayoutPanel
    $progressHeading.Dock = 'Fill'
    $progressHeading.ColumnCount = 2
    $progressHeading.RowCount = 1
    $progressHeading.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', 100))) | Out-Null
    $progressHeading.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Absolute', 180))) | Out-Null
    $progressHeading.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressCaption = New-Object System.Windows.Forms.Label
    $progressCaption.Text = 'OVERALL SETUP'
    $progressCaption.Dock = 'Fill'
    $progressCaption.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $progressCaption.ForeColor = $muted
    $progressCaption.TextAlign = 'MiddleLeft'
    $progressCaption.AutoEllipsis = $true
    $progressCaption.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressPercent = New-Object System.Windows.Forms.Label
    $progressPercent.Text = '0%'
    $progressPercent.Dock = 'Fill'
    $progressPercent.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 15)
    $progressPercent.ForeColor = $cyan
    $progressPercent.TextAlign = 'MiddleRight'
    $progressPercent.Margin = New-Object System.Windows.Forms.Padding(0)
    $progressHeading.Controls.Add($progressCaption, 0, 0)
    $progressHeading.Controls.Add($progressPercent, 1, 0)
    $progressTrack = New-Object Rmr.LauncherProgress
    $progressTrack.Dock = 'Fill'
    $progressTrack.Margin = New-Object System.Windows.Forms.Padding(0)
    $setupHint = New-Object System.Windows.Forms.Label
    $setupHint.Text = if ($firstRun) { 'First setup can take a few minutes while tools download and the database is prepared.' } else { 'Your saved universe and installed tools will be reused.' }
    $setupHint.Font = New-Object System.Drawing.Font('Segoe UI', 11.5)
    $setupHint.ForeColor = $muted
    $setupHint.Dock = 'Fill'
    $setupHint.Margin = New-Object System.Windows.Forms.Padding(0, 12, 0, 0)
    $setupHint.AutoEllipsis = $true
    $statusPanel.Controls.Add($statusEyebrow, 0, 0)
    $statusPanel.Controls.Add($stageLabel, 0, 1)
    $statusPanel.Controls.Add($messageLabel, 0, 2)
    $statusPanel.Controls.Add($progressHeading, 0, 3)
    $statusPanel.Controls.Add($progressTrack, 0, 4)
    $statusPanel.Controls.Add($setupHint, 0, 5)
    $layout.Controls.Add($statusPanel, 0, 1)

    $logHeading = New-Object System.Windows.Forms.Label
    $logHeading.Dock = 'Fill'
    $logHeading.Text = 'ACTIVITY  /  LIVE SERVER OUTPUT'
    $logHeading.ForeColor = $muted
    $logHeading.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10.5)
    $logHeading.BackColor = [System.Drawing.Color]::Transparent
    $logHeading.TextAlign = 'MiddleLeft'
    $logHeading.Margin = New-Object System.Windows.Forms.Padding(0)
    $layout.Controls.Add($logHeading, 0, 2)

    $consoleFrame = New-Object System.Windows.Forms.Panel
    $consoleFrame.Dock = 'Fill'
    $consoleFrame.BackColor = [System.Drawing.Color]::FromArgb(8, 15, 24)
    $consoleFrame.Padding = New-Object System.Windows.Forms.Padding(16, 12, 16, 12)
    $consoleFrame.Margin = New-Object System.Windows.Forms.Padding(0)
    $consoleFrame.MinimumSize = New-Object System.Drawing.Size(0, 155)
    $console = New-Object System.Windows.Forms.RichTextBox
    $console.Dock = 'Fill'
    $console.BackColor = $consoleFrame.BackColor
    $console.ForeColor = [System.Drawing.Color]::FromArgb(188, 209, 222)
    $console.Font = New-Object System.Drawing.Font('Consolas', 11.5)
    $console.BorderStyle = 'None'
    $console.ReadOnly = $true
    $console.DetectUrls = $false
    $console.WordWrap = $true
    $console.HideSelection = $false
    $console.Margin = New-Object System.Windows.Forms.Padding(0)
    $console.Text = "Welcome to your universe.`r`nPreparing Red Moon Rising...`r`n"
    $consoleFrame.Controls.Add($console)
    $layout.Controls.Add($consoleFrame, 0, 3)

    $footer = New-Object System.Windows.Forms.TableLayoutPanel
    $footer.Dock = 'Fill'
    $footer.Margin = New-Object System.Windows.Forms.Padding(0, 20, 0, 0)
    $footer.BackColor = [System.Drawing.Color]::Transparent
    $footer.ColumnCount = 4
    $footer.RowCount = 1
    foreach ($width in @(30, 25, 20, 25)) {
        $footer.ColumnStyles.Add((New-Object System.Windows.Forms.ColumnStyle('Percent', $width))) | Out-Null
    }
    $connectionLabel = New-Object System.Windows.Forms.Label
    $connectionLabel.Dock = 'Fill'
    $connectionLabel.ForeColor = $muted
    $connectionLabel.TextAlign = 'MiddleLeft'
    $connectionLabel.BackColor = [System.Drawing.Color]::Transparent
    $connectionLabel.Font = New-Object System.Drawing.Font('Segoe UI', 11)
    $connectionLabel.Text = 'LOCAL UNIVERSE  /  Setup and game data stay on your computer'
    $connectionLabel.AutoEllipsis = $true
    $connectionLabel.Margin = New-Object System.Windows.Forms.Padding(0)

    $clientButton = New-Object System.Windows.Forms.Button
    $dashboardButton = New-Object System.Windows.Forms.Button
    $logsButton = New-Object System.Windows.Forms.Button
    $stopButton = New-Object System.Windows.Forms.Button
    foreach ($button in @($clientButton, $dashboardButton, $logsButton, $stopButton)) {
        $button.Dock = 'Fill'
        $button.FlatStyle = 'Flat'
        $button.FlatAppearance.BorderSize = 1
        $button.FlatAppearance.BorderColor = [System.Drawing.Color]::FromArgb(43, 65, 81)
        $button.FlatAppearance.MouseOverBackColor = [System.Drawing.Color]::FromArgb(31, 49, 65)
        $button.FlatAppearance.MouseDownBackColor = [System.Drawing.Color]::FromArgb(39, 65, 81)
        $button.BackColor = $panelColor
        $button.ForeColor = $bright
        $button.Cursor = [System.Windows.Forms.Cursors]::Hand
        $button.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 12.5)
        $button.Margin = New-Object System.Windows.Forms.Padding(0, 0, 12, 0)
        $button.UseVisualStyleBackColor = $false
    }
    $clientButton.Text = 'Preparing...'
    $clientButton.Enabled = $false
    $clientButton.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
    $clientButton.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
    $clientButton.FlatAppearance.BorderSize = 1
    $clientButton.FlatAppearance.MouseOverBackColor = [System.Drawing.Color]::FromArgb(150, 248, 229)
    $clientButton.FlatAppearance.MouseDownBackColor = [System.Drawing.Color]::FromArgb(79, 202, 182)
    $dashboardButton.Text = 'Preparing...'
    $dashboardButton.Enabled = $false
    $dashboardButton.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
    $dashboardButton.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
    $logsButton.Text = 'Open logs'
    $stopButton.Text = 'Stop server'
    $stopButton.ForeColor = [System.Drawing.Color]::FromArgb(255, 174, 156)
    $stopButton.Margin = New-Object System.Windows.Forms.Padding(0)
    $footer.Controls.Add($clientButton, 0, 0)
    $footer.Controls.Add($dashboardButton, 1, 0)
    $footer.Controls.Add($logsButton, 2, 0)
    $footer.Controls.Add($stopButton, 3, 0)
    $layout.Controls.Add($footer, 0, 4)
    $layout.Controls.Add($connectionLabel, 0, 5)
    $tooltip = New-Object System.Windows.Forms.ToolTip
    $launchClient = $null
    if (Get-Command Start-RmrGameClient -ErrorAction SilentlyContinue) {
        $launchClient = ${function:Start-RmrGameClient}
    }

    $state = @{
        Process = $null
        Ready = $false
        StopRequested = $false
        CloseRequested = $false
        Exited = $false
        LastLogLength = -1L
        LastLogWrite = [DateTime]::MinValue
        LastStatus = ''
        WebPort = 18080
        ClientPath = ''
        PickerOpen = $false
        LastClientRequest = ''
        FirstRun = $firstRun
        Started = [DateTime]::UtcNow
        RunId = $env:RMR_RUN_ID
        StopPath = (Join-Path $Runtime 'stop.request')
        StatusPath = (Join-Path $Runtime 'status.json')
        LogPath = (Join-Path $Logs 'launcher.log')
    }

    $setReadyState = {
        param([bool]$Ready, [string]$DisabledText = 'Preparing...')
        $state.Ready = $Ready -and -not $state.StopRequested -and -not $state.Exited
        $clientButton.Enabled = $state.Ready -and [bool]$state.ClientPath
        $dashboardButton.Enabled = $state.Ready
        if ($state.Ready) {
            $clientButton.Text = 'Launch EVE'
            $clientButton.BackColor = $cyan
            $clientButton.ForeColor = $background
            $clientButton.FlatAppearance.BorderSize = 0
            $clientButton.Cursor = [System.Windows.Forms.Cursors]::Hand
            $dashboardButton.Text = 'Dashboard'
            $dashboardButton.BackColor = $panelColor
            $dashboardButton.ForeColor = $bright
            $dashboardButton.Cursor = [System.Windows.Forms.Cursors]::Hand
        } else {
            foreach ($button in @($clientButton, $dashboardButton)) {
                $button.Text = $DisabledText
                $button.BackColor = [System.Drawing.Color]::FromArgb(36, 43, 52)
                $button.ForeColor = [System.Drawing.Color]::FromArgb(137, 148, 161)
                $button.FlatAppearance.BorderSize = 1
                $button.Cursor = [System.Windows.Forms.Cursors]::Default
            }
        }
    }.GetNewClosure()

    $requestStop = {
        if ($state.StopRequested -or $state.Exited) { return }
        & $setReadyState $false 'Stopping...'
        try {
            [System.IO.File]::WriteAllText($state.StopPath, $state.RunId, [System.Text.Encoding]::ASCII)
            $state.StopRequested = $true
            $stageLabel.Text = 'Stopping Red Moon Rising'
            $form.Text = 'Red Moon Rising | Stopping'
            $statusEyebrow.Text = 'SAFE SHUTDOWN'
            $messageLabel.Text = 'Finishing the current operation and shutting down the database. Please keep this window open.'
            $setupHint.Text = 'Your universe will be kept for the next launch.'
            $stopButton.Enabled = $false
            $stopButton.Text = 'Stopping...'
            $dashboardButton.Enabled = $false
            $clientButton.Enabled = $false
        } catch {
            $messageLabel.Text = 'Could not request shutdown: ' + $_.Exception.Message
        }
    }.GetNewClosure()

    $clientButton.Add_Click({
        if (-not $state.Ready -or $state.StopRequested -or $state.Exited) { return }
        if ($null -eq $state.Process -or $state.Process.HasExited) { & $setReadyState $false 'Server offline'; return }
        try {
            if ($null -eq $launchClient) { throw 'The client launcher is unavailable. Restart start.bat.' }
            & $launchClient -ClientPath $state.ClientPath | Out-Null
        } catch { $messageLabel.Text = 'Could not launch EVE: ' + $_.Exception.Message }
    }.GetNewClosure())
    $dashboardButton.Add_Click({
        if (-not $state.Ready -or $state.StopRequested -or $state.Exited) { return }
        if ($null -eq $state.Process -or $state.Process.HasExited) { & $setReadyState $false 'Server offline'; return }
        try { Start-Process ('http://127.0.0.1:{0}/' -f $state.WebPort) | Out-Null }
        catch { $messageLabel.Text = 'Could not open your browser: ' + $_.Exception.Message }
    }.GetNewClosure())
    $logsButton.Add_Click({
        try { Start-Process explorer.exe -ArgumentList ('"{0}"' -f $Logs) | Out-Null }
        catch { $messageLabel.Text = 'Logs are saved in ' + $Logs }
    }.GetNewClosure())
    $stopButton.Add_Click({
        if ($state.Exited) { $form.Close() } else { & $requestStop }
    }.GetNewClosure())
    $form.Add_FormClosing({
        param($sender, $eventArgs)
        if (-not $state.Exited -and $null -ne $state.Process) {
            $eventArgs.Cancel = $true
            $state.CloseRequested = $true
            & $requestStop
        }
    }.GetNewClosure())

    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 250
    $timer.Add_Tick({
        # Open with sharing: worker output and atomic status updates must never block the UI.
        try {
            if ([System.IO.File]::Exists($state.StatusPath)) {
                $stream = New-Object System.IO.FileStream($state.StatusPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                try {
                    $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
                    try { $raw = $reader.ReadToEnd() } finally { $reader.Dispose() }
                } finally { $stream.Dispose() }
                if ($raw -and $raw -ne $state.LastStatus) {
                    $status = $raw | ConvertFrom-Json
                    if (-not $status.PSObject.Properties['runId'] -or [string]$status.runId -ne [string]$state.RunId) {
                        & $setReadyState $false
                        $raw = $null
                    }
                }
                if ($raw -and $raw -ne $state.LastStatus) {
                    $state.LastStatus = $raw
                    if (-not $state.StopRequested) {
                        if ($status.stage) { $stageLabel.Text = [string]$status.stage }
                        if ($status.message) { $messageLabel.Text = [string]$status.message }
                    }
                    $overall = [Math]::Max(0, [Math]::Min(100, [double]$status.progress))
                    $downloadActive = $status.PSObject.Properties['downloadPercent'] -and
                        $null -ne $status.downloadPercent -and [double]$status.downloadPercent -ge 0 -and
                        -not $status.ready -and -not $status.error -and -not $state.StopRequested
                    $phaseActive = $status.PSObject.Properties['phasePercent'] -and
                        $null -ne $status.phasePercent -and [double]$status.phasePercent -ge 0 -and
                        -not $status.ready -and -not $status.error -and -not $state.StopRequested
                    if ($downloadActive) {
                        $filePercent = [Math]::Max(0, [Math]::Min(100, [double]$status.downloadPercent))
                        $progressTrack.Value = [single]$filePercent
                        $progressCaption.Text = 'FILE DOWNLOAD   /   OVERALL SETUP ' + ('{0:0}%' -f $overall)
                        $progressPercent.Text = '{0:0}%' -f $filePercent
                    } elseif ($phaseActive) {
                        $phasePercent = [Math]::Max(0, [Math]::Min(100, [double]$status.phasePercent))
                        $phaseTitle = 'PREPARING'
                        if ($status.PSObject.Properties['phaseLabel'] -and $status.phaseLabel) { $phaseTitle = [string]$status.phaseLabel }
                        $progressTrack.Value = [single]$phasePercent
                        $progressCaption.Text = $phaseTitle + '   /   OVERALL SETUP ' + ('{0:0}%' -f $overall)
                        $progressPercent.Text = '{0:0}%' -f $phasePercent
                    } else {
                        $progressTrack.Value = [single]$overall
                        $progressCaption.Text = 'OVERALL SETUP'
                        $progressPercent.Text = '{0:0}%' -f $overall
                    }
                    if ($status.message) { $tooltip.SetToolTip($messageLabel, [string]$status.message) }
                    if ($status.webPort) { $state.WebPort = [int]$status.webPort }
                    if ($status.PSObject.Properties['clientPath'] -and $status.clientPath) {
                        $state.ClientPath = [string]$status.clientPath
                        $tooltip.SetToolTip($clientButton, $state.ClientPath)
                        $tooltip.SetToolTip($connectionLabel, $state.ClientPath)
                    }
                    $verifiedReady = [bool]$status.ready -and -not [bool]$status.error -and -not [bool]$status.stopped -and
                        -not $state.StopRequested -and -not $state.Exited -and [bool]$state.ClientPath -and
                        $null -ne $state.Process -and -not $state.Process.HasExited
                    $disabledText = if ($state.StopRequested) { 'Stopping...' } elseif ($status.error -or $status.stopped) { 'Server offline' } else { 'Preparing...' }
                    & $setReadyState $verifiedReady $disabledText
                    if ($state.Ready) {
                        $stageLabel.ForeColor = $cyan
                        $statusEyebrow.Text = 'SETUP COMPLETE'
                        $stageLabel.Text = 'READY TO LAUNCH'
                        $messageLabel.Text = 'All setup is complete. Your server is online. Click Launch EVE below to play.'
                        $tooltip.SetToolTip($messageLabel, $messageLabel.Text)
                        $progressCaption.Text = 'READY TO LAUNCH'
                        $progressPercent.Text = 'READY'
                        $progressTrack.Value = 100
                        $setupHint.Text = 'No more downloads or setup steps remain.'
                        $connectionLabel.Text = 'ONLINE  /  127.0.0.1:' + [string]$status.gamePort
                        if ($state.ClientPath) { $connectionLabel.Text += '    |    Client: ' + [IO.Path]::GetFileName($state.ClientPath.TrimEnd('\')) }
                        $form.Text = 'Red Moon Rising | READY TO LAUNCH'
                    }
                    if ($status.error) {
                        & $setReadyState $false 'Server offline'
                        $form.Text = 'Red Moon Rising | Needs attention'
                        $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
                        $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
                        $messageLabel.Text = [string]$status.message
                        $setupHint.Text = 'The activity log below has the details. Your saved data is kept.'
                        $connectionLabel.Text = 'Review the log below for details'
                    }
                    if ($status.PSObject.Properties['needsClient'] -and $status.needsClient -and -not $state.PickerOpen -and -not $state.StopRequested) {
                        $requestId = 'initial'
                        if ($status.PSObject.Properties['clientRequestId']) { $requestId = [string]$status.clientRequestId }
                        if ($requestId -ne $state.LastClientRequest) {
                            $state.LastClientRequest = $requestId
                            $state.PickerOpen = $true
                            $picker = New-Object System.Windows.Forms.FolderBrowserDialog
                            try {
                                $picker.Description = 'Select the EVE 2.10 build 3913 client folder'
                                $picker.ShowNewFolderButton = $false
                                if ($env:RMR_INVOCATION_DIRECTORY -and (Test-Path -LiteralPath $env:RMR_INVOCATION_DIRECTORY -PathType Container)) { $picker.SelectedPath = $env:RMR_INVOCATION_DIRECTORY } else { $picker.SelectedPath = $Root }
                                $chosenPath = ''
                                if ($picker.ShowDialog($form) -eq [System.Windows.Forms.DialogResult]::OK) {
                                    $chosenPath = $picker.SelectedPath
                                }
                                $choicePath = Join-Path $Runtime 'client-selection.txt'
                                [System.IO.File]::WriteAllText($choicePath, $chosenPath, (New-Object System.Text.UTF8Encoding($false)))
                            } finally {
                                $picker.Dispose()
                                $state.PickerOpen = $false
                            }
                        }
                    }
                }
            }
        } catch { } # A partially written status is retried on the next tick.

        try {
            if ([System.IO.File]::Exists($state.LogPath)) {
                $fileInfo = New-Object System.IO.FileInfo($state.LogPath)
                if ($fileInfo.Length -ne $state.LastLogLength -or $fileInfo.LastWriteTimeUtc -ne $state.LastLogWrite) {
                    $stream = New-Object System.IO.FileStream($state.LogPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
                    try {
                        $offset = [Math]::Max(0L, $stream.Length - 65536L)
                        $stream.Position = $offset
                        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8)
                        try {
                            if ($offset -gt 0) { $null = $reader.ReadLine() }
                            $tail = $reader.ReadToEnd()
                        } finally { $reader.Dispose() }
                    } finally { $stream.Dispose() }
                    $console.Text = $tail
                    $console.SelectionStart = $console.TextLength
                    $console.ScrollToCaret()
                    $state.LastLogLength = $fileInfo.Length
                    $state.LastLogWrite = $fileInfo.LastWriteTimeUtc
                }
            }
        } catch { }

        if ($null -ne $state.Process -and -not $state.Exited) {
            $state.Process.Refresh()
            if ($state.Process.HasExited) {
                $state.Exited = $true
                & $setReadyState $false 'Server offline'
                $dashboardButton.Enabled = $false
                $clientButton.Enabled = $false
                $stopButton.Enabled = $true
                $stopButton.Text = 'Close'
                $connectionLabel.Text = 'OFFLINE  /  Your universe is saved'
                $form.Text = 'Red Moon Rising | Stopped'
                $statusEyebrow.Text = 'UNTIL NEXT TIME'
                $progressCaption.Text = 'SESSION FINISHED'
                $progressPercent.Text = 'OFFLINE'
                $setupHint.Text = 'Close this window when you are ready.'
                if ($state.Process.ExitCode -eq 0) {
                    $stageLabel.Text = 'Red Moon Rising stopped'
                    $messageLabel.Text = 'Your database is kept for next time. Run start.bat whenever you are ready.'
                } else {
                    $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
                    $stageLabel.Text = 'Red Moon Rising needs attention'
                    $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
                    if (-not $state.LastStatus) { $messageLabel.Text = 'Setup exited unexpectedly. Open the logs for details, then run start.bat again.' }
                }
                if ($state.CloseRequested) { $form.Close() }
            }
        }
    }.GetNewClosure())

    $form.Add_Shown({
        try {
            # Bring the launcher forward once; later client launches may take focus.
            $form.BringToFront()
            $form.Activate()
            $null = [System.IO.Directory]::CreateDirectory($Runtime)
            $null = [System.IO.Directory]::CreateDirectory($Logs)
            $lockPath = Join-Path $Runtime 'launcher.lock'
            try {
                $probe = [System.IO.File]::Open($lockPath, [System.IO.FileMode]::OpenOrCreate, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::None)
                $probe.Dispose()
            } catch {
                throw 'Another Red Moon Rising launcher is already running from this folder. Use its window to manage the server.'
            }

            # The worker owns stale-state cleanup after obtaining its exclusive lock.
            # Environment variables avoid injecting a checkout path into executable code.
            $bootstrap = '[AppContext]::SetSwitch("Switch.System.IO.UseLegacyPathHandling",$false); [AppContext]::SetSwitch("Switch.System.IO.BlockLongPaths",$false); $self = $env:RMR_LAUNCHER; $source = [IO.File]::ReadAllText($self); $parts = $source -split ''(?m)^#==RMR_POWERSHELL==\r?$'', 2; if ($parts.Count -ne 2) { throw "Launcher payload is missing" }; & ([ScriptBlock]::Create($parts[1]))'
            $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
            $oldMode = $env:RMR_MODE
            $oldLauncher = $env:RMR_LAUNCHER
            $oldUiPid = $env:RMR_UI_PID
            try {
                $env:RMR_MODE = 'worker'
                $env:RMR_LAUNCHER = $Self
                $env:RMR_UI_PID = [string]$PID
                $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
                $state.Process = Start-Process -FilePath $powershell -ArgumentList @('-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded) -WorkingDirectory $Root -WindowStyle Hidden -PassThru
            } finally {
                $env:RMR_MODE = $oldMode
                $env:RMR_LAUNCHER = $oldLauncher
                $env:RMR_UI_PID = $oldUiPid
            }
            $timer.Start()
        } catch {
            $state.Exited = $true
            & $setReadyState $false 'Server offline'
            $statusEyebrow.Text = 'A LITTLE ATTENTION NEEDED'
            $stageLabel.Text = 'Unable to start Red Moon Rising'
            $stageLabel.ForeColor = [System.Drawing.Color]::FromArgb(255, 151, 130)
            $messageLabel.Text = $_.Exception.Message
            $console.Text = $_.Exception.ToString()
            $stopButton.Text = 'Close'
            $connectionLabel.Text = 'OFFLINE'
        }
    }.GetNewClosure())

    try { [System.Windows.Forms.Application]::Run($form) }
    finally {
        $timer.Stop()
        $timer.Dispose()
        $tooltip.Dispose()
        if ($null -ne $state.Process) { $state.Process.Dispose() }
        $form.Dispose()
        $font.Dispose()
    }
}

if ($env:RMR_MODE -eq 'stop') {
    [IO.File]::WriteAllText($StopPath, 'stop', $Utf8)
    Write-Host 'Stop requested. The launcher will save and close its database.'
    exit 0
}
if ($env:RMR_MODE -eq 'ui') {
    try { Start-LauncherUi }
    catch {
        Add-Type -AssemblyName System.Windows.Forms
        [Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Red Moon Rising could not start', 'OK', 'Error') | Out-Null
    }
} else {
    $result = Start-RmrWorker
    exit $result
}
