import SwiftUI

extension EnvironmentValues {
    /// Our bubbles' colour in the chat shown (None: the app's).
    @Entry var chatColor: ChatColor? = nil
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
        guard let chat, let first = chat.colors.first else { return AnyShapeStyle(Theme.outgoing) }
        if chat.colors.count == 1 { return AnyShapeStyle(color(first)) }
        let (start, end) = points(angle: chat.angle)
        return AnyShapeStyle(LinearGradient(colors: chat.colors.map(color), startPoint: start, endPoint: end))
    }
}

/// The presets as swatches; a click sets the chat's colour (or back to the default).
struct ChatColorPicker: View {
    let current: ChatColor?
    let choose: (ChatColor?) -> Void
    private let presets = chatColorPresets()

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Chat colour").font(.headline)
            LazyVGrid(columns: Array(repeating: GridItem(.fixed(30), spacing: 10), count: 6), spacing: 10) {
                ForEach(Array(presets.enumerated()), id: \.offset) { i, c in swatch(c, label: "Colour \(i + 1)") }
            }
            HStack {
                Text("Your bubbles in this chat, on this Mac.").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Use Default") { choose(nil) }.controlSize(.small)
            }
        }
        .padding(14)
    }

    private func swatch(_ c: ChatColor, label: String) -> some View {
        Button {
            choose(c)
        } label: {
            Circle().fill(ChatColors.style(c)).frame(width: 30, height: 30)
                .overlay(Circle().stroke(.primary, lineWidth: c == current ? 2 : 0).padding(-3))
        }
        .buttonStyle(.plain)
        .help(label)
        .accessibilityLabel(label)
        .accessibilityAddTraits(c == current ? .isSelected : [])
    }
}
