// Castellum - standalone compose window (⌘N / Reply / draft reopen).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Same self-retaining pattern as StandaloneMessageWindow. On close (⌘W or the
// close button), user-edited content is saved silently and an existing draft
// edited down to empty is discarded. Untouched reply-prefilled content is not
// a user edit and therefore does not create a draft. Sent/queued messages skip
// close-time persistence because their content is already durable.

import AppKit
import SwiftUI

@MainActor
final class ComposeWindowController: NSObject, NSWindowDelegate {
    private static var open: Set<ComposeWindowController> = []
    /// True while at least one compose window is open (palette availability).
    static var hasOpenWindow: Bool { !open.isEmpty }
    private let window: NSWindow
    private let session: ComposeSession
    private let model: AppModel
    /// Held so a streaming AI draft can be cancelled when the window closes.
    /// Passed for every window (reply or blank) so ComposeView can render the
    /// "Draft with AI" affordance either way; nil only when AI isn't configured.
    private let aiCoordinator: AiCoordinator?
    /// HP1 Send Later: notification observers for the Send Later commands.
    private var sendLaterObservers: [NSObjectProtocol] = []
    /// The compose window that was key most recently (weak: it may close).
    /// Send Later commands target the key compose window, else this one, so
    /// the command palette (which lives in the main window) still reaches the
    /// message being written.
    private static weak var lastKey: ComposeWindowController?

    #if DEBUG
    /// The session of the compose window the UIT-1 harness should drive: the key one, else the
    /// last key one, else any open one. Debug builds only.
    static var sessionForHarness: ComposeSession? {
        (open.first { $0.window.isKeyWindow } ?? lastKey ?? open.first)?.session
    }
    #endif

    /// Open a compose window. Pass `aiCoordinator` whenever AI is configured,
    /// reply or blank — ComposeView itself decides whether "Draft with AI" is
    /// enabled (a reply, with an original message) or shown disabled (blank).
    static func show(model: AppModel, session: ComposeSession,
                     aiCoordinator: AiCoordinator? = nil) {
        // One editor per stored draft: a second open of the same draft raises
        // the window that already holds it (compared on the live draftId, so a
        // message autosaved after opening is covered too).
        if let id = session.draftId, let existing = open.first(where: { $0.session.draftId == id }) {
            existing.window.deminiaturize(nil)
            existing.window.makeKeyAndOrderFront(nil)
            return
        }
        // M3 signpost: command -> window visible (PerfSignpost.swift).
        var openSpan = PerfSignpost.begin(.composeOpen)
        let controller = ComposeWindowController(model: model, session: session,
                                                 aiCoordinator: aiCoordinator)
        open.insert(controller)
        controller.window.center()
        controller.window.makeKeyAndOrderFront(nil)
        PerfSignpost.end(&openSpan)
    }

