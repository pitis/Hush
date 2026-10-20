import SwiftUI

/// The installed sticker packs, a tab each; a sticker sends at a click. A pack is added by its
/// link (or on the phone).
struct StickerPicker: View {
    let store: AppStore
    let dismiss: () -> Void
    @State private var packId: String?
    @State private var link = ""
    @State private var adding = false
    @State private var addError: String?

    private var pack: StickerPackInfo? {
        store.stickerPacks.first { $0.id == packId } ?? store.stickerPacks.first
    }

    var body: some View {
        VStack(spacing: 0) {
            if store.stickerPacks.isEmpty {
                Text("No sticker packs yet. Packs added on your phone show here, or paste a pack's link below.")
                    .font(.system(size: 12)).foregroundStyle(.secondary).multilineTextAlignment(.center)
                    .frame(maxWidth: .infinity, maxHeight: .infinity).padding(20)
            } else {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 4) {
                        ForEach(store.stickerPacks, id: \.id) { p in
                            Button {
                                packId = p.id
                            } label: {
                                ThumbnailImage(path: p.stickers[0].path, shownAt: 28).frame(width: 28, height: 28)
                                    .padding(4)
                                    .background(
                                        p.id == pack?.id ? Theme.selection : .clear,
                                        in: RoundedRectangle(cornerRadius: 6))
                            }
                            .buttonStyle(.plain).help(p.title).accessibilityLabel(p.title)
                        }
                    }
                    .padding(8)
                }
                Divider()
                if let pack {
                    ScrollView {
                        LazyVGrid(columns: Array(repeating: GridItem(.fixed(72), spacing: 8), count: 4), spacing: 8) {
                            ForEach(pack.stickers, id: \.id) { s in
                                Button {
                                    dismiss()
                                    Task { await store.sendSticker(pack: pack.id, sticker: s.id) }
                                } label: {
                                    ThumbnailImage(path: s.path, shownAt: 72).frame(width: 72, height: 72)
                                }
                                .buttonStyle(.plain)
                                .help(s.emoji ?? "")
                                .accessibilityLabel(s.emoji.map { "Sticker \($0)" } ?? "Sticker")
                            }
                        }
                        .padding(10)
                    }
                }
            }
            Divider()
            HStack(spacing: 6) {
                TextField("Sticker pack link", text: $link).textFieldStyle(.roundedBorder)
                    .onSubmit(add)
                Button("Add", action: add).disabled(link.trimmingCharacters(in: .whitespaces).isEmpty || adding)
            }
            .padding(8)
            if let addError {
                Text(addError).font(.caption).foregroundStyle(.orange).padding([.horizontal, .bottom], 8)
            }
        }
        .frame(width: 340, height: 360)
        .task { await store.loadStickerPacks() }
    }

    private func add() {
        let typed = link
        let shown = Set(store.stickerPacks.map(\.id))
        adding = true
        addError = nil
        Task {
            let failed = await store.addStickerPack(typed)
            adding = false
            if let failed {
                addError = failed
            } else {
                link = ""
                // show the pack just added (none new: it was installed already)
                packId = store.stickerPacks.first { !shown.contains($0.id) }?.id ?? packId
            }
        }
    }
}
