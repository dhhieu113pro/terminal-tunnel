# Generates the app icon: assets\icon.ico (16..256 px, PNG-compressed entries) and
# assets\icon-180.png (apple-touch-icon / favicon for the web page).
# Run:  pwsh assets\make-icon.ps1
Add-Type -AssemblyName System.Drawing

$out = $PSScriptRoot

function New-IconBitmap([int]$S) {
    $bmp = New-Object System.Drawing.Bitmap $S, $S, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    # rounded square
    $inset = 0.03 * $S; $w = $S - 2 * $inset; $r = 0.24 * $S
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($inset, $inset, $r, $r, 180, 90)
    $path.AddArc($inset + $w - $r, $inset, $r, $r, 270, 90)
    $path.AddArc($inset + $w - $r, $inset + $w - $r, $r, $r, 0, 90)
    $path.AddArc($inset, $inset + $w - $r, $r, $r, 90, 90)
    $path.CloseFigure()
    $rect = New-Object System.Drawing.RectangleF 0, 0, $S, $S
    $fill = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect,
        ([System.Drawing.Color]::FromArgb(255, 30, 38, 52)), ([System.Drawing.Color]::FromArgb(255, 8, 11, 16)), 55.0
    $g.FillPath($fill, $path)
    $border = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(230, 88, 166, 255)), ([float](0.04 * $S))
    $g.DrawPath($border, $path)

    # ">" prompt
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 88, 166, 255)), ([float](0.105 * $S))
    $pen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
    $pen.LineJoin = [System.Drawing.Drawing2D.LineJoin]::Round
    $pts = [System.Drawing.PointF[]]@(
        (New-Object System.Drawing.PointF ([float](0.30 * $S)), ([float](0.31 * $S))),
        (New-Object System.Drawing.PointF ([float](0.50 * $S)), ([float](0.50 * $S))),
        (New-Object System.Drawing.PointF ([float](0.30 * $S)), ([float](0.69 * $S))))
    $g.DrawLines($pen, $pts)

    # cursor block (same yellow as the Enter key)
    $cur = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 242, 199, 68))
    $g.FillRectangle($cur, [float](0.57 * $S), [float](0.62 * $S), [float](0.22 * $S), [float](0.085 * $S))

    $g.Dispose()
    return $bmp
}

function Get-PngBytes([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return , $ms.ToArray()
}

# --- icon.ico: header + directory + PNG images
$sizes = 16, 24, 32, 48, 64, 128, 256
$images = foreach ($s in $sizes) { , (Get-PngBytes (New-IconBitmap $s)) }

$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $len = $images[$i].Length
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))   # width  (0 means 256)
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))   # height
    $bw.Write([byte]0); $bw.Write([byte]0)                     # palette, reserved
    $bw.Write([uint16]1); $bw.Write([uint16]32)                # planes, bits per pixel
    $bw.Write([uint32]$len); $bw.Write([uint32]$offset)
    $offset += $len
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $out 'icon.ico'), $ms.ToArray())

# --- icon-180.png for the web page
$b180 = New-IconBitmap 180
$b180.Save((Join-Path $out 'icon-180.png'), [System.Drawing.Imaging.ImageFormat]::Png)

"wrote icon.ico ($($ms.Length) bytes, sizes: $($sizes -join ', ')) and icon-180.png"
