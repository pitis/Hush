import AppKit
import XCTest

@testable import Hush

final class FilesTests: XCTestCase {
    private func file(_ type: String, voice: Bool = false) -> FileSummary {
        FileSummary(contentType: type, fileName: "x", isVoiceNote: voice)
    }

    func testMessagesWithFilesAreDescribedLikeDesktop() {
        XCTAssertEqual(Files.describe(file("image/jpeg"), body: nil), "📷 Photo")
        XCTAssertEqual(Files.describe(file("image/gif"), body: nil), "🎡 GIF")
        XCTAssertEqual(Files.describe(file("video/mp4"), body: ""), "🎥 Video")
        XCTAssertEqual(Files.describe(file("audio/aac", voice: true), body: nil), "🎤 Voice Message")
        XCTAssertEqual(Files.describe(file("audio/mpeg"), body: nil), "🔈 Audio Message")
        XCTAssertEqual(Files.describe(file("application/pdf"), body: nil), "📎 File")
        // a caption replaces the word, the emoji stays
        XCTAssertEqual(Files.describe(file("image/jpeg"), body: " sunset "), "📷 sunset")
    }

    func testTheChatListUsesTheDescription() {
        var c = conversation("c:a")
        c.lastMessagePreview = ""
        c.lastMessageFile = file("image/png")
        XCTAssertEqual(
            ChatList.line(c, selected: false, typing: false, draft: "", authorName: "A"),
            .message("📷 Photo", tick: nil))
    }

    func testImagesFitTheirBoxKeepingTheirShape() {
        XCTAssertEqual(Files.displaySize(width: 4000, height: 3000), CGSize(width: 320, height: 240))
        XCTAssertEqual(Files.displaySize(width: 1000, height: 3000), CGSize(width: 106, height: 320))
        XCTAssertEqual(Files.displaySize(width: 100, height: 50), CGSize(width: 100, height: 50))
        XCTAssertEqual(Files.displaySize(width: nil, height: nil), CGSize(width: 240, height: 180))
    }

    func testOutgoingFilesKnowTheirTypeAndImageSize() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hush-files-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let png = dir.appendingPathComponent("dot.png")
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 3, pixelsHigh: 2, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        try rep.representation(using: .png, properties: [:])!.write(to: png)
        let image = Files.outgoing(png)
        XCTAssertEqual(image.contentType, "image/png")
        XCTAssertEqual(image.width, 3)
        XCTAssertEqual(image.height, 2)
        XCTAssertFalse(image.isVoiceNote)
        let pdf = dir.appendingPathComponent("plan.pdf")
        try Data("%PDF".utf8).write(to: pdf)
        XCTAssertEqual(Files.outgoing(pdf).contentType, "application/pdf")
        XCTAssertNil(Files.outgoing(pdf).width)
        let odd = dir.appendingPathComponent("blob")
        try Data([1]).write(to: odd)
        XCTAssertEqual(Files.outgoing(odd).contentType, "application/octet-stream")
    }

    func testStoredFilesOpenUnderTheirOwnName() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hush-open-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        Files.openRoot = dir.appendingPathComponent("open")
        defer { Files.openRoot = Files.defaultOpenRoot }
        let stored = dir.appendingPathComponent("42")
        try Data("pdf".utf8).write(to: stored)
        let a = Attachment(
            id: 42, message: MessageKey(author: "a", sentAt: 1), contentType: "application/pdf", fileName: "plan.pdf",
            size: 3, width: nil, height: nil, isVoiceNote: false, path: stored.path, state: .ready)
        let url = try Files.openable(a)
        XCTAssertEqual(url.lastPathComponent, "plan.pdf")
        XCTAssertEqual(try Data(contentsOf: url), Data("pdf".utf8))
        // closed, nothing of it is left outside Hush's folder
        Files.close(url)
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
        // opened again, it is made fresh from the stored file (never a stale one)
        try Data("new".utf8).write(to: dir.appendingPathComponent("43"))
        var other = a
        other.path = dir.appendingPathComponent("43").path
        XCTAssertEqual(try Data(contentsOf: try Files.openable(other)), Data("new".utf8))
        // no name: the type gives the extension
        var unnamed = a
        unnamed.fileName = nil
        XCTAssertEqual(try Files.openable(unnamed).pathExtension, "pdf")
        // a name can't climb out of the folder, or be the folder
        var sneaky = a
        sneaky.fileName = "../../evil.pdf"
        XCTAssertEqual(try Files.openable(sneaky).lastPathComponent, "evil.pdf")
        sneaky.fileName = ".."
        XCTAssertEqual(try Files.openable(sneaky).lastPathComponent, "file.pdf")
        // not downloaded, or its file gone: an error to show, not silence
        var pending = a
        pending.state = .pending
        XCTAssertThrowsError(try Files.openable(pending))
        var gone = a
        gone.path = dir.appendingPathComponent("missing").path
        XCTAssertThrowsError(try Files.openable(gone))
        // what a previous run left behind goes at launch
        Files.clearLeftovers()
        XCTAssertFalse(FileManager.default.fileExists(atPath: Files.openRoot.path))
    }

    /// A file a contact sent (a `.command`, say) gets Gatekeeper's check before it runs from Quick
    /// Look's Open With, as a browser's downloads do.
    func testOpenedFilesAreQuarantined() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hush-quarantine-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        Files.openRoot = dir.appendingPathComponent("open")
        defer { Files.openRoot = Files.defaultOpenRoot }
        let stored = dir.appendingPathComponent("42")
        try Data("echo hi".utf8).write(to: stored)
        let a = Attachment(
            id: 42, message: MessageKey(author: "a", sentAt: 1), contentType: "application/octet-stream",
            fileName: "run.command", size: 7, width: nil, height: nil, isVoiceNote: false, path: stored.path,
            state: .ready)
        XCTAssertEqual(try quarantineAgent(Files.openable(a)), "Hush")
    }

    func testOnlyRegularFilesCanBeAttached() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hush-regular-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("a.txt")
        try Data("x".utf8).write(to: file)
        XCTAssertEqual(Files.attachable([dir, file, dir.appendingPathComponent("missing")]), [file])
    }
}

