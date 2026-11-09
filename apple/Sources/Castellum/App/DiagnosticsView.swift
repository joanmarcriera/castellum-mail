// Castellum - Diagnostics window: local sync health + diagnostics gathering.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Architecture boundary: `LocalDiagnostics` (real email/path) is shown in this
// window ONLY; the "Gather Diagnostics…" button always uses the REDACTED
// `castellum_support_bundle` path, plus crash-report/app-log/Rust-core-log
// text passed through `castellum_redact_diagnostic_text` before it ever
// touches disk. The two must never be mixed. The "Performance" section and
// the exported `sync-kpi.txt` are a third, separate source
// (`castellum_last_sync_kpi`) that is already an allowlisted numbers-only
// shape like the support bundle — no redaction pass, but also no real
// email/path in it, so it is safe on both sides of the boundary.

import AppKit
import Foundation
import SwiftUI

// MARK: - View Model

@MainActor
final class DiagnosticsViewModel: ObservableObject {
    enum LoadState {
        case loading
        case loaded(LocalDiagnostics)
        case empty             // profile has no accounts
        case unavailable(String)
    }

    enum IntegrityState {
        case idle
        case running
        case ok
        case failed(Int, String?)  // (issueCount, firstIssue)
        case error(String)
    }

    enum ExportState {
        case idle
        case gathering
        case readyToSave(GatheredDiagnostics)
        case saving
        case saved(URL)
        case error(String)
    }

    /// Last-run sync performance, shown in the "Performance" section
    /// unconditionally — no button click needed, complementary to (not a
    /// replacement for) `AccountHeaderRow`'s LIVE `syncStatus` display of a
    /// sync currently in progress. `.none` is the normal, expected state for
    /// a profile that has never completed a sync (see `CastellumCore.lastSyncKpi()`);
    /// only a real fetch failure produces `.unavailable`.
    enum KpiState {
        case loading
        case none
        case loaded(LastSyncKpi)
        case unavailable(String)
    }

    @Published private(set) var loadState: LoadState = .loading
    @Published private(set) var integrityState: IntegrityState = .idle
    @Published private(set) var exportState: ExportState = .idle
    @Published private(set) var kpiState: KpiState = .loading

    // Injected FFI closures — replaced in tests with fakes.
    var fetchLocalDiagnostics: () async throws -> LocalDiagnostics
    var fetchIntegrityCheck: () async throws -> IntegrityReport
    var fetchSupportBundle: () async throws -> SupportBundle
    var fetchLastSyncKpi: () async throws -> LastSyncKpi?
    var fetchCrashReports: () -> [CrashReportFile]
    var fetchAppLog: () async -> String
    /// Directory the Rust core's structured log files live in, or `nil`.
    /// Synchronous like `fetchCrashReports` (a quick local stat/read, not
    /// worth a Task.detached of its own) — the potentially-larger file READ
    /// happens in `DiagnosticsBundleGatherer.recentRustLog`, called the same
    /// way `fetchCrashReports()`'s result is processed directly in `doGather()`.
    var fetchLogDirectory: () -> URL?
    var redactText: (String) throws -> String

    init(
        fetchLocalDiagnostics: @escaping () async throws -> LocalDiagnostics,
        fetchIntegrityCheck: @escaping () async throws -> IntegrityReport,
        fetchSupportBundle: @escaping () async throws -> SupportBundle,
        fetchLastSyncKpi: @escaping () async throws -> LastSyncKpi? = { nil },
        fetchCrashReports: @escaping () -> [CrashReportFile] = { DiagnosticsBundleGatherer.recentCrashReports() },
        fetchAppLog: @escaping () async -> String = {
            await Task.detached { DiagnosticsBundleGatherer.recentAppLog() }.value
        },
        fetchLogDirectory: @escaping () -> URL? = { nil },
        redactText: @escaping (String) throws -> String
    ) {
        self.fetchLocalDiagnostics = fetchLocalDiagnostics
        self.fetchIntegrityCheck = fetchIntegrityCheck
        self.fetchSupportBundle = fetchSupportBundle
        self.fetchLastSyncKpi = fetchLastSyncKpi
        self.fetchCrashReports = fetchCrashReports
        self.fetchAppLog = fetchAppLog
        self.fetchLogDirectory = fetchLogDirectory
        self.redactText = redactText
    }

