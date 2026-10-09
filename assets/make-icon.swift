// Draws the Stećak app icon (1024×1024 PNG): a gabled stećak tombstone under a crescent
// moon, with a carved rosette and a glowing `>_` prompt carved into its face.
// Usage: swift assets/make-icon.swift assets/icon-1024.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext
let space = CGColorSpaceCreateDeviceRGB()

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: a)
}
func gradient(_ colors: [CGColor], _ locs: [CGFloat]) -> CGGradient {
    CGGradient(colorsSpace: space, colors: colors as CFArray, locations: locs)!
}
func path(_ pts: [CGPoint]) -> CGPath {
    let p = CGMutablePath()
    p.addLines(between: pts)
    p.closeSubpath()
    return p
}
// Deterministic pseudo-random numbers so the stone grain is identical on every build.
var seed: UInt64 = 0x5EC4_AC
func rand() -> CGFloat {
    seed = seed &* 6364136223846793005 &+ 1442695040888963407
    return CGFloat((seed >> 33) % 10_000) / 10_000
}

// --- Background: rounded square, night sky ----------------------------------------------
let bg = CGPath(roundedRect: CGRect(x: 80, y: 80, width: 864, height: 864), cornerWidth: 190, cornerHeight: 190, transform: nil)
ctx.saveGState()
ctx.addPath(bg)
ctx.clip()
ctx.drawLinearGradient(gradient([rgb(0x27305a), rgb(0x141a33), rgb(0x0b0e1a)], [0, 0.55, 1]),
                       start: CGPoint(x: 512, y: 944), end: CGPoint(x: 512, y: 80), options: [])
// Moonlight halo behind the stone.
ctx.drawRadialGradient(gradient([rgb(0x8fa2d8, 0.35), rgb(0x8fa2d8, 0)], [0, 1]),
                       startCenter: CGPoint(x: 512, y: 560), startRadius: 0,
                       endCenter: CGPoint(x: 512, y: 560), endRadius: 430, options: [])
// Stars.
for (x, y, r) in [(210.0, 830.0, 5.0), (300, 760, 3.5), (820, 700, 4.0), (740, 860, 3.0), (180, 640, 3.0), (860, 560, 2.5)] {
    ctx.setFillColor(rgb(0xe8ecff, 0.85))
    ctx.fillEllipse(in: CGRect(x: x - r, y: y - r, width: 2 * r, height: 2 * r))
}
// Crescent moon, a classic stećak relief motif: a disc with a second disc cut out of it.
ctx.saveGState()
let moon = CGRect(x: 690, y: 730, width: 130, height: 130)
ctx.setShadow(offset: .zero, blur: 40, color: rgb(0xf5e6c4, 0.6))
ctx.beginTransparencyLayer(auxiliaryInfo: nil)
ctx.setFillColor(rgb(0xf3e4c2))
ctx.fillEllipse(in: moon)
// Cut the second disc out of the first (inside the layer, so only the moon is erased).
ctx.setBlendMode(.clear)
ctx.fillEllipse(in: moon.offsetBy(dx: 40, dy: 24))
ctx.endTransparencyLayer()
ctx.restoreGState()
// Grassy mound.
let mound = CGMutablePath()
mound.move(to: CGPoint(x: 80, y: 80))
mound.addLine(to: CGPoint(x: 80, y: 250))
mound.addQuadCurve(to: CGPoint(x: 944, y: 240), control: CGPoint(x: 512, y: 330))
mound.addLine(to: CGPoint(x: 944, y: 80))
mound.closeSubpath()
ctx.addPath(mound)
ctx.clip()
ctx.drawLinearGradient(gradient([rgb(0x2b3a3a), rgb(0x141c1f)], [0, 1]), start: CGPoint(x: 512, y: 330), end: CGPoint(x: 512, y: 80), options: [])
ctx.restoreGState()

// --- The stećak: front face plus a shaded side face for depth --------------------------
let front = path([CGPoint(x: 252, y: 200), CGPoint(x: 712, y: 200), CGPoint(x: 712, y: 590), CGPoint(x: 482, y: 780), CGPoint(x: 252, y: 590)])
let side = path([CGPoint(x: 712, y: 200), CGPoint(x: 790, y: 236), CGPoint(x: 790, y: 612), CGPoint(x: 560, y: 800), CGPoint(x: 482, y: 780), CGPoint(x: 712, y: 590)])

