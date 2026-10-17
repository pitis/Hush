import SwiftUI

/// Colors after Signal Desktop's dark theme.
enum Theme {
    static let window = Color(red: 0x1B / 255, green: 0x1B / 255, blue: 0x1D / 255)
    static let sidebar = Color(red: 0x1F / 255, green: 0x1F / 255, blue: 0x21 / 255)
    static let selection = Color(red: 0x2C / 255, green: 0x2C / 255, blue: 0x2E / 255)
    static let outgoing = Color(red: 0x2C / 255, green: 0x6B / 255, blue: 0xED / 255)
    static let incoming = Color(red: 0x3B / 255, green: 0x3B / 255, blue: 0x3D / 255)
    static let field = Color(red: 0x2C / 255, green: 0x2C / 255, blue: 0x2E / 255)
    static let secondaryText = Color(white: 0.62)
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