    convenience init(model: AppModel) {
        let core = model.core
        self.init(
            fetchLocalDiagnostics: {
                try await Task.detached { try core.localDiagnostics() }.value
            },
            fetchIntegrityCheck: {
                try await Task.detached { try core.integrityCheck() }.value
            },
            fetchSupportBundle: {
                try await Task.detached { try core.supportBundle() }.value
            },
            fetchLastSyncKpi: {
                try await Task.detached { try core.lastSyncKpi() }.value
            },
            fetchLogDirectory: { core.logDirectory() },
            redactText: { text in try core.redactDiagnosticText(text) }
        )
    }

    func load() {
        Task { await doLoad() }
        Task { await doLoadKpi() }
    }

    private func doLoad() async {
        loadState = .loading
        do {
            let diag = try await fetchLocalDiagnostics()
            if diag.accounts.isEmpty {
                loadState = .empty
            } else {
                loadState = .loaded(diag)
            }
        } catch {
            loadState = .unavailable(error.localizedDescription)
        }
    }

    /// Loads the "Performance" section independently of `doLoad()`'s account
    /// list — it has its own failure mode (a real fetch error) distinct from
    /// "no sync yet", which is `.none`, not an error.
    private func doLoadKpi() async {
        kpiState = .loading
        do {
            if let kpi = try await fetchLastSyncKpi() {
                kpiState = .loaded(kpi)
            } else {
                kpiState = .none
            }
        } catch {
            kpiState = .unavailable(error.localizedDescription)
        }
    }

    func runIntegrityCheck() {
        guard case .idle = integrityState else { return }
        Task { await doIntegrityCheck() }
    }

    private func doIntegrityCheck() async {
        integrityState = .running
        do {
            let report = try await fetchIntegrityCheck()
            if report.ok {
                integrityState = .ok
            } else {
                integrityState = .failed(report.issueCount, report.firstIssue)
            }
        } catch {
            integrityState = .error(error.localizedDescription)
        }
    }

    func beginGather() {
        guard case .idle = exportState else { return }
        Task { await doGather() }
    }

    private func doGather() async {
        exportState = .gathering
        do {
            let bundle = try await fetchSupportBundle()
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            let data = try encoder.encode(bundle)
            guard let json = String(data: data, encoding: .utf8) else {
                exportState = .error("Could not encode support bundle as UTF-8.")
                return
            }

            let rawLog = await fetchAppLog()
            let redactedLog = (try? redactText(rawLog)) ?? "[app log unavailable]"

            let crashReports = fetchCrashReports().map { report -> CrashReportFile in
                let redacted = (try? redactText(report.contents)) ?? "[crash report unavailable]"
                return CrashReportFile(filename: report.filename, contents: redacted)
            }

            // Rust core log excerpt: a free-text file read straight off disk,
            // so — same rule as appLog/crashReports above, CLAUDE.md gotcha 5
            // — it MUST pass through redactText before it ever touches the
            // exported bundle. `recentRustLog` never throws; a missing/empty
            // directory just yields "".
            let logDirectory = fetchLogDirectory()
            let rawRustLog = DiagnosticsBundleGatherer.recentRustLog(in: logDirectory)
            let redactedRustLog = (try? redactText(rawRustLog)) ?? "[rust log unavailable]"

            // Last-sync KPI: already the allowlisted, numbers-only
            // `LastSyncKpi` shape (see its doc comment) — no text redaction
            // needed, only a fresh independent fetch (this gather must not
            // depend on the Performance section's `kpiState` having loaded
            // first, or on it still being current).
            let kpi: LastSyncKpi?
            do {
                kpi = try await fetchLastSyncKpi()
            } catch {
                kpi = nil
            }

            exportState = .readyToSave(GatheredDiagnostics(
                supportBundleJSON: json,
                environment: Self.environmentSummary(bundle: bundle),
                appLog: redactedLog,
                crashReports: crashReports,
                rustLog: redactedRustLog,
                syncKpiSummary: Self.kpiSummaryText(kpi)
            ))
        } catch {
            exportState = .error("Failed to gather diagnostics: \(error.localizedDescription)")
        }
    }

