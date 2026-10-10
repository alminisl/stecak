# Put Windows Terminal's ConPTY (conpty.dll + OpenConsole.exe) next to stecak.exe.
# portable-pty loads a conpty.dll found beside the exe in preference to the one built into
# Windows; this newer console host makes output ~1.5x faster. Source: Microsoft's signed
# Microsoft.Windows.Console.ConPTY NuGet package.
#   scripts/fetch-conpty.ps1 -Arch x64 -Dest target/release
param(
    [ValidateSet('x64', 'arm64')] [string]$Arch = 'x64',
    [Parameter(Mandatory)] [string]$Dest,
    [string]$Version = '1.25.260930003'
)
$ErrorActionPreference = 'Stop'
$work = Join-Path ([IO.Path]::GetTempPath()) "conpty-$Version"
if (-not (Test-Path "$work/pkg")) {
    New-Item -ItemType Directory -Force $work | Out-Null
    $url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$Version/microsoft.windows.console.conpty.$Version.nupkg"
    Invoke-WebRequest $url -OutFile "$work/conpty.zip"
    Expand-Archive "$work/conpty.zip" "$work/pkg" -Force
}
$files = "$work/pkg/runtimes/win-$Arch/native/conpty.dll", "$work/pkg/build/native/runtimes/$Arch/OpenConsole.exe"
foreach ($f in $files) {
    $sig = Get-AuthenticodeSignature $f
    if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation') {
        throw "$f is not signed by Microsoft ($($sig.Status))"
    }
}
New-Item -ItemType Directory -Force $Dest | Out-Null
Copy-Item $files $Dest -Force
