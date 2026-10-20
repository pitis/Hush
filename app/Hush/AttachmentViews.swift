import AVFoundation
import AppKit
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

/// A message's files in its bubble: images inline, audio playable, anything else as a file row.
struct AttachmentList: View {
    let files: [Attachment]
    let download: (Int64) -> Void
    let open: (Attachment) -> Void

    var body: some View {
        ForEach(files, id: \.id) { a in
            if Files.isImage(a) {
                ImageAttachmentView(attachment: a, download: { download(a.id) }, open: { open(a) })
            } else if Files.isAudio(a) {
                AudioAttachmentView(attachment: a, download: { download(a.id) })
            } else {
                FileAttachmentView(attachment: a, download: { download(a.id) }, open: { open(a) })
            }
        }
    }
}

/// What a file that isn't there yet offers: download, a spinner, or try again.
struct DownloadState: View {
    let state: AttachmentState
    let download: () -> Void

    var body: some View {
        switch state {
        case .downloading:
            ProgressView().controlSize(.small)
        case .failed:
            Button(action: download) {
                Label("Retry", systemImage: "exclamationmark.arrow.circlepath")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.orange)
        case .pending:
            Button(action: download) {
                Image(systemName: "arrow.down.circle.fill").font(.system(size: 26))
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Download")
        case .ready:
            EmptyView()
        }
    }
}

struct ImageAttachmentView: View {
    let attachment: Attachment
    let download: () -> Void
    let open: () -> Void
    @State private var image: CGImage?
    @State private var undecodable = false

    var body: some View {
        let size = Files.displaySize(width: attachment.width, height: attachment.height)
        let path = attachment.state == .ready ? attachment.path : nil
        if undecodable {
            // a file that says it is an image but can't be shown: still there to open
            FileAttachmentView(attachment: attachment, download: download, open: open)
        } else {
            Group {
                if let path, let image = image ?? Thumbnails.cached(path) {
                    Button(action: open) {
                        Image(decorative: image, scale: 1).resizable().scaledToFill()
                            .frame(width: size.width, height: size.height)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(attachment.fileName ?? "Photo")
                } else {
                    Rectangle().fill(.black.opacity(0.25))
                        .overlay { DownloadState(state: attachment.state, download: download) }
                        .accessibilityLabel("Photo, \(Files.size(attachment.size))")
                }
            }
            .frame(width: size.width, height: size.height)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .task(id: path) {
                guard let path else { return }
                image = await Thumbnails.load(path)
                undecodable = image == nil
            }
        }
    }
}

struct FileAttachmentView: View {
    let attachment: Attachment
    let download: () -> Void
    let open: () -> Void