    /// Small free-text summary — app/build/OS version, schema/core version,
    /// generation timestamp — that has no home in `SupportBundle` (which
    /// carries `coreVersion`/`schemaVersion` but not the app's own marketing
    /// version or the OS version).
    static func environmentSummary(
        bundle: SupportBundle,
        appVersion: String = UpdateChecker.currentAppVersion(),
        buildNumber: String = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "?",
        osVersion: String = ProcessInfo.processInfo.operatingSystemVersionString,
        generatedAt: Date = Date()
    ) -> String {
        """
        Castellum \(appVersion) (\(buildNumber))
        macOS \(osVersion)
        Schema version: \(bundle.schemaVersion)
        Core version: \(bundle.coreVersion)
        Generated: \(Self.filenameFormatter.string(from: generatedAt)) UTC
        """
    }

    /// A small human-readable summary of the last-sync KPI snapshot, written
    /// as `sync-kpi.txt` in the exported bundle. Already the allowlisted,
    /// numbers-only `LastSyncKpi` shape (see its doc comment), so — unlike
    /// `appLog`/`crashReports`/the new Rust log excerpt — this needs no text
    /// redaction; it is built directly from typed fields, never from a raw
    /// string that could carry a mailbox path, address or error message.
    static func kpiSummaryText(_ kpi: LastSyncKpi?) -> String {
        guard let kpi else {
            return "No sync recorded yet in this profile.\n"
        }
        var lines = [
            "Last sync: \(Self.filenameFormatter.string(from: kpi.timestamp)) UTC",
            "Messages: \(kpi.messageCount)",
            "Wall time: \(String(format: "%.1f", kpi.wallMs)) ms",
        ]
        if let rate = kpi.messagesPerSecond {
            lines.append("Throughput: \(String(format: "%.1f", rate)) msgs/sec")
        }
        lines.append("IMAP round-trip time: \(String(format: "%.1f", kpi.totalMs(forPhasePrefix: "imap."))) ms")
        lines.append("DB write time: \(String(format: "%.1f", kpi.totalMs(forPhasePrefix: "db."))) ms")
        lines.append("")
        lines.append("Phases:")
        for (name, phase) in kpi.phases.sorted(by: { $0.key < $1.key }) {
            lines.append(
                "  \(name): count=\(phase.count) total=\(String(format: "%.1f", phase.totalMs))ms "
                    + "p50=\(String(format: "%.2f", phase.p50Ms))ms p95=\(String(format: "%.2f", phase.p95Ms))ms "
                    + "max=\(String(format: "%.2f", phase.maxMs))ms"
            )
        }
        return lines.joined(separator: "\n") + "\n"
    }

