import SwiftUI

/// The look the app takes: the system's, or always light or dark (Settings).
enum Appearance: String, CaseIterable, Identifiable {
    case system, light, dark

    static let key = "appearance"
    /// As before there was a choice: the app was dark only.
    static let `default` = Appearance.dark

    var id: String { rawValue }
    var label: String { rawValue.prefix(1).uppercased() + rawValue.dropFirst() }
    var scheme: ColorScheme? {
        switch self {
        case .system: nil
        case .light: .light
        case .dark: .dark
        }
    }
}

/// Colors after Signal Desktop's light and dark themes, following the window's appearance.
enum Theme {
    static let window = adaptive(light: 0xFFFFFF, dark: 0x1B1B1D)
    static let sidebar = adaptive(light: 0xF6F6F6, dark: 0x1F1F21)
    static let selection = adaptive(light: 0xE3E3E3, dark: 0x2C2C2E)
    static let outgoing = Color(red: 0x2C / 255, green: 0x6B / 255, blue: 0xED / 255)
    static let incoming = adaptive(light: 0xE9E9E9, dark: 0x3B3B3D)
    /// The words in an incoming bubble (an outgoing one's are white).
    static let incomingText = adaptive(light: 0x1B1B1D, dark: 0xFFFFFF)
    static let field = adaptive(light: 0xF0F0F0, dark: 0x2C2C2E)
    static let secondaryText = adaptive(light: 0x6B6B6B, dark: 0x9E9E9E)

    /// A colour with a light and a dark value (RGB hex), picked by the drawing appearance.
    private static func adaptive(light: UInt32, dark: UInt32) -> Color {
        let rgb = { (hex: UInt32) in
            NSColor(
                srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
                blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
        }
        return Color(
            nsColor: NSColor(name: nil) { appearance in
                appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? rgb(dark) : rgb(light)
            })
    }
    static let avatars: [Color] = [
        Color(red: 0.86, green: 0.36, blue: 0.36), Color(red: 0.89, green: 0.55, blue: 0.27),
        Color(red: 0.80, green: 0.66, blue: 0.20), Color(red: 0.36, green: 0.68, blue: 0.42),
        Color(red: 0.25, green: 0.62, blue: 0.68), Color(red: 0.32, green: 0.49, blue: 0.85),
        Color(red: 0.56, green: 0.42, blue: 0.84), Color(red: 0.80, green: 0.40, blue: 0.66),
    ]
    static let bubbleRadius: CGFloat = 18
}

/// The chats' pictures by chat id (`Conversation.avatar`), for every `Avatar` to find.
@MainActor @Observable
final class AvatarPictures {
    static let shared = AvatarPictures()
    var paths: [String: String] = [:]
}

struct Avatar: View {
    let id: String
    let name: String
    var size: CGFloat = 44

    var body: some View {
        if let path = AvatarPictures.shared.paths[id] {
            ThumbnailImage(path: path).frame(width: size, height: size).clipShape(Circle())
                .accessibilityHidden(true)
        } else {
            initials
        }
    }

    private var initials: some View {
        Circle()
            .fill(Theme.avatars[Formatting.avatarIndex(id)].gradient)
            .overlay(
                Text(Formatting.initials(name)).font(.system(size: size * 0.38, weight: .semibold))
                    .foregroundStyle(.white)
            )
            .frame(width: size, height: size)
            .accessibilityHidden(true)
    }
}