    var body: some View {
        Button {
            attachment.state == .ready ? open() : download()
        } label: {
            HStack(spacing: 10) {
                Image(nsImage: NSWorkspace.shared.icon(for: UTType(mimeType: attachment.contentType) ?? .data))
                    .resizable().frame(width: 32, height: 32)
                VStack(alignment: .leading, spacing: 2) {
                    Text(attachment.fileName ?? "File").font(.system(size: 13, weight: .medium)).lineLimit(1)
                    Text(Files.size(attachment.size)).font(.system(size: 11)).opacity(0.7)
                }
                Spacer(minLength: 8)
                DownloadState(state: attachment.state, download: download)
            }
            .frame(width: 260)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// Voice notes and audio files: play/pause, how far, how long.
struct AudioAttachmentView: View {
    let attachment: Attachment
    let download: () -> Void
    @State private var player: AVPlayer?
    @State private var opened: URL?
    @State private var playing = false
    @State private var unplayable = false
    @State private var elapsed: Double = 0
    @State private var duration: Double = 0
    @State private var observer: Any?

    var body: some View {
        HStack(spacing: 10) {
            if unplayable {
                Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                Text("This recording can't be played").font(.system(size: 12))
            } else {
                if attachment.state == .ready {
                    Button(action: toggle) {
                        Image(systemName: playing ? "pause.circle.fill" : "play.circle.fill").font(.system(size: 28))
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(playing ? "Pause" : "Play")
                } else {
                    DownloadState(state: attachment.state, download: download)
                }
                Track(fraction: duration > 0 ? elapsed / duration : 0)
                Text(Self.clock(playing || elapsed > 0 ? elapsed : duration))
                    .font(.system(size: 11).monospacedDigit()).opacity(0.8)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(attachment.isVoiceNote ? "Voice message" : (attachment.fileName ?? "Audio"))
        .onDisappear { stop() }
        .task(id: attachment.state) {
            // the length shows before playing, as in Desktop (read from the stored file: no link)
            guard attachment.state == .ready, let path = attachment.path else { return }
            let asset = AVURLAsset(
                url: URL(fileURLWithPath: path), options: [AVURLAssetOverrideMIMETypeKey: attachment.contentType])
            if let length = try? await asset.load(.duration).seconds, length.isFinite { duration = length }
        }
    }

    private func toggle() {
        if player == nil { prepare() }
        guard let player else { return }
        if playing { player.pause() } else { player.play() }
        playing.toggle()
        guard playing else { return }
        // a format AVFoundation can't play fails after starting: say so instead of a stuck Pause
        Task {
            try? await Task.sleep(for: .milliseconds(500))
            if player.currentItem?.status == .failed {
                stop()
                unplayable = true
            }
        }
    }

    private func prepare() {
        guard let url = try? Files.openable(attachment) else {
            unplayable = true
            return
        }
        opened = url
        let asset = AVURLAsset(url: url, options: [AVURLAssetOverrideMIMETypeKey: attachment.contentType])
        let item = AVPlayerItem(asset: asset)
        let player = AVPlayer(playerItem: item)
        observer = player.addPeriodicTimeObserver(forInterval: CMTime(value: 1, timescale: 10), queue: .main) {
            time in
            MainActor.assumeIsolated {
                elapsed = time.seconds
                let total = item.duration.seconds
                if total.isFinite { duration = total }
                if total.isFinite, time.seconds >= total {
                    // played to the end: back to the start, ready to play again
                    playing = false
                    elapsed = 0
                    player.seek(to: .zero)
                }
            }
        }
        self.player = player
    }

    private func stop() {
        player?.pause()
        if let observer { player?.removeTimeObserver(observer) }
        observer = nil
        player = nil
        playing = false
        if let opened { Files.close(opened) }
        opened = nil
    }

    static func clock(_ seconds: Double) -> String {
        guard seconds.isFinite, seconds > 0 else { return "0:00" }
        let s = Int(seconds.rounded())
        return "\(s / 60):" + String(format: "%02d", s % 60)
    }
}

/// How far a recording has played.
struct Track: View {
    let fraction: Double

    var body: some View {
        ZStack(alignment: .leading) {
            // the bubble's words' colour: white on blue, dark on light grey
            Capsule().fill(.quaternary)
            Capsule().fill(.primary).frame(width: 140 * min(max(fraction, 0), 1))
        }
        .frame(width: 140, height: 4)
        .accessibilityHidden(true)
    }
}

/// A received link preview: image, title and site. Only https links open.
struct LinkPreviewCard: View {
    let preview: LinkPreview

    var body: some View {
        let url = URL(string: preview.url).flatMap { $0.scheme?.lowercased() == "https" ? $0 : nil }
        Button {
            if let url { NSWorkspace.shared.open(url) }
        } label: {
            HStack(spacing: 10) {
                if let image = preview.image, image.state == .ready, let path = image.path {
                    ThumbnailImage(path: path, shownAt: 52).frame(width: 52, height: 52)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                }
                VStack(alignment: .leading, spacing: 2) {
                    if let title = preview.title {
                        Text(title).font(.system(size: 13, weight: .semibold)).lineLimit(2)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Text(url?.host() ?? preview.url).font(.system(size: 11)).opacity(0.7).lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            .frame(width: 280)
            .padding(8)
            .background(.black.opacity(0.18), in: RoundedRectangle(cornerRadius: 10))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(url == nil)
    }
}

/// A small picture of an image file, decoded off the main thread.
struct ThumbnailImage: View {
    let path: String
    /// The longest side it is shown at, in points; nil: an image bubble's.
    var shownAt: CGFloat? = nil
    @Environment(\.displayScale) private var scale
    @State private var image: CGImage?

    private var maxPixels: CGFloat { shownAt.map { ($0 * scale).rounded(.up) } ?? Thumbnails.maxPixels }

    var body: some View {
        Group {
            if let image = image ?? Thumbnails.cached(path, maxPixels: maxPixels) {
                Image(decorative: image, scale: 1).resizable().scaledToFill()
            } else {
                Rectangle().fill(.black.opacity(0.25))
            }
        }
        .task(id: "\(maxPixels) \(path)") { image = await Thumbnails.load(path, maxPixels: maxPixels) }
    }
}

/// Downsampled images, decoded off the main thread and kept in memory: decoding a full photo
/// while drawing would stall scrolling.
@MainActor
enum Thumbnails {
    /// Each image costs its bitmap's bytes: past 32 MB, NSCache lets some go.
    private static let cache = {
        let cache = NSCache<NSString, CGImage>()
        cache.totalCostLimit = 32 << 20
        return cache
    }()
    /// Files that didn't decode, not tried again on every draw.
    private static var undecodable: Set<String> = []
    /// An image bubble's longest side: two bubble widths, for a Retina screen.
    nonisolated static let maxPixels = 2 * Files.imageBox

    static func cached(_ path: String, maxPixels: CGFloat = Thumbnails.maxPixels) -> CGImage? {
        cache.object(forKey: key(path, maxPixels))
    }

    static func load(_ path: String, maxPixels: CGFloat = Thumbnails.maxPixels) async -> CGImage? {
        if let hit = cached(path, maxPixels: maxPixels) { return hit }
        if undecodable.contains(path) { return nil }
        let image = await Task.detached(priority: .userInitiated) { decode(path, maxPixels) }.value
        if let image {
            cache.setObject(image, forKey: key(path, maxPixels), cost: image.bytesPerRow * image.height)
        } else {
            undecodable.insert(path)
        }
        return image
    }

    private static func key(_ path: String, _ maxPixels: CGFloat) -> NSString {
        "\(Int(maxPixels)) \(path)" as NSString
    }

    nonisolated private static func decode(_ path: String, _ maxPixels: CGFloat) -> CGImage? {
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixels,
        ]
        guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
        return CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
    }
}
