# Draws the WinMacDock app icon and writes assets\app.ico (PNG frames, 16-256 px).
param([string]$Out = (Join-Path $PSScriptRoot '..\assets\app.ico'))
Add-Type -AssemblyName System.Drawing

function New-Frame([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $s = $size / 256.0

    function RoundRect([float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
        $p = New-Object System.Drawing.Drawing2D.GraphicsPath
        $d = $r * 2
        $p.AddArc($x, $y, $d, $d, 180, 90); $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
        $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90); $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
        $p.CloseFigure(); return $p
    }

    # Tile with a blue-to-violet gradient.
    $tile = RoundRect (8 * $s) (8 * $s) (240 * $s) (240 * $s) (56 * $s)
    $rect = New-Object System.Drawing.RectangleF 0, 0, $size, $size
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, ([System.Drawing.Color]::FromArgb(255, 34, 120, 255)), ([System.Drawing.Color]::FromArgb(255, 150, 60, 230)), 60
    $g.FillPath($grad, $tile)

    # A frosted dock bar with three app tiles.
    $bar = RoundRect (36 * $s) (150 * $s) (184 * $s) (62 * $s) (22 * $s)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(110, 255, 255, 255))), $bar)
    $colors = @([System.Drawing.Color]::FromArgb(255, 255, 95, 87), [System.Drawing.Color]::FromArgb(255, 255, 189, 46), [System.Drawing.Color]::FromArgb(255, 40, 200, 64))
    for ($i = 0; $i -lt 3; $i++) {
        $x = (56 + $i * 52) * $s
        $icon = RoundRect $x (160 * $s) (42 * $s) (42 * $s) (11 * $s)
        $g.FillPath((New-Object System.Drawing.SolidBrush $colors[$i]), $icon)
    }
    # Menu bar line at the top.
    $top = RoundRect (36 * $s) (52 * $s) (184 * $s) (22 * $s) (11 * $s)
    $g.FillPath((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(150, 255, 255, 255))), $top)

    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    return ,$ms.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$frames = $sizes | ForEach-Object { ,(New-Frame $_) }
$fs = [System.IO.File]::Create($Out)
$w = New-Object System.IO.BinaryWriter $fs
$w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $sz = $sizes[$i]; $len = $frames[$i].Length
    $w.Write([byte]($(if ($sz -ge 256) { 0 } else { $sz }))); $w.Write([byte]($(if ($sz -ge 256) { 0 } else { $sz })))
    $w.Write([byte]0); $w.Write([byte]0); $w.Write([UInt16]1); $w.Write([UInt16]32)
    $w.Write([UInt32]$len); $w.Write([UInt32]$offset); $offset += $len
}
foreach ($f in $frames) { $w.Write($f) }
$w.Close()
"wrote $Out"
