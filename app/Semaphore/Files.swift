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

    /// The downloaded file under its own name (or one with the right extension), for Quick Look,
    /// players and "Open": stored files are named by id only. A hard link, so nothing is copied.
    static func openable(_ a: Attachment) -> URL? {
        guard a.state == .ready, let path = a.path else { return nil }
        let ext = UTType(mimeType: a.contentType)?.preferredFilenameExtension
        // only the last part of a sender-chosen name, so it stays in our folder
        let given = a.fileName.map { URL(fileURLWithPath: $0).lastPathComponent }.flatMap { $0.isEmpty ? nil : $0 }
        let name = given ?? (ext.map { "file.\($0)" } ?? "file")
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("Semaphore-open", isDirectory: true)
            .appendingPathComponent(String(a.id), isDirectory: true)
        let url = dir.appendingPathComponent(name)
        if FileManager.default.fileExists(atPath: url.path) { return url }
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try FileManager.default.linkItem(at: URL(fileURLWithPath: path), to: url)
            return url
        } catch {
            return nil
        }
    }

    static func size(_ bytes: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
    }
}
