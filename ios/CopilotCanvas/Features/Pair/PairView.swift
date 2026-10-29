import AVFoundation
import SwiftUI

/// Pairing with a service: scan the QR code `copilot-canvas remote pair --qr`
/// shows on the Cloud PC, or paste its link / type the code.
struct PairView: View {
    var initial: PairLinkInfo?
    var onPaired: (PairedServer) -> Void

    @State private var linkText = ""
    @State private var address = ""
    @State private var code = ""
    @State private var token = ""
    @State private var showScanner = false
    @State private var busy = false
    @State private var error: String?
    @State private var manual = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) {
                header
                stepCard
                Button {
                    showScanner = true
                } label: {
                    Label("Scan the pairing QR code", systemImage: "qrcode.viewfinder")
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 6)
                }
                .buttonStyle(.borderedProminent)
                .disabled(busy)

                VStack(alignment: .leading, spacing: 8) {
                    Text("Or paste the pairing link").font(.subheadline.weight(.semibold))
                    TextField("ccanvas://pair?…", text: $linkText, axis: .vertical)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.system(.footnote, design: .monospaced))
                        .padding(10)
                        .background(Palette.panel, in: RoundedRectangle(cornerRadius: 8))
                    Button("Pair with this link") { pairLink(linkText) }
                        .disabled(linkText.trimmingCharacters(in: .whitespaces).isEmpty || busy)
                }

                DisclosureGroup("Enter the address and code by hand", isExpanded: $manual) {
                    VStack(alignment: .leading, spacing: 10) {
                        field("Address", "wss://copilot-canvas-47600.usw2.devtunnels.ms", $address)
                        field("Pairing code", "ABCD-EFGH-JKMN", $code)
                        field("Tunnel access token (optional)", "eyJ…", $token)
                        Button("Pair") {
                            let link = PairLinkInfo(name: "cloudpc", url: address.trimmingCharacters(in: .whitespaces), code: code, token: token.trimmingCharacters(in: .whitespacesAndNewlines), tokenExpiresMs: tokenExpiryMs(token: token))
                            pair(link)
                        }
                        .disabled(address.isEmpty || code.isEmpty || busy)
                    }
                    .padding(.top, 8)
                }
                .tint(Palette.text)

                if busy {
                    HStack(spacing: 10) {
                        ProgressView()
                        Text("Pairing… the service proves it knows the code first.").font(.footnote).foregroundStyle(.secondary)
                    }
                }
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.footnote)
                        .foregroundStyle(Palette.failed)
                }
            }
            .padding(20)
        }
        .background(Palette.background)
        .sheet(isPresented: $showScanner) {
            QRScannerSheet { text in
                showScanner = false
                pairLink(text)
            }
        }
        .onAppear {
            if let initial { pair(initial) }
            // `-pairLink URI` (simulator runs, tests).
            let args = ProcessInfo.processInfo.arguments
            if initial == nil, let index = args.firstIndex(of: "-pairLink"), index + 1 < args.count {
                pairLink(args[index + 1])
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            Image(systemName: "square.grid.3x3.square").font(.system(size: 40)).foregroundStyle(Palette.accent)
            Text("Copilot Canvas").font(.largeTitle.weight(.bold))
            Text("Your agent sessions on the Cloud PC, on the go.").foregroundStyle(.secondary)
        }
    }

    private var stepCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("On the Cloud PC, run:").font(.subheadline.weight(.semibold))
            Text("copilot-canvas remote pair --qr --name iphone")
                .font(.system(.footnote, design: .monospaced))
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Palette.background, in: RoundedRectangle(cornerRadius: 6))
                .textSelection(.enabled)
            Text("It shows a QR code with a one-time code (10 minutes) and a Dev Tunnels access token. Pair only your own phone: it can do what you can on the Cloud PC.")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(Palette.panel, in: RoundedRectangle(cornerRadius: 12))
    }

    private func field(_ title: String, _ placeholder: String, _ text: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            TextField(placeholder, text: text)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.system(.footnote, design: .monospaced))
                .padding(8)
                .background(Palette.panel, in: RoundedRectangle(cornerRadius: 6))
        }
    }

    private func pairLink(_ text: String) {
        do {
            pair(try parsePairLink(uri: text.trimmingCharacters(in: .whitespacesAndNewlines)))
        } catch {
            self.error = describe(error)
        }
    }

    private func pair(_ link: PairLinkInfo) {
        guard !busy else { return }
        busy = true
        error = nil
        Task {
            do {
                let server = try await Pairer.pair(link: link)
                busy = false
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                onPaired(server)
            } catch {
                busy = false
                self.error = describe(error)
                UINotificationFeedbackGenerator().notificationOccurred(.error)
            }
        }
    }
}

func describe(_ error: Error) -> String {
    if case let CoreError.Failed(message) = error { return message }
    return error.localizedDescription
}

/// The camera, reading QR codes (AVFoundation: every iPhone).
struct QRScannerSheet: View {
    var found: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ZStack {
                Color.black.ignoresSafeArea()
                if AVCaptureDevice.default(for: .video) == nil {
                    Text("No camera here: paste the pairing link instead.").foregroundStyle(.secondary).padding()
                } else {
                    QRScanner(found: found).ignoresSafeArea()
                    RoundedRectangle(cornerRadius: 16).stroke(Palette.accent, lineWidth: 3).frame(width: 260, height: 260)
                }
            }
            .navigationTitle("Scan the pairing code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        }
    }
}

private struct QRScanner: UIViewControllerRepresentable {
    var found: (String) -> Void

    func makeUIViewController(context: Context) -> ScannerController {
        let controller = ScannerController()
        controller.found = found
        return controller
    }

    func updateUIViewController(_ controller: ScannerController, context: Context) {}

    final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
        var found: ((String) -> Void)?
        private let session = AVCaptureSession()
        private var preview: AVCaptureVideoPreviewLayer?
        private var done = false

        override func viewDidLoad() {
            super.viewDidLoad()
            AVCaptureDevice.requestAccess(for: .video) { granted in
                guard granted else { return }
                DispatchQueue.main.async { self.setUp() }
            }
        }

        private func setUp() {
            guard let device = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: device), session.canAddInput(input) else { return }
            session.addInput(input)
            let output = AVCaptureMetadataOutput()
            guard session.canAddOutput(output) else { return }
            session.addOutput(output)
            output.setMetadataObjectsDelegate(self, queue: .main)
            output.metadataObjectTypes = [.qr]
            let preview = AVCaptureVideoPreviewLayer(session: session)
            preview.videoGravity = .resizeAspectFill
            preview.frame = view.bounds
            view.layer.addSublayer(preview)
            self.preview = preview
            DispatchQueue.global(qos: .userInitiated).async { self.session.startRunning() }
        }

        override func viewDidLayoutSubviews() {
            super.viewDidLayoutSubviews()
            preview?.frame = view.bounds
        }

        override func viewWillDisappear(_ animated: Bool) {
            super.viewWillDisappear(animated)
            session.stopRunning()
        }

        func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
            guard !done, let code = metadataObjects.compactMap({ $0 as? AVMetadataMachineReadableCodeObject }).first?.stringValue, code.hasPrefix("ccanvas://") else { return }
            done = true
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
            session.stopRunning()
            found?(code)
        }
    }
}
