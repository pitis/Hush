import SwiftUI

/// The one call's window: who, the state or duration, mute, device pickers, hang up.
struct CallWindow: View {
    let store: AppStore

    var body: some View {
        Group {
            if let call = store.call {
                CallView(store: store, call: call)
            } else {
                ContentUnavailableView("No call", systemImage: "phone.down")
            }
        }
        .frame(width: 320, height: 300)
        .background(Theme.window)
        // closing the window mid-call ends the call: a running call with no window would be
        // a call you can't hang up
        .onDisappear { if let call = store.call, call.phase != .ended { store.hangupCall() } }
    }
}

private struct CallView: View {
    let store: AppStore
    let call: AppStore.ActiveCall
    @State private var devices: AudioDevices?
    /// Ticks the duration once a second.
    @State private var now = Date()
    private let clock = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    /// An incoming call can be taken only once RingRTC rings it locally (`.ringing`); until
    /// then Accept waits (the core holds an early accept and applies it when it rings).
    private var answerable: Bool { call.incoming && (call.phase == .incoming || call.phase == .ringing) }

    private var stateLine: String {
        switch call.phase {
        case .incoming: "Incoming voice call…"
        case .outgoing: "Calling…"
        case .ringing: call.incoming ? "Incoming voice call" : "Ringing…"
        case .connecting: "Connecting…"
        case .connected: duration
        case .ended: "Call over — \(Self.wording(call.endedReason))"
        }
    }

    static func wording(_ reason: CallEnd?) -> String {
        switch reason {
        case .hungUp, .remoteHungUp, nil: "hung up"
        case .answeredElsewhere: "answered on your phone"
        case .declinedElsewhere: "declined on your phone"
        case .busy: "busy"
        case .noAnswer: "no answer"
        case .failed: "the call failed"
        }
    }

    private var duration: String {
        guard let connectedAt = call.connectedAt else { return "Connected" }
        let seconds = max(0, Int(now.timeIntervalSince(connectedAt)))
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }

    var body: some View {
        VStack(spacing: 14) {
            Avatar(id: call.conversationId, name: call.title, size: 64)
            Text(call.title).font(.title3.bold()).lineLimit(1)
            Text(stateLine).font(.callout).foregroundStyle(Theme.secondaryText).monospacedDigit()
            Spacer(minLength: 0)
            if answerable {
                HStack(spacing: 24) {
                    Button {
                        store.hangupCall()
                    } label: {
                        Label("Decline", systemImage: "phone.down.fill").frame(maxWidth: .infinity)
                    }
                    .tint(.red)
                    Button {
                        store.acceptCall()
                    } label: {
                        Label("Accept", systemImage: "phone.fill").frame(maxWidth: .infinity)
                    }
                    .tint(.green)
                    .keyboardShortcut(.defaultAction)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
            } else if call.phase == .ended {
                Button("Close") { store.dismissEndedCall() }.keyboardShortcut(.defaultAction)
            } else {
                HStack(spacing: 18) {
                    Button {
                        store.toggleCallMuted()
                    } label: {
                        Image(systemName: call.muted ? "mic.slash.fill" : "mic.fill")
                            .font(.system(size: 17))
                            .frame(width: 28, height: 28)
                    }
                    .buttonStyle(.bordered)
                    .tint(call.muted ? .yellow : nil)
                    .accessibilityLabel(call.muted ? "Unmute" : "Mute")
                    DeviceMenu(
                        icon: "speaker.wave.2.fill", label: "Speaker",
                        devices: devices?.outputs ?? [], current: devices?.currentOutput
                    ) { id in
                        store.setCallAudioOutput(id)
                    }
                    DeviceMenu(
                        icon: "mic", label: "Microphone",
                        devices: devices?.inputs ?? [], current: devices?.currentInput
                    ) { id in
                        store.setCallAudioInput(id)
                    }
                    Button {
                        store.hangupCall()
                    } label: {
                        Image(systemName: "phone.down.fill").font(.system(size: 17)).frame(width: 28, height: 28)
                    }
                    .buttonStyle(.borderedProminent)
                    .tint(.red)
                    .accessibilityLabel("Hang up")
                }
            }
        }
        .padding(20)
        .task { devices = await store.callAudioDevices() }
        .onReceive(clock) { now = $0 }
    }
}

/// A picker over the call's microphones or speakers.
private struct DeviceMenu: View {
    let icon: String
    let label: String
    let devices: [AudioDevice]
    let current: String?
    let select: (String) -> Void

    var body: some View {
        Menu {
            ForEach(devices, id: \.id) { device in
                Button {
                    select(device.id)
                } label: {
                    if device.id == current {
                        Label(device.name, systemImage: "checkmark")
                    } else {
                        Text(device.name)
                    }
                }
            }
        } label: {
            Image(systemName: icon).font(.system(size: 15)).frame(width: 24, height: 24)
        }
        .menuStyle(.borderlessButton)
        .frame(width: 36)
        .accessibilityLabel(label)
    }
}
