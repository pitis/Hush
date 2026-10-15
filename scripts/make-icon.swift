// Draws the app icon (not Signal's): three signal lamps on a dark rounded square.
// Usage: swift scripts/make-icon.swift <out.png>   (1024×1024; scripts/make-icon.sh makes the set)
import AppKit

let size: CGFloat = 1024
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()
let ctx = NSGraphicsContext.current!.cgContext
// macOS icon grid: the body is inset ~10% with a ~22% corner radius
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let path = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)
ctx.addPath(path)
ctx.clip()
let top = NSColor(red: 0.16, green: 0.17, blue: 0.21, alpha: 1).cgColor
let bottom = NSColor(red: 0.07, green: 0.07, blue: 0.09, alpha: 1).cgColor
ctx.drawLinearGradient(
    CGGradient(colorsSpace: nil, colors: [top, bottom] as CFArray, locations: [0, 1])!,
    start: CGPoint(x: 0, y: 924), end: CGPoint(x: 0, y: 100), options: [])
// three lamps, top to bottom: red, amber, green (the middle one lit brightest)
let lamps: [(NSColor, CGFloat)] = [
    (NSColor(red: 0.93, green: 0.30, blue: 0.27, alpha: 1), 0.55),
    (NSColor(red: 0.98, green: 0.73, blue: 0.20, alpha: 1), 1.0),
    (NSColor(red: 0.27, green: 0.80, blue: 0.47, alpha: 1), 0.55),
]
let radius: CGFloat = 108
for (i, (color, glow)) in lamps.enumerated() {
    let center = CGPoint(x: 512, y: 512 + CGFloat(1 - i) * 250)
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 70 * glow, color: color.withAlphaComponent(glow).cgColor)
    ctx.setFillColor(color.withAlphaComponent(0.35 + 0.65 * glow).cgColor)
    ctx.fillEllipse(in: CGRect(x: center.x - radius, y: center.y - radius, width: radius * 2, height: radius * 2))
    ctx.restoreGState()
}
image.unlockFocus()
let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
