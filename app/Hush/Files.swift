import Foundation
import ImageIO
import UniformTypeIdentifiers

/// Attachments in the app: how they are described, laid out, opened, and prepared for sending.
enum Files {
    /// A message with files, worded like Signal Desktop's lists and notifications: the kind's
    /// emoji, then the caption or the kind ("📷 Photo", "📷 sunset").
    static func describe(_ file: FileSummary, body: String?) -> String {
        let (emoji, word): (String, String) =
            switch file.contentType {
            case "image/gif": ("🎡", "GIF")
            case let t where t.hasPrefix("image/"): ("📷", "Photo")
            case let t where t.hasPrefix("video/"): ("🎥", "Video")
            case _ where file.isVoiceNote: ("🎤", "Voice Message")
            case let t where t.hasPrefix("audio/"): ("🔈", "Audio Message")
            default: ("📎", "File")
            }
        let caption = body?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return "\(emoji) \(caption.isEmpty ? word : caption)"
    }

    static func summary(_ a: Attachment) -> FileSummary {
        FileSummary(contentType: a.contentType, fileName: a.fileName, isVoiceNote: a.isVoiceNote)
    }

    static func isImage(_ a: Attachment) -> Bool { a.contentType.hasPrefix("image/") && !a.isVoiceNote }
    static func isAudio(_ a: Attachment) -> Bool { a.isVoiceNote || a.contentType.hasPrefix("audio/") }

    static let imageBox: CGFloat = 320

    /// An image's size in a bubble: its shape, at most `imageBox` either way, never enlarged.
    static func displaySize(width: UInt32?, height: UInt32?) -> CGSize {
        guard let width, let height, width > 0, height > 0 else { return CGSize(width: 240, height: 180) }
        let (w, h) = (CGFloat(width), CGFloat(height))
        let scale = min(1, imageBox / w, imageBox / h)
        return CGSize(width: (w * scale).rounded(.down), height: (h * scale).rounded(.down))
    }

    /// A file chosen, dropped or pasted, described for the core: its MIME type, and pixel size
    /// for images (as the photo shows, so rotated photos are swapped).
    static func outgoing(_ url: URL) -> OutgoingFile {
        let type = UTType(filenameExtension: url.pathExtension)
        var width: UInt32?
        var height: UInt32?
        if type?.conforms(to: .image) == true, let source = CGImageSourceCreateWithURL(url as CFURL, nil),
            let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
            let w = props[kCGImagePropertyPixelWidth] as? UInt32, let h = props[kCGImagePropertyPixelHeight] as? UInt32
        {
            // EXIF orientations 5–8 turn the picture a quarter
            let turned = (5...8).contains(props[kCGImagePropertyOrientation] as? Int ?? 1)
            (width, height) = turned ? (h, w) : (w, h)
        }
        return OutgoingFile(
            path: url.path, contentType: type?.preferredMIMEType ?? "application/octet-stream", width: width,
            height: height, isVoiceNote: false)
    }

    /// Where opened files are linked; swapped only by tests.
    static let defaultOpenRoot = FileManager.default.temporaryDirectory.appendingPathComponent(
        "Hush-open", isDirectory: true)
    nonisolated(unsafe) static var openRoot = defaultOpenRoot

    /// Where pasted pictures wait to be sent; swapped only by tests.
    static let defaultPasteRoot = FileManager.default.temporaryDirectory.appendingPathComponent(
        "Hush-paste", isDirectory: true)
    nonisolated(unsafe) static var pasteRoot = defaultPasteRoot

    enum OpenError: LocalizedError {
        case notDownloaded

        var errorDescription: String? { "it isn't downloaded" }
    }

    /// The downloaded file under its own name (or one with the right extension), for Quick Look
    /// and players: stored files are named by id only. A hard link (a copy across volumes),
    /// made fresh each time; `close` removes it, so a message deleted for everyone leaves nothing
    /// readable here.
    static func openable(_ a: Attachment) throws -> URL {
        guard a.state == .ready, let path = a.path else { throw OpenError.notDownloaded }
        let ext = UTType(mimeType: a.contentType)?.preferredFilenameExtension
        // only the last part of a sender-chosen name, so it stays in our folder
        let given = a.fileName.map { URL(fileURLWithPath: $0).lastPathComponent }
            .flatMap { ["", ".", "..", "/"].contains($0) ? nil : $0 }
        let name = given ?? (ext.map { "file.\($0)" } ?? "file")
        let dir = openRoot.appendingPathComponent(String(a.id), isDirectory: true)
        let url = dir.appendingPathComponent(name)
        let fm = FileManager.default
        try? fm.removeItem(at: dir)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        do {
            try fm.linkItem(at: URL(fileURLWithPath: path), to: url)
        } catch {
            try fm.copyItem(at: URL(fileURLWithPath: path), to: url)  // e.g. another volume
        }
        // a link marks the stored file too: harmless, Hush only reads it
        try quarantine(url)
        return url
    }

    /// Marks a file from a chat as come from someone else, as a browser marks its downloads:
    /// Gatekeeper checks it before it runs (a `.command` a contact sent, opened from Quick Look's
    /// Open With or from Finder). A file already marked keeps its mark, and with it any "Open
    /// Anyway" given before.
    static func quarantine(_ file: URL) throws {
        let name = "com.apple.quarantine"
        guard getxattr(file.path, name, nil, 0, 0, 0) < 0 else { return }
        // Launch Services' download flags, written directly: through its API each mark would also
        // be a lasting row in its quarantine events database, a record of when files were opened
        let mark = "0081;\(String(Int(Date().timeIntervalSince1970), radix: 16));Hush;"
        guard setxattr(file.path, name, mark, mark.utf8.count, 0, 0) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
    }

    /// Quarantines each file in `folder` (an export's `files/`), as many as it can: what failed.
    static func quarantineFiles(in folder: URL) -> [Error] {
        do {
            return try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil).compactMap {
                do {
                    try quarantine($0)
                    return nil
                } catch { return error }
            }
        } catch { return [error] }
    }

    /// Done with an opened file: its link goes.
    static func close(_ url: URL) {
        try? FileManager.default.removeItem(at: url.deletingLastPathComponent())
    }

    /// Files out of the tray, sent (the core keeps its own copy) or dropped: the pictures pasted
    /// into it go. Files chosen or dropped from the user's folders are theirs and stay.
    static func discard(_ paths: [String]) {
        let root = pasteRoot.standardizedFileURL.path
        for path in paths {
            let file = URL(fileURLWithPath: path).standardizedFileURL  // no `..` climbing out
            if file.deletingLastPathComponent().path == root { try? FileManager.default.removeItem(at: file) }
        }
    }

    /// At launch, once the core holds the data folder: what a previous run left open or pasted
    /// goes.
    static func clearLeftovers() {
        for root in [openRoot, pasteRoot] { try? FileManager.default.removeItem(at: root) }
    }

    /// Of dropped or pasted items, the ones that can be sent: files (not folders) that exist.
    static func attachable(_ urls: [URL]) -> [URL] {
        urls.filter { (try? $0.resourceValues(forKeys: [.isRegularFileKey]))?.isRegularFile == true }
    }

    static func size(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
