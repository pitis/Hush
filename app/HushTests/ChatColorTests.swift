import SwiftUI
import XCTest

@testable import Hush

@MainActor
final class ChatColorTests: XCTestCase {
    func testGradientsRunAtSignalsAngles() {
        let close = { (p: UnitPoint, x: Double, y: Double) in
            XCTAssertEqual(p.x, x, accuracy: 0.0001)
            XCTAssertEqual(p.y, y, accuracy: 0.0001)
        }
        let up = ChatColors.points(angle: 0)  // bottom to top
        close(up.start, 0.5, 1)
        close(up.end, 0.5, 0)
        let right = ChatColors.points(angle: 90)  // left to right
        close(right.start, 0, 0.5)
        close(right.end, 1, 0.5)
        let down = ChatColors.points(angle: 180)
        close(down.start, 0.5, 0)
        close(down.end, 0.5, 1)
    }

    func testColoursAreReadAsARGB() {
        let c = NSColor(ChatColors.color(0x80FF_0000)).usingColorSpace(.sRGB)!
        XCTAssertEqual(c.redComponent, 1, accuracy: 0.01)
        XCTAssertEqual(c.greenComponent, 0, accuracy: 0.01)
        XCTAssertEqual(c.alphaComponent, 0.5, accuracy: 0.01)
    }

    func testThePresetsAreSignals() {
        XCTAssertEqual(chatColorPresets().count, 22)
        XCTAssertEqual(wallpaperPresets().count, 21)
    }

    func testChoosingAWallpaperAsksTheCore() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.setWallpaper("c:a", ChatColor(colors: [0xFF40_3B91], angle: 0))
        store.setWallpaper("c:b", nil)
        await eventually { fake.flagCalls.count == 2 }
        XCTAssertEqual(fake.flagCalls.sorted(), ["wallpaper c:a [4282399633]", "wallpaper c:b []"])
    }

    func testChoosingAColourAsksTheCore() async {
        let fake = FakeClient(linked: true)
        let store = AppStore(open: { _ in fake })
        await store.boot()
        store.setChatColor("c:a", ChatColor(colors: [0xFFCF_163E], angle: 0))
        store.setChatColor("c:b", nil)
        await eventually { fake.flagCalls.count == 2 }
        XCTAssertEqual(fake.flagCalls.sorted(), ["color c:a [4291761726]", "color c:b []"])
    }
}
