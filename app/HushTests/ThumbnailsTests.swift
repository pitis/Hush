import ImageIO
import UniformTypeIdentifiers
import XCTest

@testable import Hush

@MainActor
final class ThumbnailsTests: XCTestCase {
    /// Writes a `width`×`height` PNG.
    private func picture(width: Int, height: Int) throws -> String {
        let context = CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        context.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(destination, context.makeImage()!, nil)
        XCTAssertTrue(CGImageDestinationFinalize(destination))
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url.path
    }

    func testAPictureShownSmallIsDecodedSmall() async throws {
        let path = try picture(width: 2000, height: 1000)

        let small = await Thumbnails.load(path, maxPixels: 80)
        let avatar = try XCTUnwrap(small)
        XCTAssertEqual([avatar.width, avatar.height], [80, 40])
        // the bubble's decode of the same file is its own
        let large = await Thumbnails.load(path)
        let bubble = try XCTUnwrap(large)
        XCTAssertEqual(bubble.width, Int(Thumbnails.maxPixels))
        XCTAssertEqual(Thumbnails.cached(path, maxPixels: 80)?.width, 80)
    }
}
