// A window of known content for tests that record the real screen (`#[ignore]`d ones in
// src/capture/sck_compat.rs). It never takes focus or clicks, and it quits on its own.
//
//   swiftc -O scripts/test-panel.swift -o ~/Library/Caches/scrawl-port/test-panel
//   test-panel --rect 300,200,800,500 --seconds 6 --mode split
//
// --rect is global top-left points. --mode:
//   split   left half red, right half blue
//   solid   one color, --color R,G,B (0-255, sRGB)
//   noise   new random pixels every display frame
//   flip    black, turning white and back at each --flip-at time (CLOCK_UPTIME_RAW nanoseconds,
//           the clock CMSampleBuffers are stamped with), printing when each change was committed
// --titled gives it a title bar and a normal level, so `scrawl record --window TITLE` finds it.
// Prints {"window":ID} once it's up.

import AppKit
import QuartzCore

var rect = CGRect(x: 300, y: 200, width: 800, height: 500)
var seconds = 6.0
var mode = "split"
var color: [CGFloat] = [255, 0, 255]
var flips: [UInt64] = []
var title: String?
var args = CommandLine.arguments.dropFirst().makeIterator()
while let arg = args.next() {
  switch arg {
  case "--rect":
    let v = (args.next() ?? "").split(separator: ",").compactMap { Double($0) }
    if v.count == 4 { rect = CGRect(x: v[0], y: v[1], width: v[2], height: v[3]) }
  case "--seconds": seconds = Double(args.next() ?? "") ?? seconds
  case "--mode": mode = args.next() ?? mode
  case "--color": color = (args.next() ?? "").split(separator: ",").compactMap { Double($0) }.map { CGFloat($0) }
  case "--flip-at": flips = (args.next() ?? "").split(separator: ",").compactMap { UInt64($0) }
  case "--titled": title = args.next()
  default: fatalError("unknown argument \(arg)")
  }
}

func srgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat) -> CGColor {
  CGColor(srgbRed: r / 255, green: g / 255, blue: b / 255, alpha: 1)
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let primary = NSScreen.screens[0].frame.height
let frame = NSRect(x: rect.minX, y: primary - rect.maxY, width: rect.width, height: rect.height)
let window = title == nil
  ? NSPanel(contentRect: frame, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
  : NSWindow(contentRect: frame, styleMask: [.titled], backing: .buffered, defer: false)
if let title { window.title = title } else { window.level = .floating }
window.ignoresMouseEvents = true
window.isReleasedWhenClosed = false
let root = NSView(frame: NSRect(origin: .zero, size: rect.size))
root.wantsLayer = true
window.contentView = root
let base = root.layer!

switch mode {
case "split":
  base.backgroundColor = srgb(255, 0, 0)
  let right = CALayer()
  right.frame = CGRect(x: rect.width / 2, y: 0, width: rect.width / 2, height: rect.height)
  right.backgroundColor = srgb(0, 0, 255)
  base.addSublayer(right)
case "solid": base.backgroundColor = srgb(color[0], color[1], color[2])
case "flip": base.backgroundColor = srgb(0, 0, 0)
case "noise":
  let (w, h) = (Int(rect.width), Int(rect.height))
  let space = CGColorSpace(name: CGColorSpace.sRGB)!
  var pixels = [UInt8](repeating: 255, count: w * h * 4)
  Timer.scheduledTimer(withTimeInterval: 1.0 / 60, repeats: true) { _ in
    pixels.withUnsafeMutableBytes { arc4random_buf($0.baseAddress, $0.count) }
    let context = CGContext(data: &pixels, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
                            bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    CATransaction.begin()
    CATransaction.setDisableActions(true)
    base.contents = context.makeImage()
    CATransaction.commit()
  }
default: fatalError("unknown mode \(mode)")
}
window.orderFrontRegardless()
print("{\"window\":\(window.windowNumber)}")
fflush(stdout)

if !flips.isEmpty {
  Thread.detachNewThread {
    for (index, at) in flips.enumerated() {
      while clock_gettime_nsec_np(CLOCK_UPTIME_RAW) + 2_000_000 < at { usleep(500) }
      while clock_gettime_nsec_np(CLOCK_UPTIME_RAW) < at {}
      DispatchQueue.main.sync {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        base.backgroundColor = index % 2 == 0 ? srgb(255, 255, 255) : srgb(0, 0, 0)
        CATransaction.commit()
        CATransaction.flush()
      }
      print("{\"flip\":\(index),\"at\":\(at),\"committed\":\(clock_gettime_nsec_np(CLOCK_UPTIME_RAW))}")
      fflush(stdout)
    }
  }
}
Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in exit(0) }
app.run()
