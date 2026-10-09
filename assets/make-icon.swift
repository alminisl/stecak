// Draws the Stećak app icon (1024×1024 PNG): a gabled stećak tombstone with a carved
// rosette and a `>_` prompt. Usage: swift assets/make-icon.swift assets/icon-1024.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

// Background: macOS-style rounded square, night sky gradient.
let bg = CGPath(roundedRect: CGRect(x: 80, y: 80, width: 864, height: 864), cornerWidth: 190, cornerHeight: 190, transform: nil)
ctx.addPath(bg)
ctx.clip()
let sky = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [rgb(0x1b2033), rgb(0x0e1018)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(sky, start: CGPoint(x: 512, y: 944), end: CGPoint(x: 512, y: 80), options: [])

// Ground line.
ctx.setFillColor(rgb(0x2a3048))
ctx.fill(CGRect(x: 80, y: 80, width: 864, height: 150))

// The stećak: a gabled ("sarcophagus with roof") block in limestone.
let stone = CGMutablePath()
stone.move(to: CGPoint(x: 262, y: 200))
stone.addLine(to: CGPoint(x: 762, y: 200))
stone.addLine(to: CGPoint(x: 762, y: 600))
stone.addLine(to: CGPoint(x: 512, y: 790))
stone.addLine(to: CGPoint(x: 262, y: 600))
stone.closeSubpath()
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 40, color: rgb(0x000000, 0.55))
ctx.addPath(stone)
let lime = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [rgb(0xd9d2c1), rgb(0xa79f8c)] as CFArray, locations: [0, 1])!
ctx.clip()
ctx.drawLinearGradient(lime, start: CGPoint(x: 262, y: 790), end: CGPoint(x: 762, y: 200), options: [])
ctx.restoreGState()

// Roof ridge line, carved.
ctx.setStrokeColor(rgb(0x7d7564))
ctx.setLineWidth(10)
ctx.move(to: CGPoint(x: 262, y: 600))
ctx.addLine(to: CGPoint(x: 762, y: 600))
ctx.strokePath()

// Carved rosette (a common stećak motif) in the gable.
let center = CGPoint(x: 512, y: 672)
ctx.setStrokeColor(rgb(0x6d6553))
ctx.setLineWidth(8)
ctx.strokeEllipse(in: CGRect(x: center.x - 52, y: center.y - 52, width: 104, height: 104))
for i in 0..<6 {
    let a = CGFloat(i) * .pi / 3
    let p = CGPoint(x: center.x + cos(a) * 26, y: center.y + sin(a) * 26)
    ctx.strokeEllipse(in: CGRect(x: p.x - 26, y: p.y - 26, width: 52, height: 52))
}

// Carved prompt `>_` in the face of the stone, glowing amber like a terminal.
ctx.setLineCap(.round)
ctx.setLineJoin(.round)
ctx.setStrokeColor(rgb(0xf2a65a))
ctx.setShadow(offset: .zero, blur: 24, color: rgb(0xf2a65a, 0.7))
ctx.setLineWidth(46)
ctx.move(to: CGPoint(x: 360, y: 500))
ctx.addLine(to: CGPoint(x: 470, y: 400))
ctx.addLine(to: CGPoint(x: 360, y: 300))
ctx.strokePath()
ctx.move(to: CGPoint(x: 530, y: 290))
ctx.addLine(to: CGPoint(x: 670, y: 290))
ctx.strokePath()

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
