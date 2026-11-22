# Windows' part of ci/screenshots.sh (theca/computer#96): the screen set to
# 1920x1080 as a monitor would be, and a picture of it.
#   screenshots.ps1 screen
#   screenshots.ps1 shot <file.png>
param([Parameter(Mandatory)][string]$Action, [string]$File)
$ErrorActionPreference = 'Stop'

switch ($Action) {
    'screen' {
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Display {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
        public int dmFields, dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
        public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels;
        public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
        public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
    }
    [DllImport("user32.dll")] public static extern bool EnumDisplaySettings(string d, int m, ref DEVMODE dm);
    [DllImport("user32.dll")] public static extern int ChangeDisplaySettings(ref DEVMODE dm, int flags);
    public static int Set(int w, int h) {
        var dm = new DEVMODE(); dm.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
        EnumDisplaySettings(null, -1, ref dm);
        dm.dmPelsWidth = w; dm.dmPelsHeight = h; dm.dmFields = 0x80000 | 0x100000;
        return ChangeDisplaySettings(ref dm, 0);
    }
}
'@
        $result = [Display]::Set(1920, 1080)
        # 0: DISP_CHANGE_SUCCESSFUL.
        if ($result -ne 0) { throw "ChangeDisplaySettings: $result" }
    }
    'shot' {
        Add-Type -AssemblyName System.Windows.Forms, System.Drawing
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
        $bitmap.Save($File, [System.Drawing.Imaging.ImageFormat]::Png)
        $graphics.Dispose()
        $bitmap.Dispose()
    }
    default { throw "unknown action: $Action" }
}