    private init(model: AppModel, session: ComposeSession,
                 aiCoordinator: AiCoordinator? = nil) {
        self.model = model
        self.session = session
        self.aiCoordinator = aiCoordinator
        let view = ComposeView(model: model, session: session, aiCoordinator: aiCoordinator)
        let hosting = NSHostingController(rootView: view)
        window = NSWindow(contentViewController: hosting)
        window.title = session.subject.isEmpty ? "New Message" : session.subject
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 560, height: 460))
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
        session.requestClose = { [weak self] in self?.window.close() }
        installSendLaterCommands()
    }

    func windowDidBecomeKey(_ notification: Notification) {
        Self.lastKey = self
    }

    /// Route the Send Later commands (menu, palette, key bindings) to the
    /// compose window being written. Exactly one window acts on each command.
    private func installSendLaterCommands() {
        let routes: [(Notification.Name, SendLaterPreset?)] = [
            (.castellumSendLater, nil),
            (.castellumSendLaterInOneHour, .inOneHour),
            (.castellumSendLaterThisEvening, .thisEvening),
            (.castellumSendLaterTomorrowMorning, .tomorrowMorning),
            (.castellumSendLaterMonday, .mondayMorning),
        ]
        for (name, preset) in routes {
            let token = NotificationCenter.default.addObserver(
                forName: name, object: nil, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, self.isSendLaterTarget else { return }
                    if let preset {
                        self.session.sendLaterCommand = preset
                    } else {
                        self.session.showSendLaterPicker = true
                    }
                }
            }
            sendLaterObservers.append(token)
        }
        // MM-7: "Insert Snippet…" targets the compose window the same way.
        let snippetToken = NotificationCenter.default.addObserver(
            forName: .castellumInsertSnippet, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.isSendLaterTarget else { return }
                self.session.showSnippetPicker = true
            }
        }
        sendLaterObservers.append(snippetToken)
    }

    private var isSendLaterTarget: Bool {
        if window.isKeyWindow { return true }
        let anyComposeKey = Self.open.contains { $0.window.isKeyWindow }
        if anyComposeKey { return false }
        // The one compose window that exists is the only possible target, even if it never
        // became key (the app was not frontmost when the command ran).
        return Self.lastKey === self || (Self.lastKey == nil && Self.open.count == 1)
    }

    func windowWillClose(_ notification: Notification) {
        saveOrDiscardOnClose()
        cancelInFlightAiDraft()
        for token in sendLaterObservers { NotificationCenter.default.removeObserver(token) }
        sendLaterObservers = []
        Self.open.remove(self)
    }

    /// Closing the window destroys the only place a draft suggestion could
    /// land, so a still-running `draft_reply`/`draft_message` job is pure
    /// waste: Ollama keeps generating against a dead window and the entry
    /// stays in `AiCoordinator.jobs` as a zombie until the app restarts (see
    /// #427). Cancel it — a reply is keyed by the original message id, a
    /// blank compose window by its own `composeSessionId`, so a second
    /// window drafting against a different message/session is unaffected.
    private func cancelInFlightAiDraft() {
        guard let aiCoordinator else { return }
        if let originalId = session.originalMessageDbId {
            aiCoordinator.cancelActiveJobs(forOwnerId: originalId, feature: "draft_reply")
        } else {
            aiCoordinator.cancelActiveJobs(
                forOwnerId: session.composeSessionId, feature: "draft_message")
        }
    }

    /// ⌘W behavior: persist only an actual edit relative to the state the
    /// composer opened with. Skipped once the message was sent or queued
    /// (content already persisted), and while a send is in flight (its own save
    /// is about to assign the draft id; saving here too would insert a duplicate).
    private func saveOrDiscardOnClose() {
        let session = self.session
        let model = self.model
        guard !session.suppressSaveOnClose, !session.isSending else {
            // Sent or sending: the send cleans up any server mirror itself.
            if let draftId = session.draftId { model.cancelServerDraftSave(draftId: draftId) }
            return
        }
        // While the first save of a new draft is in flight, decide once it
        // has reported, so the close updates that row instead of inserting a
        // second one (#3444). The closure keeps the session alive meanwhile.
        session.afterPendingDraftCreation {
            // Checked again here: a Send queued behind the same creation may
            // have started meanwhile (Send then Cmd-W), and saving now would
            // insert a second row beside the send's own (review of #3444).
            guard !session.suppressSaveOnClose, !session.isSending else {
                if let draftId = session.draftId { model.cancelServerDraftSave(draftId: draftId) }
                return
            }
            switch session.closeAction {
            case .none:
                if let draftId = session.draftId { model.cancelServerDraftSave(draftId: draftId) }
            case .save:
                // Edits from the last debounce window (or a recipient-only
                // edit) have not reached the server Drafts copy yet: mirror
                // them now instead of dropping the pending save (#3444).
                if let draftId = session.draftId, model.hasPendingServerDraftSave(draftId: draftId) {
                    model.flushServerDraftSave(session: session, draftId: draftId)
                } else {
                    model.saveDraft(session.buildDraftForSave(
                        renderMarkdown: session.markdownRenderer(core: model.core)
                    ))
                }
            case .deleteDraft(let id):
                model.cancelServerDraftSave(draftId: id)
                model.deleteDraft(id: id, accountId: session.accountId)
            }
        }
    }
}
