import CoreImage
import XCTest

@testable import Semaphore

final class QRCodeTests: XCTestCase {
    func testQRCodeDecodesBackToTheLinkURL() throws {
        let url = "sgnl://linkdevice?uuid=AbC-123_x&pub_key=BQ%2Bxyz%3D%3D"
        let image = try XCTUnwrap(QRCode.image(for: url))
        let cg = try XCTUnwrap(image.cgImage(forProposedRect: nil, context: nil, hints: nil))
        let detector = try XCTUnwrap(
            CIDetector(
                ofType: CIDetectorTypeQRCode, context: nil, options: [CIDetectorAccuracy: CIDetectorAccuracyHigh]))
        let features = detector.features(in: CIImage(cgImage: cg)).compactMap { $0 as? CIQRCodeFeature }
        XCTAssertEqual(features.first?.messageString, url)
    }
}
