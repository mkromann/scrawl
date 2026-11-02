import AppKit
import AVFoundation
import CoreMedia
import CoreVideo
import ScreenCaptureKit
import SwiftUI
import VideoToolbox

// What the helper calls of the macOS 14 and 15 SDKs, for a build against the macOS 13 SDK (Command
// Line Tools on Ventura, `scripts/macos13-env.sh`). Each keeps the SDK's name and shape so the
// call sites read the same on every SDK. `build.rs` defines SCRAWL_SDK_14 from that SDK on.

#if !SCRAWL_SDK_14

// MARK: SwiftUI

/// SwiftUI's `Spring`, as far as `Track` uses it: ω = 2π / duration, ζ = 1 − bounce.
struct Spring {
  let duration: Double
  let bounce: Double

  private var omega: Double { 2 * .pi / duration }
  private var zeta: Double { 1 - bounce }

  /// Until under 0.1% of a unit step is left: (1 + x) e⁻ˣ = 0.001 at x ≈ 9.23 without bounce,
  /// and the envelope e^(−ζωt) / √(1 − ζ²) with it.
  var settlingDuration: Double {
    zeta >= 1 ? 9.2334 / omega : log(1000 / (1 - zeta * zeta).squareRoot()) / (zeta * omega)
  }

  /// `(displacement, velocity)` at `time`, measured from the target.
  private func state(from x0: Double, velocity v0: Double, time t: Double) -> (Double, Double) {
    let decay = exp(-zeta * omega * t)
    if zeta >= 1 {
      let b = v0 + omega * x0
      return ((x0 + b * t) * decay, (b - omega * (x0 + b * t)) * decay)
    }
    let damped = omega * (1 - zeta * zeta).squareRoot()
    let a = x0, b = (v0 + zeta * omega * x0) / damped
    let (sine, cosine) = (sin(damped * t), cos(damped * t))
    let x = a * cosine + b * sine
    return (x * decay, (damped * (b * cosine - a * sine) - zeta * omega * x) * decay)
  }

  func value(fromValue: Double, toValue: Double, initialVelocity: Double, time: Double) -> Double {
    toValue + state(from: fromValue - toValue, velocity: initialVelocity, time: time).0
  }

  func velocity(fromValue: Double, toValue: Double, initialVelocity: Double, time: Double) -> Double {
    state(from: fromValue - toValue, velocity: initialVelocity, time: time).1
  }
}

extension Animation {
  /// The same spring under its older parameters.
  static func spring(duration: Double, bounce: Double) -> Animation {
    .spring(response: duration, dampingFraction: 1 - bounce)
  }
}

private struct GeometryChange<T: Equatable>: PreferenceKey {
  static var defaultValue: T? { nil }
  static func reduce(value: inout T?, nextValue: () -> T?) { value = nextValue() ?? value }
}

extension View {
  /// The two-value `onChange`. The old value is the one captured when the view last updated.
  func onChange<V: Equatable>(of value: V, _ action: @escaping (V, V) -> Void) -> some View {
    onChange(of: value) { [value] new in action(value, new) }
  }

  /// A clear background is measured exactly where the view is, and the preference answers once
  /// at first layout and again whenever the transformed value changes.
  func onGeometryChange<T: Equatable>(
    for _: T.Type,
    of transform: @escaping (GeometryProxy) -> T,
    action: @escaping (T) -> Void
  ) -> some View {
    background(GeometryReader { Color.clear.preference(key: GeometryChange<T>.self, value: transform($0)) })
      .onPreferenceChange(GeometryChange<T>.self) { if let value = $0 { action(value) } }
  }
}

// MARK: Display link

/// `CADisplayLink` and `NSView.displayLink`, macOS 14: `CVDisplayLink` ticks, on the main thread.
/// Like the real one it keeps its target until `invalidate()`.
final class CADisplayLink: NSObject {
  private var target: NSObject?
  private let selector: Selector
  private var link: CVDisplayLink?
  /// When the frame being drawn reaches the screen, on `CACurrentMediaTime`'s clock.
  private(set) var targetTimestamp: CFTimeInterval = 0

