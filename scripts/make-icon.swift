// Draws the Lanes app icon (1024×1024 PNG). Run: swift scripts/make-icon.swift assets/icon.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// macOS icon grid: 824pt body centred in 1024 with a soft shadow.
let body = NSRect(x: 100, y: 100, width: 824, height: 824)
let shape = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.28)
shadow.shadowBlurRadius = 28
shadow.shadowOffset = NSSize(width: 0, height: -12)
NSGraphicsContext.saveGraphicsState()
shadow.set()
NSColor(srgbRed: 0.18, green: 0.27, blue: 0.70, alpha: 1).setFill()
shape.fill()
NSGraphicsContext.restoreGraphicsState()

let gradient = NSGradient(colors: [
    NSColor(srgbRed: 0.40, green: 0.53, blue: 0.97, alpha: 1),
    NSColor(srgbRed: 0.17, green: 0.26, blue: 0.68, alpha: 1),
])!
gradient.draw(in: shape, angle: -90)

// Three lanes: the focused one tall and bright, neighbours peeking.
func lane(_ x: CGFloat, _ h: CGFloat, _ alpha: CGFloat) {
    let w: CGFloat = 168
    let r = NSRect(x: x, y: 512 - h / 2, width: w, height: h)
    NSColor.white.withAlphaComponent(alpha).setFill()
    NSBezierPath(roundedRect: r, xRadius: 40, yRadius: 40).fill()
}
lane(196, 380, 0.55)
lane(428, 540, 1.0)
lane(660, 380, 0.55)

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
