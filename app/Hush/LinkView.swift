import SwiftUI

struct LinkView: View {
    let url: String?
    let error: String?
    /// How restoring a backup went, if one was just restored.
    let note: String?
    let retry: () -> Void
    let restore: (URL, String) async -> String?

    var body: some View {
        VStack(spacing: 18) {
            Text("Link Hush to your phone").font(.title2.bold())
            Text("In Signal on your phone: Settings → Linked devices → Link new device, then scan this code.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 360)
            Text("If your phone offers to transfer your message history, accept it to see your older messages here.")
                .font(.callout)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 360)
            if let note {
                Text(note).font(.callout.bold()).multilineTextAlignment(.center).frame(maxWidth: 360)
            }
            Group {
                if let url, let image = QRCode.image(for: url) {
                    Image(nsImage: image).interpolation(.none).resizable()
                        .accessibilityLabel("Linking QR code")
                } else if error != nil {
                    Image(systemName: "exclamationmark.triangle").font(.largeTitle).foregroundStyle(.secondary)
                } else {
                    ProgressView()
                }
            }
            .frame(width: 280, height: 280)
            if let error, url == nil {
                Text(error).foregroundStyle(.red).font(.callout).multilineTextAlignment(.center)
                    .frame(maxWidth: 360)
                Button("Try again", action: retry).keyboardShortcut(.defaultAction)
            }
            RestoreButton(restore: restore)
        }
        .padding(40)
    }
}
