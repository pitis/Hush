import AppKit
import SwiftUI
import XCTest

@testable import Semaphore

final class AppearanceTests: XCTestCase {
    func testTheChoicesMapToColorSchemesDarkFirst() {
        XCTAssertEqual(Appearance.allCases.map(\.label), ["System", "Light", "Dark"])
        XCTAssertEqual(Appearance.default, .dark, "as before: the app was dark only")
        XCTAssertNil(Appearance.system.scheme)
        XCTAssertEqual(Appearance.light.scheme, .light)
        XCTAssertEqual(Appearance.dark.scheme, .dark)
    }

    func testThemeColoursFollowTheAppearance() {
        let resolve = { (color: Color, name: NSAppearance.Name) -> CGFloat in
            var white: CGFloat = 0
            NSAppearance(named: name)!.performAsCurrentDrawingAppearance {
                NSColor(color).usingColorSpace(.deviceGray)!.getWhite(&white, alpha: nil)
            }
            return white
        }
        XCTAssertGreaterThan(resolve(Theme.window, .aqua), 0.9, "light window")
        XCTAssertLessThan(resolve(Theme.window, .darkAqua), 0.2, "dark window")
        XCTAssertLessThan(resolve(Theme.incomingText, .aqua), 0.2, "dark words on a light bubble")
        XCTAssertGreaterThan(resolve(Theme.incomingText, .darkAqua), 0.9)
    }
}
