import SwiftUI

/// Our safety number with someone, to compare with the one their phone shows: the QR code their
/// phone scans, and 60 digits in 12 groups of 5.
struct SafetyNumberView: View {
    let store: AppStore
    let aci: String
    let name: String
    @State private var read: AppStore.SafetyRead?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 16) {
            Text("Safety Number with \(name)").font(.title3.bold()).multilineTextAlignment(.center)
            switch read {
            case nil:
                ProgressView().frame(width: 200, height: 200)
            case .known(let number):
                if let image = QRCode.image(for: number.scannable) {
                    Image(nsImage: image).interpolation(.none).resizable()
                        .frame(width: 200, height: 200)
                        .accessibilityLabel("Safety number code")
                }
                Text(Self.formatted(number.digits))
                    .font(.system(size: 17, design: .monospaced))
                    .lineSpacing(4)
                    .fixedSize()
                    .textSelection(.enabled)
                Text(
                    "To check that your chat with \(name) is end-to-end encrypted, compare these numbers with "
                        + "the ones their phone shows, or let it scan this code."
                )
                .font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            case .unknown:
                Text("There's no safety number with \(name) yet: there is one once you've exchanged messages.")
                    .foregroundStyle(.secondary).multilineTextAlignment(.center)
            case .failed(let message):
                Text(message).foregroundStyle(.red).multilineTextAlignment(.center)
            }
            Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
        }
        .padding(24)
        .frame(width: 360)
        .task { read = await store.safetyNumber(aci) }
    }

    /// As Signal's apps show it: 3 rows of 4 groups of 5 digits.
    nonisolated static func formatted(_ digits: String) -> String {
        let groups = stride(from: 0, to: digits.count, by: 5).map { String(digits.dropFirst($0).prefix(5)) }
        return stride(from: 0, to: groups.count, by: 4).map {
            groups[$0..<min($0 + 4, groups.count)].joined(separator: " ")
        }
        .joined(separator: "\n")
    }
}