/// Who marked the file as come from elsewhere (its `com.apple.quarantine`), or nil if unmarked.
func quarantineAgent(_ url: URL) throws -> String? {
    guard getxattr(url.path, "com.apple.quarantine", nil, 0, 0, 0) > 0 else { return nil }
    return try url.resourceValues(forKeys: [.quarantinePropertiesKey])
        .quarantineProperties?[kLSQuarantineAgentNameKey as String] as? String
}

func attachment(_ id: Int64, _ key: MessageKey, _ type: String = "image/jpeg", state: AttachmentState = .ready)
    -> Attachment
{
    Attachment(
        id: id, message: key, contentType: type, fileName: "f", size: 1, width: 4, height: 3, isVoiceNote: false,
        path: state == .ready ? "/tmp/\(id)" : nil, state: state)
}

@MainActor
final class AppStoreFilesTests: XCTestCase {
    private func linkedStore(_ fake: FakeClient) async -> AppStore {
        let store = AppStore(open: { _ in fake })
        await store.boot()
        return store
    }

    private func file(_ name: String) -> OutgoingFile {
        OutgoingFile(path: "/tmp/\(name)", contentType: "image/png", width: 1, height: 1, isVoiceNote: false)
    }

    func testAMessagesFilesAndPreviewLoadWithIt() async {
        let fake = FakeClient(linked: true)
        let key = MessageKey(author: "a", sentAt: 1)
        fake.messageAnswers["c:a"] = [message("c:a", "a", 1)]
        fake.attachmentAnswers[key] = [attachment(7, key)]
        fake.previewAnswers[key] = LinkPreview(
            url: "https://example.org", title: "Example", description: nil, image: nil)
        let store = await linkedStore(fake)
        store.select("c:a")
        await eventually { store.attachments[key]?.map(\.id) == [7] }
        XCTAssertEqual(store.previews[key]?.title, "Example")
        // a finished download refreshes them
        fake.attachmentAnswers[key] = [attachment(7, key, state: .failed)]
        store.handle(.attachmentChanged(conversationId: "c:a", message: key, id: 7))
        await eventually { store.attachments[key]?.first?.state == .failed }
        store.download(7)
        XCTAssertEqual(fake.downloads, [7])
    }

    func testChosenFilesWaitInTheTrayAndGoOutWithTheCaption() async {
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.select("c:a")
        store.addFiles([file("a.png"), file("b.png")])
        store.addFiles([file("a.png")])  // the same file once
        XCTAssertEqual(store.outgoingFiles.map(\.path), ["/tmp/a.png", "/tmp/b.png"])
        store.removeFile("/tmp/b.png")
        let sent = await store.send(" look ")
        XCTAssertTrue(sent)
        XCTAssertEqual(fake.sentFiles.first?.files.map(\.path), ["/tmp/a.png"])
        XCTAssertEqual(fake.sentFiles.first?.caption, "look")
        XCTAssertEqual(fake.sent.count, 0, "files go with the text, not as a separate message")
        XCTAssertTrue(store.outgoingFiles.isEmpty)
        // files alone, no caption
        store.addFiles([file("c.png")])
        let alone = await store.send("")
        XCTAssertTrue(alone)
        XCTAssertNil(fake.sentFiles.last?.caption)
    }

    func testPressingSendTwiceSendsTheFilesOnce() async {
        let fake = FakeClient(linked: true)
        fake.sendDelay = .milliseconds(50)
        let store = await linkedStore(fake)
        store.select("c:a")
        store.addFiles([file("a.png")])
        async let first = store.send("look")
        async let second = store.send("")
        let results = await [first, second]
        XCTAssertEqual(results.filter { $0 }.count, 1)
        XCTAssertEqual(fake.sentFiles.count, 1)
    }