// Ground shadow.
ctx.setFillColor(rgb(0x000000, 0.45))
ctx.fillEllipse(in: CGRect(x: 210, y: 168, width: 640, height: 70))

ctx.saveGState()
ctx.addPath(side)
ctx.clip()
ctx.drawLinearGradient(gradient([rgb(0x9c9480), rgb(0x6d665a)], [0, 1]), start: CGPoint(x: 712, y: 700), end: CGPoint(x: 790, y: 200), options: [])
ctx.restoreGState()

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 30, color: rgb(0x000000, 0.5))
ctx.addPath(front)
ctx.setFillColor(rgb(0xcfc7b3))
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(front)
ctx.clip()
ctx.drawLinearGradient(gradient([rgb(0xebe4d2), rgb(0xcbc2ab), rgb(0xa9a089)], [0, 0.55, 1]),
                       start: CGPoint(x: 300, y: 780), end: CGPoint(x: 712, y: 200), options: [])
// Limestone grain: faint pits and flecks.
for _ in 0..<900 {
    let x = 252 + rand() * 460, y = 200 + rand() * 580, r = 1 + rand() * 3.2
    ctx.setFillColor(rand() > 0.5 ? rgb(0x6f6656, 0.18) : rgb(0xffffff, 0.18))
    ctx.fillEllipse(in: CGRect(x: x, y: y, width: r, height: r))
}
ctx.restoreGState()

// Chiselled edges: light along the top-left, dark along the roof line.
ctx.setLineJoin(.round)
ctx.setStrokeColor(rgb(0xfaf5e8, 0.75))
ctx.setLineWidth(5)
ctx.move(to: CGPoint(x: 254, y: 590))
ctx.addLine(to: CGPoint(x: 482, y: 778))
ctx.strokePath()
ctx.setStrokeColor(rgb(0x6d665a, 0.9))
ctx.setLineWidth(9)
ctx.move(to: CGPoint(x: 252, y: 588))
ctx.addLine(to: CGPoint(x: 712, y: 588))
ctx.strokePath()

// "Carved" stroke: a dark groove with a light lip offset down-right, then the shape itself.
func carve(_ draw: () -> Void, width: CGFloat) {
    ctx.saveGState()
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.translateBy(x: 3, y: -3)
    ctx.setStrokeColor(rgb(0xfdf8ec, 0.7))
    ctx.setLineWidth(width)
    draw()
    ctx.strokePath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.setStrokeColor(rgb(0x5d5547))
    ctx.setLineWidth(width)
    draw()
    ctx.strokePath()
    ctx.restoreGState()
}

// Rosette in the gable: a ring of six petals.
let c = CGPoint(x: 482, y: 670)
carve({ ctx.addEllipse(in: CGRect(x: c.x - 56, y: c.y - 56, width: 112, height: 112)) }, width: 8)
for i in 0..<6 {
    let a = CGFloat(i) * .pi / 3
    let p = CGPoint(x: c.x + cos(a) * 27, y: c.y + sin(a) * 27)
    carve({ ctx.addEllipse(in: CGRect(x: p.x - 27, y: p.y - 27, width: 54, height: 54)) }, width: 6)
}

// The prompt `>_`: carved groove first, then glowing amber "embers" inside it.
let chevron = { ctx.move(to: CGPoint(x: 340, y: 500)); ctx.addLine(to: CGPoint(x: 452, y: 400)); ctx.addLine(to: CGPoint(x: 340, y: 300)) }
let underscore = { ctx.move(to: CGPoint(x: 512, y: 296)); ctx.addLine(to: CGPoint(x: 640, y: 296)) }
carve(chevron, width: 64)
carve(underscore, width: 64)
for (shape, glow) in [(chevron, true), (underscore, true)] where glow {
    ctx.saveGState()
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.setShadow(offset: .zero, blur: 36, color: rgb(0xffa040, 0.95))
    ctx.setStrokeColor(rgb(0xf7a447))
    ctx.setLineWidth(42)
    shape()
    ctx.strokePath()
    ctx.setShadow(offset: .zero, blur: 0, color: nil)
    ctx.setStrokeColor(rgb(0xffd79a))
    ctx.setLineWidth(14)
    shape()
    ctx.strokePath()
    ctx.restoreGState()
}

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