    /// Writes `castellum-diagnostics-<timestamp>/` inside `parent`: a
    /// support-bundle.json, an environment.txt, an app-log.txt, a
    /// rust-log.txt (tail excerpt of the Rust core's own structured log,
    /// already redacted), a sync-kpi.txt, and either a crash-reports/ folder
    /// (one already-redacted .txt file per report) or a crash-reports.txt
    /// note when none were found. Every file is written under the SAME new
    /// folder so "Reveal in Finder" (Task 6) can select one self-contained
    /// thing.
    func saveGatheredBundle(_ gathered: GatheredDiagnostics, toParentDirectory parent: URL) async {
        exportState = .saving
        let folderURL = parent.appendingPathComponent(Self.bundleFolderName(), isDirectory: true)
        do {
            let fm = FileManager.default
            try fm.createDirectory(at: folderURL, withIntermediateDirectories: true)
            try Data(gathered.supportBundleJSON.utf8)
                .write(to: folderURL.appendingPathComponent("support-bundle.json"), options: .atomic)
            try Data(gathered.environment.utf8)
                .write(to: folderURL.appendingPathComponent("environment.txt"), options: .atomic)
            try Data(gathered.appLog.utf8)
                .write(to: folderURL.appendingPathComponent("app-log.txt"), options: .atomic)
            try Data(gathered.rustLog.utf8)
                .write(to: folderURL.appendingPathComponent("rust-log.txt"), options: .atomic)
            try Data(gathered.syncKpiSummary.utf8)
                .write(to: folderURL.appendingPathComponent("sync-kpi.txt"), options: .atomic)
            if gathered.crashReports.isEmpty {
                let note = String(
                    localized: "No Castellum crash reports found in the last \(DiagnosticsBundleGatherer.maxCrashReportAgeDays) days.\n"
                )
                try Data(note.utf8)
                    .write(to: folderURL.appendingPathComponent("crash-reports.txt"), options: .atomic)
            } else {
                let crashDir = folderURL.appendingPathComponent("crash-reports", isDirectory: true)
                try fm.createDirectory(at: crashDir, withIntermediateDirectories: true)
                for report in gathered.crashReports {
                    let stem = (report.filename as NSString).deletingPathExtension
                    let safeName = stem.isEmpty ? report.filename + ".txt" : stem + ".txt"
                    try Data(report.contents.utf8)
                        .write(to: crashDir.appendingPathComponent(safeName), options: .atomic)
                }
            }
            exportState = .saved(folderURL)
        } catch {
            exportState = .error("Could not save diagnostics bundle: \(error.localizedDescription)")
        }
    }

    func resetExport() {
        exportState = .idle
    }

    func resetIntegrity() {
        integrityState = .idle
    }

    /// Safe folder name: `castellum-diagnostics-<yyyyMMdd-HHmmss>`.
    static func bundleFolderName(date: Date = Date()) -> String {
        "castellum-diagnostics-\(Self.filenameFormatter.string(from: date))"
    }

    private static let filenameFormatter: DateFormatter = {
        let fmt = DateFormatter()
        fmt.dateFormat = "yyyyMMdd-HHmmss"
        fmt.locale = Locale(identifier: "en_US_POSIX")
        fmt.timeZone = TimeZone(identifier: "UTC")
        return fmt
    }()
}

/// Everything gathered for one "Gather Diagnostics…" run, pre-redaction-applied
/// (app log and crash reports are already redacted by the time this exists —
/// see `DiagnosticsViewModel.doGather()`). The support bundle JSON needs no
/// further redaction: it is already the allowlisted `SupportBundle` shape.
struct GatheredDiagnostics: Equatable {
    let supportBundleJSON: String
    let environment: String
    let appLog: String
    let crashReports: [CrashReportFile]
    /// Tail excerpt of the Rust core's structured log file, already redacted
    /// — same rule as `appLog`/`crashReports` above.
    let rustLog: String
    /// Human-readable last-sync KPI summary (see `DiagnosticsViewModel.kpiSummaryText`).
    /// Built from the already-allowlisted `LastSyncKpi` shape, so unlike the
    /// other three fields here it needs no redaction.
    let syncKpiSummary: String
}

// MARK: - View

struct DiagnosticsView: View {
    @ObservedObject private var model: AppModel
    @StateObject private var vm: DiagnosticsViewModel
    @State private var hostWindow: NSWindow?
    @State private var selectedRow: String?

    init(model: AppModel) {
        _model = ObservedObject(wrappedValue: model)
        _vm = StateObject(wrappedValue: DiagnosticsViewModel(model: model))
    }