    func testADownloadTheCoreRefusesSaysWhy() async {
        let fake = FakeClient(linked: true)
        fake.downloadError = TestError(description: "not linked")
        let store = await linkedStore(fake)
        store.download(7)
        XCTAssertNotNil(store.lastError)
    }

    /// A pasted picture is Hush's own copy: it goes once sent (the core keeps its copy) or dropped
    /// from the tray; files chosen or dropped from the user's folders stay.
    func testPastedPicturesGoOnceSentOrDropped() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("hush-paste-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        Files.pasteRoot = root.appendingPathComponent("paste")
        defer { Files.pasteRoot = Files.defaultPasteRoot }
        try FileManager.default.createDirectory(at: Files.pasteRoot, withIntermediateDirectories: true)
        func made(_ name: String, _ dir: URL) -> OutgoingFile {
            let url = dir.appendingPathComponent(name)
            try? Data("png".utf8).write(to: url)
            return Files.outgoing(url)
        }
        func exists(_ f: OutgoingFile) -> Bool { FileManager.default.fileExists(atPath: f.path) }
        let fake = FakeClient(linked: true)
        let store = await linkedStore(fake)
        store.select("c:a")
        let (sent, own) = (made("sent.png", Files.pasteRoot), made("own.png", root))
        store.addFiles([sent, own])
        fake.sendError = TestError(description: "offline")
        _ = await store.send("")
        XCTAssertTrue(exists(sent), "a failed send keeps it, back in the tray")
        fake.sendError = nil
        _ = await store.send("")
        XCTAssertFalse(exists(sent))
        XCTAssertTrue(exists(own), "the user's own file isn't Hush's to remove")
        let removed = made("removed.png", Files.pasteRoot)
        store.addFiles([removed, own])
        store.removeFile(removed.path)
        XCTAssertFalse(exists(removed))
        store.removeFile(own.path)
        XCTAssertTrue(exists(own))
        let left = made("left.png", Files.pasteRoot)
        store.addFiles([left])
        store.select("c:b")
        XCTAssertFalse(exists(left), "switching chats empties the tray")
        // what a previous run left pasted goes at launch
        _ = made("leftover.png", Files.pasteRoot)
        Files.clearLeftovers()
        XCTAssertFalse(FileManager.default.fileExists(atPath: Files.pasteRoot.path))
    }

    func testAFailedSendKeepsTheFilesAndSwitchingChatsClearsThem() async {
        let fake = FakeClient(linked: true)
        fake.sendError = TestError(description: "too big")
        let store = await linkedStore(fake)
        store.select("c:a")
        store.addFiles([file("a.png")])
        let sent = await store.send("")
        XCTAssertFalse(sent)
        XCTAssertEqual(store.outgoingFiles.count, 1)
        XCTAssertNotNil(store.lastError)
        store.select("c:b")
        XCTAssertTrue(store.outgoingFiles.isEmpty)
    }
}

final class PasteTests: XCTestCase {
    func testPastedFilesAndImagesBecomeAttachmentsButTextStaysText() throws {
        let board = NSPasteboard(name: NSPasteboard.Name("hush.tests.\(UUID())"))
        defer { board.releaseGlobally() }
        // text: the field pastes it
        board.clearContents()
        board.setString("hello", forType: .string)
        XCTAssertTrue(try Composer.pastedFiles(from: board).isEmpty)
        // text from Pages, Word or Numbers also carries a picture of itself: still text
        board.clearContents()
        board.writeObjects([NSImage(size: NSSize(width: 1, height: 1))])
        board.setString("a paragraph", forType: .string)
        XCTAssertTrue(try Composer.pastedFiles(from: board).isEmpty)
        // files copied in Finder (they come with their names as text too)
        let file = FileManager.default.temporaryDirectory.appendingPathComponent("paste-\(UUID()).txt")
        try Data("x".utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        board.clearContents()
        board.writeObjects([file as NSURL])
        XCTAssertEqual(try Composer.pastedFiles(from: board).map(\.standardizedFileURL), [file.standardizedFileURL])
        // an image (a screenshot to the clipboard): saved as a PNG to send
        let image = NSImage(size: NSSize(width: 2, height: 2))
        image.lockFocus()
        NSColor.red.drawSwatch(in: NSRect(x: 0, y: 0, width: 2, height: 2))
        image.unlockFocus()
        board.clearContents()
        board.writeObjects([image])
        let pasted = try Composer.pastedFiles(from: board)
        XCTAssertEqual(pasted.count, 1)
        XCTAssertEqual(pasted.first?.pathExtension, "png")
        XCTAssertNotNil(NSImage(contentsOf: try XCTUnwrap(pasted.first)))
        XCTAssertEqual(
            pasted.first?.deletingLastPathComponent().standardizedFileURL, Files.pasteRoot.standardizedFileURL)
        try? FileManager.default.removeItem(at: try XCTUnwrap(pasted.first))
    }
}
