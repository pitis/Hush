import SwiftUI

/// What a click on a poll option votes, and how a poll's votes read (Desktop's words).
enum PollChoice {
    /// Our options after clicking `index`: in a single-choice poll the click is the choice (on
    /// our choice it takes the vote back); with several, it toggles that option.
    static func after(clicking index: UInt32, in poll: Poll, me: String) -> [UInt32] {
        let mine = poll.options.indices.filter { poll.options[$0].voters.contains(me) }.map { UInt32($0) }
        if poll.allowMultiple {
            return mine.contains(index) ? mine.filter { $0 != index } : (mine + [index]).sorted()
        }
        return mine == [index] ? [] : [index]
    }

    /// "3 votes", "No votes · Ended": each voter once.
    static func summary(_ poll: Poll) -> String {
        let voters = Set(poll.options.flatMap(\.voters)).count
        let votes = voters == 0 ? "No votes" : voters == 1 ? "1 vote" : "\(voters) votes"
        return poll.ended ? "\(votes) · Ended" : votes
    }
}

/// A poll in its bubble: each option with its share of the votes, ours ticked; a click votes.
struct PollCard: View {
    let question: String?
    let poll: Poll
    let me: String?
    let mine: Bool
    let vote: ([UInt32]) -> Void
    let end: () -> Void

    var body: some View {
        let total = max(1, Set(poll.options.flatMap(\.voters)).count)
        VStack(alignment: .leading, spacing: 8) {
            Label(question ?? "Poll", systemImage: "chart.bar.fill").font(.system(size: 14, weight: .semibold))
            Text(poll.allowMultiple ? "Choose one or more" : "Choose one").font(.system(size: 11))
                .foregroundStyle(.secondary)
            ForEach(Array(poll.options.enumerated()), id: \.offset) { i, option in
                let chosen = me.map { option.voters.contains($0) } ?? false
                Button {
                    if let me { vote(PollChoice.after(clicking: UInt32(i), in: poll, me: me)) }
                } label: {
                    VStack(alignment: .leading, spacing: 3) {
                        HStack {
                            Image(systemName: chosen ? "checkmark.circle.fill" : "circle")
                            Text(option.text).font(.system(size: 13))
                            Spacer()
                            Text("\(option.voters.count)").font(.system(size: 12)).monospacedDigit()
                        }
                        GeometryReader { g in
                            Capsule().fill(.quaternary)
                                .overlay(alignment: .leading) {
                                    Capsule().fill(.secondary)
                                        .frame(width: g.size.width * CGFloat(option.voters.count) / CGFloat(total))
                                }
                        }
                        .frame(height: 4)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .disabled(poll.ended || me == nil)
                .accessibilityLabel(
                    "\(option.text), \(option.voters.count == 1 ? "1 vote" : "\(option.voters.count) votes")\(chosen ? ", your choice" : "")"
                )
            }
            HStack {
                Text(PollChoice.summary(poll)).font(.system(size: 11)).foregroundStyle(.secondary)
                Spacer()
                if mine && !poll.ended {
                    Button("End poll", action: end).font(.system(size: 11)).buttonStyle(.bordered)
                }
            }
        }
        .frame(minWidth: 220, maxWidth: 320)
    }
}
