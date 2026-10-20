// The Sophia Labs logo as a macOS app icon: Apple's grid (an 824 px rounded square on a 1024 canvas, a soft shadow).
import AppKit
import CoreGraphics

let args = CommandLine.arguments
let src = NSImage(contentsOfFile: args[1])!
var rect = CGRect(origin: .zero, size: src.size)
let logo = src.cgImage(forProposedRect: &rect, context: nil, hints: nil)!
let size = 1024, body: CGFloat = 824, radius: CGFloat = 185.4
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx.interpolationQuality = .high
let frame = CGRect(x: (CGFloat(size) - body) / 2, y: (CGFloat(size) - body) / 2 + 10, width: body, height: body)
let shape = CGPath(roundedRect: frame, cornerWidth: radius, cornerHeight: radius, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 24, color: CGColor(gray: 0, alpha: 0.32))
ctx.addPath(shape); ctx.setFillColor(CGColor(gray: 0, alpha: 1)); ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(shape); ctx.clip()
ctx.draw(logo, in: frame)
ctx.restoreGState()
let out = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: out)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[2]))
print("wrote \(args[2])")
