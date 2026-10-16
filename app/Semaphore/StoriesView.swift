import AVKit
import SwiftUI

/// Stories by chat, and how they're drawn.
enum StoryList {
    struct Group: Identifiable, Equatable {
        let conversationId: String
        let stories: [Story]
        var id: String { conversationId }
        var unseen: Bool { stories.contains { !$0.viewed } }
        var newest: Int64 { stories.map(\.sentAt).max() ?? 0 }
    }

    /// One group per chat, oldest story first: ours first, then chats with unseen stories, newest
    /// first within each.
    static func groups(_ stories: [Story], me: String?) -> [Group] {
        let byChat = Dictionary(grouping: stories, by: \.conversationId)
        return byChat.map { Group(conversationId: $0.key, stories: $0.value.sorted { $0.sentAt < $1.sentAt }) }
            .sorted { a, b in
                let ours = { (g: Group) in g.stories.contains { $0.author == me } }
                if ours(a) != ours(b) { return ours(a) }
                if a.unseen != b.unseen { return a.unseen }
                return a.newest > b.newest
            }
    }

    /// `groups` in the order `order` gives (chats that came since go last, in their own order).
    static func ordered(_ groups: [Group], as order: [String]) -> [Group] {
        let place = { (id: String) in order.firstIndex(of: id) ?? Int.max }
        return groups.enumerated().sorted { a, b in
            place(a.element.id) != place(b.element.id) ? place(a.element.id) < place(b.element.id) : a.offset < b.offset
        }
        .map(\.element)
    }

    /// Others' stories not seen yet.
    static func unseenCount(_ stories: [Story], me: String?) -> Int {
        stories.filter { !$0.viewed && $0.author != me }.count
    }

    /// An ARGB number's components, 0...1.
    static func rgba(_ argb: UInt32) -> (red: Double, green: Double, blue: Double, alpha: Double) {
        let c = { (shift: UInt32) in Double((argb >> shift) & 0xFF) / 255 }
        return (c(16), c(8), c(0), c(24))
    }

    static func color(_ argb: UInt32) -> Color {
        let c = rgba(argb)
        return Color(red: c.red, green: c.green, blue: c.blue, opacity: c.alpha)
    }
}

/// The stories window: chats on the left, the story on the right; ←/→ step through them.
struct StoriesView: View {
    let store: AppStore
    @State private var selected: String?
    @State private var index = 0
    /// The chats' order as the window opened: seeing a story doesn't move its chat under ←/→.
    @State private var order: [String] = []
    @Environment(\.dismiss) private var dismiss

    private var groups: [StoryList.Group] {
        StoryList.ordered(StoryList.groups(store.stories, me: store.ownAci), as: order)
    }

