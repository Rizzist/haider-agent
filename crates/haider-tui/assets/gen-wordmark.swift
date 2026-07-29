// gen-wordmark.swift — rasterize the Arabic "حيدر" (Haider) wordmark to a
// transparent PNG via CoreText, which shapes Arabic natively (contextual
// joining, ligatures, RTL). Run offline; the PNG is bundled into haider-tui and
// displayed through the terminal-graphics pipeline (Kitty/iTerm2) with an ASCII
// fallback. This is the deterministic source of the bundled asset.
//
// usage: swift gen-wordmark.swift <font-family> <pt> <hexRRGGBB> <out.png> [text]
//   e.g. swift gen-wordmark.swift "DecoType Naskh" 256 D9A441 wordmark-haider-gold.png

import CoreText
import CoreGraphics
import ImageIO
import Foundation
import UniformTypeIdentifiers

let args = CommandLine.arguments
guard args.count >= 5 else {
    FileHandle.standardError.write("usage: gen-wordmark.swift <font> <pt> <hex> <out.png> [text]\n".data(using: .utf8)!)
    exit(2)
}
let fontName = args[1]
let pt = CGFloat(Double(args[2]) ?? 256)
let hex = args[3]
let outPath = args[4]
let text = args.count >= 6 ? args[5] : "حيدر"

func color(_ hex: String) -> CGColor {
    var v: UInt64 = 0
    Scanner(string: hex).scanHexInt64(&v)
    let r = CGFloat((v >> 16) & 0xff) / 255.0
    let g = CGFloat((v >> 8) & 0xff) / 255.0
    let b = CGFloat(v & 0xff) / 255.0
    return CGColor(srgbRed: r, green: g, blue: b, alpha: 1.0)
}

let font = CTFontCreateWithName(fontName as CFString, pt, nil)
let attrs: [NSAttributedString.Key: Any] = [
    .init(rawValue: kCTFontAttributeName as String): font,
    .init(rawValue: kCTForegroundColorAttributeName as String): color(hex),
]
let attr = NSAttributedString(string: text, attributes: attrs)
let line = CTLineCreateWithAttributedString(attr)

// Draw onto a generously oversized canvas so nothing clips, then tight-crop to
// the actual ink pixels — typographic/optical bounds are far looser than the
// real strokes for these display faces, which left the wordmark floating in
// whitespace. The pixel scan gives a box hugging the ink.
let ink = CTLineGetBoundsWithOptions(line, [.useOpticalBounds])
let margin = pt * 1.0
let w = Int(ceil(ink.width + margin * 2))
let h = Int(ceil(ink.height + margin * 2))

let cs = CGColorSpaceCreateDeviceRGB()
let bmi = CGImageAlphaInfo.premultipliedLast.rawValue
guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8,
                          bytesPerRow: 0, space: cs, bitmapInfo: bmi) else {
    FileHandle.standardError.write("context failed\n".data(using: .utf8)!); exit(1)
}
ctx.clear(CGRect(x: 0, y: 0, width: w, height: h))   // transparent
ctx.setAllowsAntialiasing(true)
ctx.setShouldAntialias(true)
ctx.setShouldSubpixelPositionFonts(true)
ctx.setShouldSmoothFonts(true)
ctx.textPosition = CGPoint(x: margin - ink.origin.x, y: margin - ink.origin.y)
CTLineDraw(line, ctx)

// Scan the alpha channel for the ink bounding box. CG bitmap data row 0 is the
// BOTTOM row (origin bottom-left); CGImage crop space is top-left origin, so we
// flip Y when converting.
let bpr = ctx.bytesPerRow
guard let raw = ctx.data else {
    FileHandle.standardError.write("no ctx data\n".data(using: .utf8)!); exit(1)
}
let buf = raw.bindMemory(to: UInt8.self, capacity: bpr * h)
var minX = w, minY = h, maxX = -1, maxY = -1
for dy in 0..<h {
    let rowoff = dy * bpr
    for dx in 0..<w {
        if buf[rowoff + dx * 4 + 3] > 12 {   // alpha threshold
            if dx < minX { minX = dx }
            if dx > maxX { maxX = dx }
            if dy < minY { minY = dy }
            if dy > maxY { maxY = dy }
        }
    }
}
guard maxX >= 0 else {
    FileHandle.standardError.write("no ink drawn (font missing glyphs?)\n".data(using: .utf8)!); exit(1)
}
let pad = Int(ceil(pt * 0.10))
let cropX = max(0, minX - pad)
let cropXR = min(w - 1, maxX + pad)
// data rows are bottom-up; convert the [minY,maxY] data span to a top-left rect.
let topDataY = maxY            // visually-highest ink is the largest data row
let botDataY = minY
let cropTop = max(0, (h - 1 - topDataY) - pad)
let cropBot = min(h - 1, (h - 1 - botDataY) + pad)
let cw = cropXR - cropX + 1
let chh = cropBot - cropTop + 1

guard let full = ctx.makeImage(),
      let img = full.cropping(to: CGRect(x: cropX, y: cropTop, width: cw, height: chh)) else {
    FileHandle.standardError.write("image/crop failed\n".data(using: .utf8)!); exit(1)
}
let url = URL(fileURLWithPath: outPath)
guard let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else {
    FileHandle.standardError.write("dest failed\n".data(using: .utf8)!); exit(1)
}
CGImageDestinationAddImage(dest, img, nil)
if CGImageDestinationFinalize(dest) {
    print("wrote \(outPath)  \(w)x\(h)  font=\(fontName) pt=\(Int(pt)) hex=\(hex) text=\(text)")
} else {
    FileHandle.standardError.write("finalize failed\n".data(using: .utf8)!); exit(1)
}
