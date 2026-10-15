import XCTest

@testable import Semaphore

final class FormattingTests: XCTestCase {
    private let vienna = TimeZone(identifier: "Europe/Vienna")!
    private var cal: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = vienna
        c.locale = Locale(identifier: "en_US")
        return c
    }
    private func date(_ y: Int, _ mo: Int, _ d: Int, _ h: Int, _ mi: Int) -> Date {
        cal.date(from: DateComponents(year: y, month: mo, day: d, hour: h, minute: mi))!
    }
    private func ms(_ d: Date) -> Int64 { Int64(d.timeIntervalSince1970 * 1000) }

    func testRelativeTimeBuckets() {
        let now = date(2026, 10, 15, 18, 0)  // a Thursday
        let f = Formatting(calendar: cal)
        XCTAssertEqual(f.listTime(ms(now.addingTimeInterval(-30)), now: now), "Now")
        XCTAssertEqual(f.listTime(ms(now.addingTimeInterval(-11 * 60)), now: now), "11m")
        XCTAssertEqual(f.listTime(ms(date(2026, 10, 15, 9, 5)), now: now), "09:05")
        XCTAssertEqual(f.listTime(ms(date(2026, 10, 13, 9, 5)), now: now), "Tue")
        XCTAssertEqual(f.listTime(ms(date(2026, 7, 18, 9, 5)), now: now), "Jul 18")
        XCTAssertEqual(f.listTime(ms(date(2025, 7, 18, 9, 5)), now: now), "7/18/25")
        XCTAssertEqual(f.listTime(0, now: now), "")
    }

    func testDaySeparators() {
        let now = date(2026, 10, 15, 18, 0)
        let f = Formatting(calendar: cal)
        XCTAssertEqual(f.dayTitle(ms(date(2026, 10, 15, 1, 0)), now: now), "Today")
        XCTAssertEqual(f.dayTitle(ms(date(2026, 10, 14, 23, 0)), now: now), "Yesterday")
        XCTAssertEqual(f.dayTitle(ms(date(2026, 10, 12, 10, 0)), now: now), "Mon, Oct 12")
        XCTAssertEqual(f.dayTitle(ms(date(2025, 10, 12, 10, 0)), now: now), "Oct 12, 2025")
    }

    func testPreviewText() {
        var c = conversation("c:a")
        c.lastMessagePreview = "hi"
        XCTAssertEqual(Formatting.preview(c), "hi")
        c.lastMessagePreview = ""
        XCTAssertEqual(Formatting.preview(c), "Attachment")
        c.lastMessageAt = 0
        XCTAssertEqual(Formatting.preview(c), "")
    }

    func testInitialsAndStableAvatarColor() {
        XCTAssertEqual(Formatting.initials("He Be Hatin"), "HB")
        XCTAssertEqual(Formatting.initials("bob"), "B")
        XCTAssertEqual(Formatting.initials("8 🅰️ test"), "8")
        XCTAssertEqual(Formatting.initials(""), "#")
        XCTAssertEqual(Formatting.avatarIndex("c:abc"), Formatting.avatarIndex("c:abc"))
        XCTAssertTrue((0..<Formatting.avatarPaletteSize).contains(Formatting.avatarIndex("c:xyz")))
    }

    func testFuzzyMatch() {
        XCTAssertTrue(Formatting.matches("He Be Hatin", query: "hbh"))
        XCTAssertTrue(Formatting.matches("He Be Hatin", query: "hatin"))
        XCTAssertTrue(Formatting.matches("anything", query: "  "))
        XCTAssertFalse(Formatting.matches("He Be Hatin", query: "xyz"))
    }
}

final class TimelineTests: XCTestCase {
    private var cal: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Europe/Vienna")!
        return c
    }

    func testDaySeparatorsAndGrouping() {
        let day1 = Int64(1_760_500_000_000)  // some day
        let msgs = [
            message("g:1", "alice", day1),
            message("g:1", "alice", day1 + 60_000),  // same sender, 1 min later → grouped
            message("g:1", "bob", day1 + 120_000),  // other sender → new run
            message("g:1", "bob", day1 + 120_000 + 4 * 60_000),  // same sender 4 min later → new run
            message("g:1", "bob", day1 + 86_400_000),  // next day → separator, new run
        ]
        let items = Timeline.items(for: msgs, calendar: cal)
        let separators = items.filter { if case .day = $0 { return true } else { return false } }
        XCTAssertEqual(separators.count, 2)
        let runStarts = items.compactMap { item -> Bool? in
            if case .message(_, let first, _) = item { return first }
            return nil
        }
        XCTAssertEqual(runStarts, [true, false, true, true, true])
        let runEnds = items.compactMap { item -> Bool? in
            if case .message(_, _, let last) = item { return last }
            return nil
        }
        XCTAssertEqual(runEnds, [false, true, true, true, true])
    }

    func testTicks() {
        XCTAssertEqual(Timeline.tick(for: .pending), .clock)
        XCTAssertEqual(Timeline.tick(for: .sent), .single)
        XCTAssertEqual(Timeline.tick(for: .delivered), .double)
        XCTAssertEqual(Timeline.tick(for: .read), .doubleFilled)
        XCTAssertEqual(Timeline.tick(for: .failed), .failed)
    }
}

final class ComposerTests: XCTestCase {
    func testReturnSendsShiftOrOptionReturnInsertsANewline() {
        XCTAssertEqual(Composer.action(for: []), .send)
        XCTAssertEqual(Composer.action(for: .shift), .newline)
        XCTAssertEqual(Composer.action(for: .option), .newline)
    }

    func testBlankTextCantBeSent() {
        XCTAssertFalse(Composer.canSend(" \n "))
        XCTAssertTrue(Composer.canSend(" hi "))
    }

    func testDraftsAreKeptPerConversation() {
        let defaults = UserDefaults(suiteName: "semaphore.tests.\(UUID())")!
        Composer.defaults = defaults
        defer { Composer.defaults = .standard }
        Composer.saveDraft("for a", for: "c:a")
        Composer.saveDraft("for b", for: "c:b")
        XCTAssertEqual(Composer.draft(for: "c:a"), "for a")
        XCTAssertEqual(Composer.draft(for: "c:b"), "for b")
        Composer.saveDraft("  ", for: "c:a")
        XCTAssertEqual(Composer.draft(for: "c:a"), "")
    }

    func testDisplayTitleFallsBackToAShortId() {
        XCTAssertEqual(AppStore.displayTitle(conversation("c:abcdefgh-1234")), "c:abcdefgh-1234")
        var c = conversation("c:abcdefgh-1234")
        c.title = ""
        XCTAssertEqual(AppStore.displayTitle(c), "abcdefgh")
        var g = conversation("g:00ff")
        g.title = ""
        XCTAssertEqual(AppStore.displayTitle(g), "Group")
    }
}
