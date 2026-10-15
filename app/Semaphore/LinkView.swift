import SwiftUI

struct LinkView: View {
    let url: String?
    let error: String?
    let retry: () -> Void

    var body: some View {
        VStack(spacing: 18) {
            Text("Link Semaphore to your phone").font(.title2.bold())
            Text("In Signal on your phone: Settings → Linked devices → Link new device, then scan this code.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 360)
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
            if let error {
                Text(error).foregroundStyle(.red).font(.callout).multilineTextAlignment(.center)
                    .frame(maxWidth: 360)
                Button("Try again", action: retry).keyboardShortcut(.defaultAction)
            }
        }
        .padding(40)
    }
}
