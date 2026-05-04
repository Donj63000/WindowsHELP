param(
    [switch]$BuildRelease,
    [int]$LaunchDelaySeconds = 8,
    [string]$OutputDir = "target\ui-smoke"
)

$ErrorActionPreference = "Stop"

if ($BuildRelease) {
    cargo build --release
}

$exe = Join-Path (Get-Location) "target\release\WindowsHELP.exe"
if (-not (Test-Path $exe)) {
    throw "Executable introuvable: $exe"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$signature = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public class UiSmokeNative {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
  public const uint WM_CLOSE = 0x0010;
  public const uint SWP_NOSIZE = 0x0001;
  public const uint SWP_NOMOVE = 0x0002;
  public const uint SWP_SHOWWINDOW = 0x0040;
  public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
'@
Add-Type -TypeDefinition $signature -ErrorAction SilentlyContinue

function Get-WindowRectForHandle {
    param([IntPtr]$Handle)

    $rect = New-Object UiSmokeNative+RECT
    [void][UiSmokeNative]::GetWindowRect($Handle, [ref]$rect)
    return $rect
}

function Find-WindowsHelpWindow {
    param([int]$ProcessId)

    $matches = New-Object System.Collections.Generic.List[object]
    $targetProcessId = $ProcessId
    $callback = [UiSmokeNative+EnumWindowsProc]{
        param([IntPtr]$hWnd, [IntPtr]$lParam)

        [uint32]$windowProcessId = 0
        [void][UiSmokeNative]::GetWindowThreadProcessId($hWnd, [ref]$windowProcessId)
        if ($windowProcessId -ne [uint32]$targetProcessId -or -not [UiSmokeNative]::IsWindowVisible($hWnd)) {
            return $true
        }

        $title = New-Object System.Text.StringBuilder 256
        [void][UiSmokeNative]::GetWindowText($hWnd, $title, $title.Capacity)
        $rect = Get-WindowRectForHandle $hWnd
        $width = $rect.Right - $rect.Left
        $height = $rect.Bottom - $rect.Top

        if ($title.ToString() -eq "WindowsHELP" -and $width -ge 300 -and $height -ge 300) {
            $matches.Add([PSCustomObject]@{
                Handle = $hWnd
                Width = $width
                Height = $height
                Area = $width * $height
            })
        }

        return $true
    }

    [void][UiSmokeNative]::EnumWindows($callback, [IntPtr]::Zero)
    if ($matches.Count -eq 0) {
        return [IntPtr]::Zero
    }

    return ($matches | Sort-Object Area -Descending | Select-Object -First 1).Handle
}

$proc = Start-Process -FilePath $exe -PassThru
$screenshotPath = Join-Path $OutputDir "windowshelp-ui-smoke.png"

try {
    $windowHandle = [IntPtr]::Zero
    $deadline = (Get-Date).AddSeconds($LaunchDelaySeconds)
    do {
        Start-Sleep -Milliseconds 250
        $proc = Get-Process -Id $proc.Id -ErrorAction Stop
        $windowHandle = Find-WindowsHelpWindow -ProcessId $proc.Id
    } while ($windowHandle -eq [IntPtr]::Zero -and (Get-Date) -lt $deadline)

    if ($windowHandle -eq [IntPtr]::Zero) {
        throw "La vraie fenetre WindowsHELP n'est pas disponible"
    }
    if (-not $proc.Responding) {
        throw "La fenetre ne repond pas"
    }

    [void][UiSmokeNative]::ShowWindow($windowHandle, 5)
    [void][UiSmokeNative]::SetForegroundWindow($windowHandle)
    $topMostFlags = [UiSmokeNative]::SWP_NOMOVE -bor [UiSmokeNative]::SWP_NOSIZE -bor [UiSmokeNative]::SWP_SHOWWINDOW
    $topMostApplied = [UiSmokeNative]::SetWindowPos($windowHandle, [IntPtr](-1), 0, 0, 0, 0, $topMostFlags)
    if (-not $topMostApplied) {
        throw "La fenetre WindowsHELP ne peut pas etre remontee au premier plan"
    }
    Start-Sleep -Milliseconds 800
    $foreground = [UiSmokeNative]::GetForegroundWindow()
    $foregroundVerified = ($foreground -eq $windowHandle)

    $rect = Get-WindowRectForHandle $windowHandle
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -lt 300 -or $height -lt 300) {
        throw "Dimensions de fenetre invalides: ${width}x${height}"
    }

    $screen = [System.Windows.Forms.Screen]::FromHandle($windowHandle)
    $work = $screen.WorkingArea
    $margin = 24
    if ($rect.Left -lt ($work.Left - $margin) -or
        $rect.Top -lt ($work.Top - $margin) -or
        $rect.Right -gt ($work.Right + $margin) -or
        $rect.Bottom -gt ($work.Bottom + $margin)) {
        throw "La fenetre deborde de la zone utile: rect=[$($rect.Left),$($rect.Top),$($rect.Right),$($rect.Bottom)] work=$work"
    }

    $bitmap = New-Object System.Drawing.Bitmap $width, $height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
    $bitmap.Save($screenshotPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    [void][UiSmokeNative]::SetWindowPos($windowHandle, [IntPtr](-2), 0, 0, 0, 0, $topMostFlags)

    $sampleCount = 0
    $distinct = New-Object 'System.Collections.Generic.HashSet[int]'
    $stepX = [Math]::Max(1, [int]($width / 24))
    $stepY = [Math]::Max(1, [int]($height / 24))
    for ($x = 0; $x -lt $width; $x += $stepX) {
        for ($y = 0; $y -lt $height; $y += $stepY) {
            [void]$distinct.Add($bitmap.GetPixel($x, $y).ToArgb())
            $sampleCount++
        }
    }
    if ($sampleCount -eq 0 -or $distinct.Count -lt 4) {
        throw "La capture semble vide ou uniforme: couleurs=$($distinct.Count)"
    }

    $topRightBrightPixels = 0
    $topRightXStart = [Math]::Max(0, $width - 360)
    $topRightXEnd = [Math]::Max($topRightXStart, $width - 18)
    $topRightYStart = [Math]::Min($height - 1, 16)
    $topRightYEnd = [Math]::Min($height - 1, 58)
    for ($x = $topRightXStart; $x -lt $topRightXEnd; $x += 2) {
        for ($y = $topRightYStart; $y -lt $topRightYEnd; $y += 2) {
            $pixel = $bitmap.GetPixel($x, $y)
            if ($pixel.R -ge 150 -and $pixel.G -ge 150 -and $pixel.B -ge 150) {
                $topRightBrightPixels++
            }
        }
    }
    if ($topRightBrightPixels -lt 24) {
        throw "La zone droite de la barre haute ne montre pas les controles attendus: pixels_clairs=$topRightBrightPixels"
    }

    $bitmap.Dispose()

    [void][UiSmokeNative]::PostMessage($windowHandle, [UiSmokeNative]::WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) {
            break
        }
    }

    if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) {
        throw "Le processus reste actif apres la fermeture"
    }

    [PSCustomObject]@{
        Status = "OK"
        Window = "${width}x${height}"
        WorkArea = $work.ToString()
        Screenshot = (Resolve-Path $screenshotPath).Path
        DistinctSampledColors = $distinct.Count
        TopRightBrightPixels = $topRightBrightPixels
        ForegroundVerified = $foregroundVerified
    } | Format-List
}
finally {
    $remaining = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
    if ($remaining) {
        Stop-Process -Id $proc.Id -Force
    }
}
