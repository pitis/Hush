import ImageIO
import UniformTypeIdentifiers
import XCTest

@testable import Hush

final class GroupPictureTests: XCTestCase {
    /// Writes a `width`×`height` image (red, its left half transparent) as `type`, with an EXIF
    /// orientation.
    private func picture(_ type: UTType, width: Int, height: Int, orientation: Int = 1) throws -> URL {
        let context = CGContext(
            data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        context.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
        context.fill(CGRect(x: width / 2, y: 0, width: width - width / 2, height: height))
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let destination = CGImageDestinationCreateWithURL(url as CFURL, type.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(
            destination, context.makeImage()!, [kCGImagePropertyOrientation: orientation] as CFDictionary)
        XCTAssertTrue(CGImageDestinationFinalize(destination))
        return url
    }

    private func decode(_ data: Data) -> CGImage {
        CGImageSourceCreateImageAtIndex(CGImageSourceCreateWithData(data as CFData, nil)!, 0, nil)!
    }

    /// The colour of pixel (x, y) from the top left, as 0...255 RGB.
    private func pixel(_ image: CGImage, _ x: Int, _ y: Int) -> [Int] {
        var bytes = [UInt8](repeating: 0, count: 4)
        let context = CGContext(
            data: &bytes, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        context.draw(image, in: CGRect(x: -x, y: y - image.height + 1, width: image.width, height: image.height))
        return bytes.prefix(3).map(Int.init)
    }

    func testABigPictureShrinksToFitAndTransparencyTurnsWhite() throws {
        let url = try picture(.png, width: 2000, height: 1000)
        let jpeg = try XCTUnwrap(GroupPicture.jpeg(from: url))
        let image = decode(jpeg)
        XCTAssertEqual([image.width, image.height], [1024, 512])
        XCTAssertGreaterThan(pixel(image, 10, 10).min()!, 240, "the transparent half is white, not black")
        let red = pixel(image, 1000, 10)
        XCTAssertGreaterThan(red[0], 200)
        XCTAssertLessThan(red[1], 50)
    }

    func testAPhotoTakenSidewaysComesOutUpright() throws {
        // stored 200×100, shown rotated a quarter turn (as phones save portrait photos)
        let url = try picture(.jpeg, width: 200, height: 100, orientation: 6)
        let image = decode(try XCTUnwrap(GroupPicture.jpeg(from: url)))
        XCTAssertEqual([image.width, image.height], [100, 200])
    }

    func testAFileThatIsntAPictureIsRefused() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try Data("not a picture".utf8).write(to: url)
        XCTAssertNil(GroupPicture.jpeg(from: url))
    }
}
