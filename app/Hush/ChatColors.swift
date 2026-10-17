import SwiftUI

extension EnvironmentValues {
    /// Our bubbles' colour in the chat shown (None: the app's).
    @Entry var chatColor: ChatColor? = nil
    /// The chat shown has a wallpaper: what isn't in a bubble gets a backing to stay readable.
    @Entry var onWallpaper = false
}

extension View {
    /// A pill behind text that sits on the timeline, when there's a wallpaper under it.
    func wallpaperBacked() -> some View { modifier(WallpaperBacking()) }
}

private struct WallpaperBacking: ViewModifier {
    @Environment(\.onWallpaper) private var onWallpaper

    func body(content: Content) -> some View {
        if onWallpaper {
            content.padding(.horizontal, 10).padding(.vertical, 4).background(.regularMaterial, in: Capsule())
        } else {
            content
        }
    }
}

/// A chat's wallpaper behind its timeline; dimmed in dark mode, as Signal does.
struct Wallpaper: View {
    let wallpaper: ChatColor
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        Rectangle().fill(ChatColors.fill(wallpaper))
            .overlay(Color.black.opacity(scheme == .dark ? 0.4 : 0))
            .ignoresSafeArea()
    }
}

/// Chat colours as SwiftUI draws them.
enum ChatColors {
    /// 0xAARRGGBB.
    static func color(_ argb: UInt32) -> Color {
        Color(
            .sRGB, red: Double((argb >> 16) & 0xFF) / 255, green: Double((argb >> 8) & 0xFF) / 255,
            blue: Double(argb & 0xFF) / 255, opacity: Double((argb >> 24) & 0xFF) / 255)
    }

    /// A gradient's start and end for Signal's angle: 0° runs bottom to top, 90° left to right.
    static func points(angle: UInt32) -> (start: UnitPoint, end: UnitPoint) {
        let radians = Double(angle) * .pi / 180
        let (dx, dy) = (sin(radians) / 2, -cos(radians) / 2)
        return (UnitPoint(x: 0.5 - dx, y: 0.5 - dy), UnitPoint(x: 0.5 + dx, y: 0.5 + dy))
    }

    /// An outgoing bubble's fill.
    static func style(_ chat: ChatColor?) -> AnyShapeStyle {
        guard let chat, !chat.colors.isEmpty else { return AnyShapeStyle(Theme.outgoing) }
        return fill(chat)
    }

    /// A colour or gradient as a fill (a bubble's, a wallpaper's).
    static func fill(_ c: ChatColor) -> AnyShapeStyle {
        guard c.colors.count > 1 else { return AnyShapeStyle(color(c.colors.first ?? 0)) }
        let (start, end) = points(angle: c.angle)
        return AnyShapeStyle(LinearGradient(colors: c.colors.map(color), startPoint: start, endPoint: end))
    }
}

/// A chat's colour and wallpaper as swatches; a click sets one (or puts it back to the default).
struct ChatStylePicker: View {
    let chatColor: ChatColor?
    let wallpaper: ChatColor?
    let chooseColor: (ChatColor?) -> Void
    let chooseWallpaper: (ChatColor?) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            section("Chat colour", presets: chatColorPresets(), current: chatColor, choose: chooseColor)
            Divider()
            section("Wallpaper", presets: wallpaperPresets(), current: wallpaper, choose: chooseWallpaper)
            Text("For this chat, on this Mac.").font(.caption).foregroundStyle(.secondary)
        }
        .padding(14)
    }

    private func section(
        _ title: String, presets: [ChatColor], current: ChatColor?, choose: @escaping (ChatColor?) -> Void
    ) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text(title).font(.headline)
                Spacer()
                Button("Use Default") { choose(nil) }.controlSize(.small)
            }
            LazyVGrid(columns: Array(repeating: GridItem(.fixed(30), spacing: 10), count: 6), spacing: 10) {
                ForEach(Array(presets.enumerated()), id: \.offset) { i, c in
                    let label = "\(title) \(i + 1)"
                    Button {
                        choose(c)
                    } label: {
                        Circle().fill(ChatColors.fill(c)).frame(width: 30, height: 30)
                            .overlay(Circle().stroke(.primary, lineWidth: c == current ? 2 : 0).padding(-3))
                    }
                    .buttonStyle(.plain)
                    .help(label)
                    .accessibilityLabel(label)
                    .accessibilityAddTraits(c == current ? .isSelected : [])
                }
            }
        }
    }
}
