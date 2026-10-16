import SwiftUI

/// The history transfer's progress, shown over the chats, with a way out at every stage.
struct HistoryView: View {
    let progress: AppStore.HistoryProgress
    let cancel: () -> Void
    let retry: () -> Void

    private var title: String {
        switch progress.stage {
        case .waiting: "Preparing to transfer…"
        case .downloading: "Transferring your messages…"
        case .importing: "Importing your messages…"
        case .failed: "The transfer didn't finish"
        case .done, .noHistory: ""
        }
    }

    private var detail: String? {
        switch progress.stage {
        case .waiting:
            "Keep Signal open on your phone."
        case .downloading where progress.total > 0:
            String(
                format: "%.0f of %.0f MB", Double(progress.done) / 1_048_576, Double(progress.total) / 1_048_576)
        case .importing where progress.done > 0:
            "\(progress.done.formatted()) items"
        case .failed:
            "Your chats keep working; new messages arrive once you continue."
        default:
            nil
        }
    }

    var body: some View {
        VStack(spacing: 14) {
            if progress.stage == .failed {
                Image(systemName: "exclamationmark.triangle").font(.largeTitle).foregroundStyle(.secondary)
            } else if progress.stage == .downloading, progress.total > 0 {
                ProgressView(value: Double(progress.done), total: Double(progress.total))
                    .frame(width: 220)
            } else {
                ProgressView()
            }
            Text(title).font(.headline)
            if let detail { Text(detail).font(.callout).foregroundStyle(.secondary) }
            if progress.stage == .failed {
                HStack {
                    Button("Continue Without History", action: cancel)
                    Button("Retry", action: retry).buttonStyle(.borderedProminent)
                }
            } else {
                Button("Continue Without History", action: cancel)
            }
        }
        .padding(28)
        .frame(maxWidth: 380)
        .background(Theme.window, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(.separator))
    }
}