  init(target: NSObject, selector: Selector) {
    self.target = target
    self.selector = selector
  }

  func add(to _: RunLoop, forMode _: RunLoop.Mode) {
    var created: CVDisplayLink?
    CVDisplayLinkCreateWithActiveCGDisplays(&created)
    guard let created else { return }
    CVDisplayLinkSetOutputHandler(created) { [weak self] _, _, output, _, _ in
      let time = CFTimeInterval(output.pointee.hostTime) / CVGetHostClockFrequency()
      DispatchQueue.main.async { self?.tick(at: time) }
      return kCVReturnSuccess
    }
    CVDisplayLinkStart(created)
    link = created
  }

  private func tick(at time: CFTimeInterval) {
    targetTimestamp = time
    target?.perform(selector, with: self)
  }

  func invalidate() {
    if let link { CVDisplayLinkStop(link) }
    link = nil
    target = nil
  }
}

extension NSView {
  func displayLink(target: NSObject, selector: Selector) -> CADisplayLink {
    CADisplayLink(target: target, selector: selector)
  }
}

// MARK: AVFoundation

extension AVCaptureDevice.DeviceType {
  /// The name changed in the macOS 14 SDK; a cabled iPhone is the same kind of device.
  static var external: Self { .externalUnknown }
}

extension AVCaptureDevice {
  static var wasConnectedNotification: NSNotification.Name { .AVCaptureDeviceWasConnected }
  static var wasDisconnectedNotification: NSNotification.Name { .AVCaptureDeviceWasDisconnected }
}

extension AVCaptureInput.Port {
  static var formatDescriptionDidChangeNotification: NSNotification.Name { .AVCaptureInputPortFormatDescriptionDidChange }
}

// MARK: ScreenCaptureKit

/// `SCScreenshotManager` (macOS 14): the first complete frame of a stream.
enum SCScreenshotManager {
  /// Long enough for a stream that's slow to start; the first frame doesn't wait for a change.
  private static let timeout: Double = 3

  static func captureImage(contentFilter: SCContentFilter, configuration: SCStreamConfiguration) async throws -> CGImage {
    let grab = FirstFrame()
    let stream = SCStream(filter: contentFilter, configuration: configuration, delegate: nil)
    try stream.addStreamOutput(grab, type: .screen, sampleHandlerQueue: grab.queue)
    try await stream.startCapture()
    defer { Task { try? await stream.stopCapture() } }
    return try await grab.image(timeout: timeout)
  }
}

private final class FirstFrame: NSObject, SCStreamOutput {
  let queue = DispatchQueue(label: "scrawl.first-frame")
  private var waiting: CheckedContinuation<CGImage, Error>?
  /// Kept for whoever asks: a still screen sends one frame, maybe before `image` is called.
  private var result: Result<CGImage, Error>?

  func image(timeout: Double) async throws -> CGImage {
    try await withCheckedThrowingContinuation { continuation in
      queue.async {
        self.waiting = continuation
        self.deliver()
        self.queue.asyncAfter(deadline: .now() + timeout) {
          self.finish(.failure(HelperError("no frame from ScreenCaptureKit")))
        }
      }
    }
  }

  private func finish(_ outcome: Result<CGImage, Error>) {
    if result == nil { result = outcome }
    deliver()
  }

  private func deliver() {
    guard let result, let waiting else { return }
    waiting.resume(with: result)
    self.waiting = nil
  }

  func stream(_: SCStream, didOutputSampleBuffer buffer: CMSampleBuffer, of type: SCStreamOutputType) {
    guard type == .screen, result == nil,
          let attachments = CMSampleBufferGetSampleAttachmentsArray(buffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
          let status = attachments.first?[.status] as? Int, status == SCFrameStatus.complete.rawValue,
          let pixels = CMSampleBufferGetImageBuffer(buffer) else { return }
    var image: CGImage?
    VTCreateCGImageFromCVPixelBuffer(pixels, options: nil, imageOut: &image)
    if let image { finish(.success(image)) }
  }
}

#endif