    var body: some View {
        VStack(spacing: 0) {
            mainContent
            Divider()
            bottomBar
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
        }
        .frame(minWidth: 580, minHeight: 360)
        .background(WindowAccessor(window: $hostWindow))
        .task { vm.load() }
        .onChange(of: exportReadyGathered) { gathered in
            guard let gathered else { return }
            presentDestinationPicker(gathered: gathered)
        }
    }

    /// Presented as a sheet attached to the Diagnostics window
    /// (window-modal). Cancel writes nothing and returns to idle. A directory
    /// picker — not a save panel — because this writes a whole folder, not
    /// one file.
    private func presentDestinationPicker(gathered: GatheredDiagnostics) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "Choose where to save the diagnostics folder."
        panel.directoryURL = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
        let complete: (NSApplication.ModalResponse) -> Void = { response in
            if response == .OK, let url = panel.urls.first {
                Task { await vm.saveGatheredBundle(gathered, toParentDirectory: url) }
            } else {
                vm.resetExport()
            }
        }
        if let window = hostWindow {
            panel.beginSheetModal(for: window, completionHandler: complete)
        } else {
            complete(panel.runModal())
        }
    }

    // MARK: - Main content

    @ViewBuilder
    private var mainContent: some View {
        switch vm.loadState {
        case .loading:
            loadingView
        case .loaded(let diag):
            loadedView(diag)
        case .empty:
            emptyView
        case .unavailable(let msg):
            unavailableView(msg)
        }
    }

    private var loadingView: some View {
        VStack(spacing: 12) {
            ProgressView()
            Text("Loading diagnostics…")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func loadedView(_ diag: LocalDiagnostics) -> some View {
        // Selectable rows: clicking highlights and ↑/↓ moves — the rows are
        // informational, but a dead-feeling list reads as broken.
        List(selection: $selectedRow) {
            PerformanceSection(state: vm.kpiState)
            ForEach(diag.accounts, id: \.accountId) { account in
                AccountSection(
                    account: account,
                    syncStatus: model.accountSyncStatuses[account.accountId]
                )
            }
        }
        .listStyle(.inset(alternatesRowBackgrounds: true))
    }

    private var emptyView: some View {
        VStack(spacing: 12) {
            Image(systemName: "tray")
                .font(.system(size: 32))
                .foregroundStyle(.tertiary)
            Text("No accounts configured.")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func unavailableView(_ message: String) -> some View {
        VStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 28))
                .foregroundStyle(.orange)
            Text("Diagnostics unavailable")
                .fontWeight(.medium)
            Text(message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding()
    }

    // MARK: - Bottom bar

    private var bottomBar: some View {
        HStack(spacing: 12) {
            integritySection
            Spacer()
            exportSection
        }
    }

    @ViewBuilder
    private var integritySection: some View {
        switch vm.integrityState {
        case .idle:
            Button("Run Database Check") { vm.runIntegrityCheck() }
                .accessibilityLabel("Run SQLite integrity check")
        case .running:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Checking…")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
        case .ok:
            HStack(spacing: 4) {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                Text("Database OK")
                    .font(.callout)
                Button("Clear") { vm.resetIntegrity() }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Database check passed")
        case .failed(let count, let first):
            HStack(spacing: 4) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.red)
                Text("\(count) issue(s) found")
                    .font(.callout)
                if let first {
                    Text("— \(first)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Button("Clear") { vm.resetIntegrity() }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("\(count) database issue(s) found")
        case .error(let msg):
            HStack(spacing: 4) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                Text(msg)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button("Clear") { vm.resetIntegrity() }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
        }
    }

    @ViewBuilder
    private var exportSection: some View {
        VStack(alignment: .trailing, spacing: 4) {
            switch vm.exportState {
            case .idle:
                Button("Gather Diagnostics…") { vm.beginGather() }
                    .accessibilityLabel("Gather a sanitised diagnostics bundle")
                bundleCaption
            case .gathering:
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Gathering…")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
            case .saving:
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Saving…")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
                bundleCaption
            case .readyToSave:
                // Handled via .onChange → window-modal destination picker;
                // show a spinner while the sheet comes up.
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Preparing…")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
            case .saved(let url):
                HStack(spacing: 4) {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                    Text("Saved \(url.lastPathComponent)")
                        .font(.caption)
                    Button("Reveal in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([url])
                    }
                    .buttonStyle(.borderless)
                    .font(.caption)
                    Button("Clear") { vm.resetExport() }
                        .buttonStyle(.borderless)
                        .font(.caption)
                }
                .accessibilityLabel("Diagnostics saved to \(url.lastPathComponent)")
            case .error(let msg):
                HStack(spacing: 4) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                    Text(msg)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button("Dismiss") { vm.resetExport() }
                        .buttonStyle(.borderless)
                        .font(.caption)
                }
            }
        }
    }

    private var bundleCaption: some View {
        Text(
            "Includes app/OS versions, provider capabilities, anonymised sync counters and error categories, a short recent app-log excerpt, and any recent Castellum crash reports — with addresses, message content and credentials always removed. Nothing is sent anywhere: you choose where to save it, and can attach the folder to an email yourself."
        )
        .font(.caption2)
        .foregroundStyle(.tertiary)
        .frame(maxWidth: 360)
        .multilineTextAlignment(.trailing)
    }

    // MARK: - Helpers

    private var exportReadyGathered: GatheredDiagnostics? {
        if case .readyToSave(let gathered) = vm.exportState { return gathered }
        return nil
    }
}

// MARK: - Performance section

/// Glanceable last-sync performance, sourced from `castellum_last_sync_kpi`.
/// Shown unconditionally (no "Gather Diagnostics…" click needed) — this is
/// HISTORICAL/last-run data, a complement to `AccountHeaderRow`'s LIVE
/// `syncStatus` text for a sync currently in progress, not a replacement
/// for it.
private struct PerformanceSection: View {
    let state: DiagnosticsViewModel.KpiState

    var body: some View {
        Section("Last Sync Performance") {
            switch state {
            case .loading:
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Loading…")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
            case .none:
                Text("No sync recorded yet.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            case .unavailable(let message):
                HStack(spacing: 4) {
                    Image(systemName: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                    Text(message)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            case .loaded(let kpi):
                KpiSummaryRows(kpi: kpi)
            }
        }
    }
}

/// The actual glanceable numbers for a loaded `LastSyncKpi`: throughput
/// (msgs/sec), IMAP round-trip time (sum of every `imap.*` phase) and DB
/// write time (sum of every `db.*` phase, which is where
/// `db.upsert_descriptor`/`db.rebuild_threads` — the two phases the 2026-09-23
/// O(n²) FTS incident made expensive — would show up first). A trailing
/// "As of …" line makes clear this is the LAST run, not a live number.
private struct KpiSummaryRows: View {
    let kpi: LastSyncKpi

    private static let relativeFormatter: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return f
    }()

    var body: some View {
        Group {
            metricRow("Messages synced", "\(kpi.messageCount)")
            if let rate = kpi.messagesPerSecond {
                metricRow("Throughput", "\(String(format: "%.1f", rate)) msgs/sec")
            }
            metricRow("Wall time", "\(String(format: "%.2f", kpi.wallMs / 1000)) s")
            metricRow("IMAP round-trip", "\(String(format: "%.0f", kpi.totalMs(forPhasePrefix: "imap."))) ms")
            metricRow("DB write time", "\(String(format: "%.0f", kpi.totalMs(forPhasePrefix: "db."))) ms")
            Text("As of \(Self.relativeFormatter.localizedString(for: kpi.timestamp, relativeTo: Date()))")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
    }

    private func metricRow(_ label: String, _ value: String) -> some View {
        HStack {
            Text(label)
                .font(.callout)
            Spacer()
            Text(value)
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }
}

// MARK: - Account section

private struct AccountSection: View {
    let account: LocalAccountDiagnostics
    let syncStatus: AccountSyncStatus?

    var body: some View {
        Section {
            ForEach(account.mailboxes, id: \.mailboxId) { mailbox in
                MailboxRow(mailbox: mailbox)
                    .tag(mailbox.mailboxId)
            }
        } header: {
            AccountHeaderRow(account: account, syncStatus: syncStatus)
        }
    }
}

private struct AccountHeaderRow: View {
    let account: LocalAccountDiagnostics
    let syncStatus: AccountSyncStatus?

    /// Sync-state tracking is newer than most profiles: an account whose
    /// folders already hold mail but has no recorded state should say "no
    /// sync recorded", not imply it never fetched anything.
    private var hasLocalData: Bool {
        account.mailboxes.contains { $0.totalMessages > 0 }
    }

    var body: some View {
        HStack(spacing: 8) {
            statusIcon(consecutiveFailures: account.consecutiveFailures,
                       lastSuccessAt: account.lastSuccessAt,
                       lastAttemptAt: account.lastAttemptAt,
                       hasLocalData: hasLocalData)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(account.email)
                        .font(.headline)
                        .foregroundStyle(.primary)
                    Text(account.provider.uppercased())
                        .font(.caption2)
                        .fontWeight(.semibold)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Color.secondary.opacity(0.18)))
                        .foregroundStyle(.secondary)
                }
                if let syncStatus {
                    Text(syncStatus.message)
                        .font(.caption)
                        .foregroundStyle(syncStatus.state.isFailure ? Color.orange : Color.secondary)
                    if let rawError = syncStatus.rawError, !rawError.isEmpty {
                        Text("Raw error: \(rawError)")
                            .font(.caption2.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                } else {
                    syncSummaryText(
                        consecutiveFailures: account.consecutiveFailures,
                        lastSuccessAt: account.lastSuccessAt,
                        lastAttemptAt: account.lastAttemptAt,
                        lastErrorKind: account.lastErrorKind,
                        lastError: account.lastError,
                        lastErrorPhase: account.lastErrorPhase,
                        hasLocalData: hasLocalData
                    )
                }
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accountAccessibilityLabel)
    }

    private var accountAccessibilityLabel: String {
        let status = statusLabel(
            consecutiveFailures: account.consecutiveFailures,
            lastSuccessAt: account.lastSuccessAt,
            lastAttemptAt: account.lastAttemptAt,
            hasLocalData: hasLocalData
        )
        return String(localized: "\(account.email): \(status)")
    }
}

private struct MailboxRow: View {
    let mailbox: LocalMailboxDiagnostics

    private var hasLocalData: Bool { mailbox.totalMessages > 0 }

    var body: some View {
        HStack(spacing: 8) {
            statusIcon(consecutiveFailures: mailbox.consecutiveFailures,
                       lastSuccessAt: mailbox.lastSuccessAt,
                       lastAttemptAt: mailbox.lastAttemptAt,
                       hasLocalData: hasLocalData)
                .font(.caption)
            VStack(alignment: .leading, spacing: 1) {
                Text(mailbox.path)
                    .font(.callout)
                syncSummaryText(
                    consecutiveFailures: mailbox.consecutiveFailures,
                    lastSuccessAt: mailbox.lastSuccessAt,
                    lastAttemptAt: mailbox.lastAttemptAt,
                    lastErrorKind: mailbox.lastErrorKind,
                    lastError: mailbox.lastError,
                    lastErrorPhase: mailbox.lastErrorPhase,
                    hasLocalData: hasLocalData
                )
                .font(.caption)
                HStack(spacing: 8) {
                    Text("\(mailbox.totalMessages) messages")
                    if mailbox.messagesWithoutBody > 0 {
                        Text("·")
                        Text("\(mailbox.messagesWithoutBody) missing body")
                    }
                }
                .font(.caption2)
                .foregroundStyle(.tertiary)
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(mailboxAccessibilityLabel)
    }

    private var mailboxAccessibilityLabel: String {
        let status = statusLabel(
            consecutiveFailures: mailbox.consecutiveFailures,
            lastSuccessAt: mailbox.lastSuccessAt,
            lastAttemptAt: mailbox.lastAttemptAt,
            hasLocalData: hasLocalData
        )
        return String(localized: "\(mailbox.path): \(status), \(mailbox.totalMessages) messages")
    }
}

// MARK: - Shared helpers

/// Sync-health status icon.
private func statusIcon(
    consecutiveFailures: Int64,
    lastSuccessAt: String?,
    lastAttemptAt: String?,
    hasLocalData: Bool
) -> some View {
    let (name, colour) = statusIconSpec(
        consecutiveFailures: consecutiveFailures,
        lastSuccessAt: lastSuccessAt,
        lastAttemptAt: lastAttemptAt,
        hasLocalData: hasLocalData
    )
    return Image(systemName: name).foregroundStyle(colour)
}

private func statusIconSpec(
    consecutiveFailures: Int64,
    lastSuccessAt: String?,
    lastAttemptAt: String?,
    hasLocalData: Bool
) -> (String, Color) {
    if consecutiveFailures > 0 {
        return ("xmark.circle.fill", .red)
    }
    if lastSuccessAt != nil {
        return ("checkmark.circle.fill", .green)
    }
    if lastAttemptAt != nil {
        return ("circle.dotted", .secondary)
    }
    if hasLocalData {
        return ("questionmark.circle", .secondary)
    }
    return ("circle", .secondary)
}

/// Status wording rule: "never synced" is only claimed when there is no
/// local data at all. A folder that already holds messages but has no
/// recorded sync state predates sync-state tracking — say "no sync
/// recorded", which is what the database actually shows.
func statusLabel(
    consecutiveFailures: Int64,
    lastSuccessAt: String?,
    lastAttemptAt: String?,
    hasLocalData: Bool
) -> String {
    if consecutiveFailures > 0 { return "error" }
    if lastSuccessAt != nil { return "healthy" }
    if lastAttemptAt != nil { return "syncing" }
    return hasLocalData ? "no sync recorded" : "never synced"
}

@ViewBuilder
private func syncSummaryText(
    consecutiveFailures: Int64,
    lastSuccessAt: String?,
    lastAttemptAt: String?,
    lastErrorKind: String?,
    lastError: String?,
    lastErrorPhase: String?,
    hasLocalData: Bool
) -> some View {
    if consecutiveFailures > 0 {
        HStack(spacing: 4) {
            if let kind = lastErrorKind {
                Text(kind)
                    .foregroundStyle(.red)
                    .accessibilityLabel("Error kind: \(kind)")
            }
            if let error = lastError {
                Text("— \(error)")
                    .foregroundStyle(.secondary)
            }
            if let phase = lastErrorPhase {
                Text("(\(phase))")
                    .foregroundStyle(.tertiary)
            }
            Text("· \(consecutiveFailures) failure(s)")
                .foregroundStyle(.red)
                .accessibilityLabel("\(consecutiveFailures) consecutive failures")
        }
        .font(.caption)
    } else if let ts = lastSuccessAt {
        Text("Last synced: \(ts)")
            .font(.caption)
            .foregroundStyle(.secondary)
    } else if lastAttemptAt != nil {
        Text("Syncing…")
            .font(.caption)
            .foregroundStyle(.secondary)
    } else if hasLocalData {
        Text("No sync recorded yet — status tracking is new in this version")
            .font(.caption)
            .foregroundStyle(.tertiary)
    } else {
        Text("Never synced")
            .font(.caption)
            .foregroundStyle(.tertiary)
    }
}

// MARK: - Window accessor

/// Captures the hosting `NSWindow` so the save panel can be presented as a
/// window-modal sheet (`beginSheetModal(for:)`) rather than a floating panel.
struct WindowAccessor: NSViewRepresentable {
    @Binding var window: NSWindow?

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async { self.window = view.window }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        DispatchQueue.main.async {
            if self.window !== nsView.window { self.window = nsView.window }
        }
    }
}