    var body: some View {
        HStack(spacing: 0) {
            List(groups, selection: $selected) { group in
                HStack(spacing: 10) {
                    Avatar(id: group.conversationId, name: name(group), size: 30)
                        .padding(3)
                        .overlay(Circle().stroke(group.unseen ? Theme.outgoing : .clear, lineWidth: 2))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(name(group)).font(.system(size: 13, weight: group.unseen ? .semibold : .regular))
                        Text(Formatting().listTime(group.newest)).font(.system(size: 11))
                            .foregroundStyle(Theme.secondaryText)
                    }
                }
                .tag(group.conversationId)
            }
            .frame(width: 220)
            Divider()
            viewer
        }
        .frame(minWidth: 720, minHeight: 520)
        .onAppear {
            order = StoryList.groups(store.stories, me: store.ownAci).map(\.id)
            selected = selected ?? order.first
        }
        .onChange(of: selected) { _, _ in index = 0 }
        .onKeyPress(.leftArrow) { step(-1) ? .handled : .ignored }
        .onKeyPress(.rightArrow) { step(1) ? .handled : .ignored }
        .onKeyPress(.escape) {
            dismiss()
            return .handled
        }
    }

    private var current: Story? {
        guard let group = groups.first(where: { $0.conversationId == selected }), group.stories.indices.contains(index)
        else { return nil }
        return group.stories[index]
    }

    @ViewBuilder private var viewer: some View {
        if let story = current {
            VStack(spacing: 10) {
                HStack {
                    Text(story.author == store.ownAci ? "You" : store.senderName(story.author))
                        .font(.system(size: 13, weight: .semibold))
                    Text(Formatting().listTime(story.sentAt)).font(.system(size: 12))
                        .foregroundStyle(Theme.secondaryText)
                    Spacer()
                    Button {
                        _ = step(-1)
                    } label: {
                        Image(systemName: "chevron.left")
                    }
                    .disabled(index == 0).accessibilityLabel("Previous story")
                    Button {
                        _ = step(1)
                    } label: {
                        Image(systemName: "chevron.right")
                    }
                    .accessibilityLabel("Next story")
                }
                StoryCard(story: story, download: store.download)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .padding(16)
            .task(id: "\(story.author)@\(story.sentAt)") {
                if !story.viewed { await store.storyViewed(story) }
            }
        } else {
            Text("No stories").foregroundStyle(Theme.secondaryText).frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func name(_ group: StoryList.Group) -> String {
        if group.stories.contains(where: { $0.author == store.ownAci }) && !group.conversationId.hasPrefix("g:") {
            return "My stories"
        }
        if let c = store.conversations.first(where: { $0.id == group.conversationId }) { return store.title(c) }
        return store.senderName(group.stories.first?.author ?? "")
    }

    /// The next or previous story, on into the next chat's. False at either end.
    private func step(_ by: Int) -> Bool {
        let list = groups
        guard let g = list.firstIndex(where: { $0.conversationId == selected }) else { return false }
        let next = index + by
        if list[g].stories.indices.contains(next) {
            index = next
            return true
        }
        let other = g + by
        guard list.indices.contains(other) else { return false }
        selected = list[other].conversationId
        DispatchQueue.main.async { index = by > 0 ? 0 : list[other].stories.count - 1 }
        return true
    }
}

/// One story: its words on their background, or its photo or video.
struct StoryCard: View {
    let story: Story
    let download: (Int64) -> Void

    var body: some View {
        if let text = story.text {
            ZStack {
                background(text)
                VStack(spacing: 12) {
                    Text(text.text)
                        .font(font(text.style))
                        .multilineTextAlignment(.center)
                        .foregroundStyle(text.foreground.map(StoryList.color) ?? .white)
                        .padding(text.textBackground == nil ? 0 : 8)
                        .background(
                            text.textBackground.map(StoryList.color) ?? .clear, in: RoundedRectangle(cornerRadius: 8))
                    if let link = text.link, let url = URL(string: link), url.scheme == "https" {
                        Link(url.host() ?? link, destination: url).font(.system(size: 13, weight: .medium))
                            .padding(.horizontal, 12).padding(.vertical, 6)
                            .background(.black.opacity(0.3), in: Capsule()).foregroundStyle(.white)
                    }
                }
                .padding(32)
            }
            .aspectRatio(9 / 16, contentMode: .fit)
            .clipShape(RoundedRectangle(cornerRadius: 14))
        } else if let file = story.file {
            switch file.state {
            case .ready:
                if let path = file.path {
                    if file.contentType.hasPrefix("video/") {
                        StoryVideo(path: path).clipShape(RoundedRectangle(cornerRadius: 14))
                    } else {
                        ThumbnailImage(path: path).scaledToFit().clipShape(RoundedRectangle(cornerRadius: 14))
                    }
                }
            case .downloading:
                ProgressView()
            case .pending:
                // fetched when opened (never before: not every story is looked at)
                ProgressView().task(id: file.id) { download(file.id) }
            case .failed:
                Button("Retry") { download(file.id) }
            }
        } else {
            Text("This story can't be shown").foregroundStyle(Theme.secondaryText)
        }
    }

    @ViewBuilder private func background(_ text: StoryText) -> some View {
        switch text.background.count {
        case 0: Color(red: 0.2, green: 0.27, blue: 0.4)
        case 1: StoryList.color(text.background[0])
        default:
            // Signal's angle: 0 bottom to top, 90 left to right
            let radians = Double(text.angle) * .pi / 180
            let dx = sin(radians) / 2
            let dy = cos(radians) / 2
            LinearGradient(
                colors: text.background.map(StoryList.color),
                startPoint: UnitPoint(x: 0.5 - dx, y: 0.5 + dy), endPoint: UnitPoint(x: 0.5 + dx, y: 0.5 - dy))
        }
    }

    private func font(_ style: UInt32) -> Font {
        switch style {
        case 2: .system(size: 28, weight: .bold)
        case 3: .system(size: 28, design: .serif)
        case 4: .custom("Snell Roundhand", size: 32)
        case 5: .system(size: 28, weight: .semibold).width(.condensed)
        default: .system(size: 28)
        }
    }
}

/// A story's video, with one player for as long as it shows (a redraw doesn't restart it).
struct StoryVideo: View {
    let path: String
    @State private var player: AVPlayer?

    var body: some View {
        VideoPlayer(player: player)
            .task(id: path) {
                player?.pause()
                player = AVPlayer(url: URL(fileURLWithPath: path))
            }
            .onDisappear { player?.pause() }
    }
}
