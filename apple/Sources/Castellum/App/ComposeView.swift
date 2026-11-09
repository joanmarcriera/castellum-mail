// Castellum - compose window content (new message / reply / draft editing).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Plain-text v1: To/Cc/Bcc/Subject + body, account picker when there is more
// than one account. Send = save draft -> queue -> SMTP delivery pass; a failed
// delivery leaves the message in the durable outbox and says so. Replies open
// with the quoted original below the caret (ReplyPrefill.body); no
// attachments/HTML yet.

import SwiftUI
import UniformTypeIdentifiers

/// Persistence work requested when a compose window closes normally.
/// Lifecycle guards such as an in-flight send are handled by the window
/// controller before consulting this content-derived action.
enum ComposeCloseAction: Equatable {
    case none
    case save
    case deleteDraft(Int64)
}

/// Normalise optional user guidance before it enters an AI prompt.
enum AiDraftInstruction {
    static func normalised(_ instruction: String) -> String? {
        let trimmed = instruction.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

/// Which AI job `AiDraftSection` drives: a reply (drafting against an
/// original message) or a blank compose window (drafting from an
/// instruction, keyed by the window's own stable id — there is no message to
/// key against). Distinct feature names and prompt builders on the core side.
fileprivate enum AiDraftMode: Equatable {
    case reply(originalMessageId: String)
    case newMessage(sessionId: String)

    var feature: String {
        switch self {
        case .reply: return "draft_reply"
        case .newMessage: return "draft_message"
        }
    }

    /// The key `AiCoordinator.jobs` tracks this mode's job under.
    var ownerId: String {
        switch self {
        case .reply(let originalMessageId): return originalMessageId
        case .newMessage(let sessionId): return sessionId
        }
    }
}

/// The mutable state of one compose window. Owned by the window controller so
/// close-time behavior (save silently / discard) can inspect it.
@MainActor
final class ComposeSession: ObservableObject {
    private struct EditableSnapshot: Equatable {
        let accountId: String
        let toText: String
        let ccText: String
        let bccText: String
        let subject: String
        let body: String
    }

    /// Changing the From account swaps the old signature for the new one, but
    /// only when the old signature block is still present unmodified — a user
    /// edit (or removal) is never clobbered (`SignatureSupport.
    /// swapForAccountChange`). Swift doesn't call `didSet` for a property's own
    /// initial assignment inside `init`, so this never fires while `init` sets
    /// `accountId` for the first time — only on a later, user-driven change
    /// (e.g. the From picker in `ComposeView`).
    @Published var accountId: String {
        didSet {
            guard accountId != oldValue else { return }
            // The new account's default replaces the inserted signature
            // (compared against what was inserted, not the library's current
            // text). An edited one stays, and no longer counts as any of the
            // new account's signatures. Its HTML version is dropped too: the
            // HTML part must never carry the previous account's signature
            // HTML behind a plain block the user is now editing as their own.
            // After an explicit "No signature" ( review: honour
            // the choice), the new account's default is not inserted either.
            let next = userChoseNoSignature ? nil : preferences.signatureLibrary.defaultItem(for: accountId)
            if !switchSignature(to: next, insertWhenRemoved: false) {
                signatureId = nil
                insertedSignatureHTML = nil
            }
            signatureNotice = nil
            // An identity belongs to exactly one account — switching From
            // accounts must not keep a stale identity pointed at the wrong
            // one (S47).
            fromIdentity = nil
            // The new account gets its own default; a reply's "addressed to"
            // no longer applies across accounts.
            identityContext = .newMessage
            identityAutoChosen = false
        }
    }
    /// The identity/alias chosen in the From picker for THIS message, or
    /// `nil` for the account's own address (S47).
    @Published var fromIdentity: ComposeIdentity?
    /// What this window was opened for, for the automatic From choice (MM-6).
    private(set) var identityContext: IdentitySelectionContext = .newMessage
    /// True once the From was chosen automatically (or came from a reopened
    /// draft, or the user picked one): the automatic choice runs at most once.
    private(set) var identityAutoChosen = false

    /// Picks the starting From identity once the account's identities are
    /// known (they load when the window appears). See `IdentitySelector`.
    func autoSelectIdentity(identities: [Identity], accountEmail: String) {
        guard !identityAutoChosen else { return }
        identityAutoChosen = true
        fromIdentity = IdentitySelector.choose(
            context: identityContext, accountEmail: accountEmail, identities: identities)
    }

    /// The user picked a From by hand: leave it alone from now on.
    func userChoseFrom(_ identity: ComposeIdentity?) {
        identityAutoChosen = true
        fromIdentity = identity
    }
    // MARK: signature picker

    /// The saved signature whose block compose last put into `body`, shown by
    /// the footer picker; nil = none (or not one of this account's).
    @Published private(set) var signatureId: String?
    /// That signature's plain text and HTML *as inserted*: swaps compare
    /// against what is in the body, so editing the library while this window
    /// is open cannot strand the block.
    private(set) var insertedSignature = ""
    private(set) var insertedSignatureHTML: String?
    /// One-line caption when a picked signature could not be applied.
    @Published var signatureNotice: String?
    /// The user explicitly picked "No signature" in this window: a later
    /// From change keeps the message unsigned instead of inserting the new
    /// account's default. In-session only (a reopened draft without a
    /// recognised signature cannot tell "chose none" from "deleted it").
    private(set) var userChoseNoSignature = false

    /// The footer picker: swap in `id`'s signature (nil = None) unless the
    /// signature in the body was edited, in which case nothing changes and
    /// `signatureNotice` says why.
    func chooseSignature(_ id: String?) {
        guard id != signatureId else { return }
        let item = id.flatMap { preferences.signatureLibrary.item(id: $0) }
        if switchSignature(to: item, insertWhenRemoved: true) {
            signatureNotice = nil
            userChoseNoSignature = (id == nil)
        } else if SignatureSupport.block(for: insertedSignature).isEmpty {
            // Nothing of ours was inserted: the "-- " line was typed (or pasted).
            signatureNotice = String(localized: "Signature not changed: the message already has a “-- ” signature line. Delete it to insert one.")
        } else {
            signatureNotice = String(localized: "Signature not changed: the one in the message was edited. Delete it to insert another.")
        }
    }

    /// Applies `item` (nil = none) over the inserted signature; true when it
    /// took effect (state updated), false when the body was left alone.
    @discardableResult
    private func switchSignature(to item: SignatureItem?, insertWhenRemoved: Bool) -> Bool {
        let outcome = SignatureSwitch.apply(from: insertedSignature, to: item?.plainText ?? "",
                                            in: body, insertWhenRemoved: insertWhenRemoved)
        guard outcome.applied else { return false }
        if outcome.body != body { body = outcome.body }
        signatureId = item?.id
        insertedSignature = item?.plainText ?? ""
        insertedSignatureHTML = item?.htmlIfAny
        return true
    }

    /// Records the signature `init` inserted: the account default when its
    /// text is what the caller passed (the production path), else just the
    /// text (tests pass bare text).
    private func recordInitialSignature(_ text: String, accountId: String) {
        insertedSignature = text
        if let item = preferences.signatureLibrary.defaultItem(for: accountId),
           !text.isEmpty, item.plainText == text {
            signatureId = item.id
            insertedSignatureHTML = item.htmlIfAny
        }
    }

    /// Where account signatures come from — `Preferences.shared` in
    /// production; a test injects an isolated `Preferences(defaults:)` so
    /// signature tests never touch the app's real UserDefaults domain.
    private let preferences: Preferences
    @Published var toText: String
    @Published var ccText = ""
    @Published var bccText = ""
    @Published var subject: String
    @Published var body: String
    /// Set once the draft exists in the core (updates instead of inserting).
    @Published var draftId: Int64?
    /// True only for a window reopened from a stored draft (`init(draft:)`).
    /// Any other window that holds a `draftId` created it itself (early save
    /// for attachments, autosave), so closing it empty must delete that row.
    private(set) var openedFromStoredDraft = false
    /// The Re:/Fwd: subject the window opened with; it is not the user's text,
    /// so the forgotten-attachment check ignores it while unchanged.
    private(set) var inheritedSubject = ""
    /// The attribution line the app wrote into a reply's body (nil otherwise);
    /// not persisted, so a reopened draft falls back to the `wrote:` detector.
    private(set) var generatedAttribution: String?
    /// Whether this compose window sends Markdown as multipart/alternative
    /// (S46). Starts from the account's Settings > Accounts default
    /// (`Preferences.markdownDefault(for:)`) for a fresh message, or from
    /// whether the reopened draft already carries rendered html for an
    /// existing one; either way it is then a free-standing per-message
    /// choice — switching the From account does not reset it, mirroring how
    /// every other field in this class is a deliberate user choice once set.
    @Published var markdownEnabled: Bool
    /// The style of the HTML alternative (MM-5): starts from
    /// `Preferences.markdownTheme`, then is a per-message choice. Used by the
    /// live preview AND the sent part, through `markdownRenderer(core:)`.
    @Published var markdownTheme: MarkdownThemeChoice
    /// Non-nil while the last send attempt has something to report.
    @Published var sendError: String?
    /// The last send was refused because SMTP AUTH is switched off for the
    /// mailbox (MS1): the explanation and Microsoft's link go under the
    /// caption. Reset by the next send attempt's outcome.
    @Published var showsSmtpAuthDisabledHelp = false
    /// Non-nil when the last send attempt failed because no credentials are
    /// stored for this account — presented as an actionable alert (the tracker
    /// #2152) rather than folded into `sendError`'s inline caption.
    @Published var missingCredentialsAccountId: String?
    @Published var isSending = false
    /// HP5a: the word that made the forgotten-attachment check ask before
    /// sending; non-nil while the "No attachment" confirmation is showing.
    @Published var attachmentWarningWord: String?
    /// HP1 Send Later: a preset chosen from a command (palette, menu or key
    /// binding) for ComposeView to act on, and the picker popover's visibility
    /// (also opened by the "Send Later…" command).
    @Published var sendLaterCommand: SendLaterPreset?
    @Published var showSendLaterPicker = false
    /// MM-7 snippets: the picker sheet's visibility (also opened by the "Insert Snippet…"
    /// command) and the handle to the body's NSTextView, which snippets are inserted through.
    @Published var showSnippetPicker = false
    let bodyHandle = ComposeBodyHandle()
    /// The time chosen for the send the attachment warning is currently
    /// holding (nil = an ordinary Send), so "Send Anyway" keeps it.
    var pendingSendAt: Date?
    /// The draft reached the outbox: content is durably persisted and no longer
    /// editable (the core refuses updates once status != draft).
    @Published var queuedInOutbox = false
    /// The draft's attachments (metadata only; bytes live in the core).
    @Published var attachments: [DraftAttachmentMeta] = []
    /// Non-nil while the last attach/remove/forward-copy has something to report.
    @Published var attachmentError: String?
    /// True once the user has attached or removed a file themselves, so an
    /// attachment-only message counts as an edit worth saving on close.
    private(set) var userTouchedAttachments = false
    /// Non-nil only for a Forward window: the source message whose attachments
    /// get copied in once the draft has an id. Distinct from
    /// `originalMessageDbId` (which a forward also sets, for AI) because this
    /// gates the one-time attachment copy so it never fires for a reply.
    let forwardSourceMessageId: String?
    /// How many attachments the forwarded message has, to notice a shortfall.
    let forwardSourceAttachmentCount: Int
    private var didCopyForwardedAttachments = false

    let inReplyTo: String?
    let referencesHeader: String?
    /// Database id of the original message this is a reply to (e.g.
    /// `"gmail:acct:42"`). Used by AI draft reply. Distinct from `inReplyTo`,
    /// which is the RFC 2822 Message-ID header used for threading.
    let originalMessageDbId: String?
    /// Stable per-window id for AI "Draft with AI" jobs on a BLANK compose
    /// window (feature = draft_message) — there is no original message id to
    /// key by, unlike a reply's `originalMessageDbId`. Generated once and
    /// never persisted; only used to track/cancel the in-flight job.
    let composeSessionId = UUID().uuidString
    /// Set by the window controller; asks the hosting window to close.
    var requestClose: (() -> Void)?
    /// Set after a successful send (or once queued) so closing doesn't re-save.
    var suppressSaveOnClose = false

    /// Reply fields and quoted content are generated before the user sees the
    /// window. Preserve that baseline so they do not count as an edit merely
    /// because the compose session is non-empty.
    private let initialSnapshot: EditableSnapshot

    /// A fresh message (optionally prefilled as a reply). `signature` is the
    /// account's plain-text signature (Settings > Accounts via
    /// `Preferences.signature(for:)`); the caller passes it in rather than this
    /// class reaching for `Preferences.shared` itself, so it stays a pure,
    /// no-hidden-dependency initializer for `accountId`/`prefill` alone —
    /// `Preferences.shared` is only consulted from `accountId`'s `didSet`
    /// (a user-driven From-account change), never from `init`. Inserted above
    /// the quoted text for a reply (`SignatureSupport.insert`); folded into
    /// `initialSnapshot` so it isn't mistaken for a user edit.
    init(accountId: String, prefill: ReplyPrefill? = nil, signature: String = "",
         preferences: Preferences = .shared) {
        let toText = prefill?.toText ?? ""
        let ccText = prefill?.ccText ?? ""
        let subject = prefill?.subject ?? ""
        self.inheritedSubject = subject
        self.generatedAttribution = prefill?.attributionLine
        let body = SignatureSupport.insert(signature: signature, into: prefill?.body ?? "")
        self.preferences = preferences
        self.accountId = accountId
        self.markdownEnabled = preferences.markdownDefault(for: accountId)
        self.markdownTheme = preferences.markdownTheme
        self.toText = toText
        self.ccText = ccText
        self.subject = subject
        self.body = body
        self.inReplyTo = prefill?.inReplyTo
        self.referencesHeader = prefill?.referencesHeader
        // A forward is not a reply: AI "Draft with AI" treats it as a blank
        // message rather than drafting a reply to the forwarded original.
        self.originalMessageDbId = (prefill?.isForward == true) ? nil : prefill?.originalDbId
        self.forwardSourceMessageId = (prefill?.isForward == true) ? prefill?.originalDbId : nil
        self.forwardSourceAttachmentCount = prefill?.forwardAttachmentCount ?? 0
        if let prefill, !prefill.isForward, prefill.originalDbId != nil {
            self.identityContext = .reply(recipients: prefill.originalRecipients)
        }
        self.initialSnapshot = EditableSnapshot(
            accountId: accountId,
            toText: toText,
            ccText: ccText,
            bccText: "",
            subject: subject,
            body: body
        )
        recordInitialSignature(SignatureSupport.block(for: signature).isEmpty ? "" : signature,
                               accountId: accountId)
    }

    /// Reopen an existing (still-editable) draft. `body` is used verbatim —
    /// no signature insertion here (S26): a saved draft's body already holds
    /// whatever it held when it was saved, signature block included if one
    /// was inserted for the original compose session, so reopening it never
    /// adds a second one.
    init(draft: Draft, preferences: Preferences = .shared) {
        let toText = AddressFormatter.text(draft.to)
        let ccText = AddressFormatter.text(draft.cc)
        let bccText = AddressFormatter.text(draft.bcc)
        self.preferences = preferences
        self.accountId = draft.accountId
        self.markdownEnabled = draft.bodyHtml != nil
        self.markdownTheme = MarkdownThemeChoice.infer(fromHtml: draft.bodyHtml) ?? preferences.markdownTheme
        self.fromIdentity = draft.fromEmail.map { ComposeIdentity(email: $0, name: draft.fromName) }
        self.identityAutoChosen = true   // a reopened draft keeps the From it was saved with
        self.toText = toText
        self.ccText = ccText
        self.bccText = bccText
        self.subject = draft.subject
        self.body = draft.bodyText
        self.draftId = draft.id
        self.openedFromStoredDraft = true
        self.inheritedSubject = draft.inReplyTo != nil ? draft.subject : ""
        self.inReplyTo = draft.inReplyTo
        self.referencesHeader = draft.referencesHeader
        self.originalMessageDbId = nil   // drafts don't carry the original DB id
        self.forwardSourceMessageId = nil   // a reopened draft already holds its own attachments
        self.forwardSourceAttachmentCount = 0
        self.initialSnapshot = EditableSnapshot(
            accountId: draft.accountId,
            toText: toText,
            ccText: ccText,
            bccText: bccText,
            subject: draft.subject,
            body: draft.bodyText
        )
        self.storedDraftChoices = (draft.fromEmail, markdownEnabled, markdownTheme)
        // Which saved signature the draft still carries, for the picker.
        if let item = SignatureSwitch.recognise(
            in: draft.bodyText, among: preferences.signatureLibrary.items(for: draft.accountId)) {
            signatureId = item.id
            insertedSignature = item.plainText
            insertedSignatureHTML = item.htmlIfAny
        } else {
            // None recognised (edited, or never had one): compare against the
            // account default, as S26 did, so a From change or a pick never
            // stacks a second signature under an edited one.
            insertedSignature = preferences.signature(for: draft.accountId)
        }
    }

    /// A message prefilled from a `mailto:` link, a Services menu selection,
    /// or a Share Extension handoff (S57): to/cc/bcc plus subject and body,
    /// all already-decoded plain text — no address parsing here, exactly
    /// like a blank compose window (`AddressParser` runs once, on Send).
    /// Distinct from `ReplyPrefill` because none of these three sources
    /// carries threading/original-message metadata.
    init(accountId: String, mailto: MailtoPrefill, signature: String = "",
         preferences: Preferences = .shared) {
        let body = SignatureSupport.insert(signature: signature, into: mailto.body)
        self.preferences = preferences
        self.accountId = accountId
        self.markdownEnabled = preferences.markdownDefault(for: accountId)
        self.markdownTheme = preferences.markdownTheme
        self.toText = mailto.to
        self.ccText = mailto.cc
        self.bccText = mailto.bcc
        self.subject = mailto.subject
        self.body = body
        self.inReplyTo = nil
        self.referencesHeader = nil
        self.originalMessageDbId = nil
        self.forwardSourceMessageId = nil
        self.forwardSourceAttachmentCount = 0
        self.initialSnapshot = EditableSnapshot(
            accountId: accountId,
            toText: mailto.to,
            ccText: mailto.cc,
            bccText: mailto.bcc,
            subject: mailto.subject,
            body: body
        )
        recordInitialSignature(SignatureSupport.block(for: signature).isEmpty ? "" : signature,
                               accountId: accountId)
    }

    private var currentSnapshot: EditableSnapshot {
        EditableSnapshot(
            accountId: accountId,
            toText: toText,
            ccText: ccText,
            bccText: bccText,
            subject: subject,
            body: body
        )
    }

    /// True only while the editable fields differ from what the window opened
    /// with. Generated reply recipients, subject, attribution, and quote are
    /// therefore not mistaken for user-authored draft content.
    var hasUserEdits: Bool {
        currentSnapshot != initialSnapshot || userTouchedAttachments || storedDraftChoicesChanged
    }

    /// The From identity and Markdown choices a reopened draft was stored
    /// with (`init(draft:)` only). They are not in `EditableSnapshot`: on a
    /// fresh window the From is picked automatically after the window opens,
    /// which must not make an untouched reply look edited.
    private var storedDraftChoices: (fromEmail: String?, markdown: Bool, theme: MarkdownThemeChoice)?

    /// True when a reopened draft's From identity, Markdown switch or theme
    /// differs from what was stored. None of them changes the account or the
    /// text, so without this term closing dropped the choice.
    private var storedDraftChoicesChanged: Bool {
        guard let stored = storedDraftChoices else { return false }
        return fromIdentity?.email != stored.fromEmail
            || markdownEnabled != stored.markdown
            || (markdownEnabled && markdownTheme != stored.theme)
    }

    var hasContent: Bool {
        !attachments.isEmpty
            || ![toText, ccText, bccText, subject, body]
                .joined()
                .trimmingCharacters(in: .whitespacesAndNewlines)
                .isEmpty
    }

    /// Decide close-time persistence from both edit state and current content.
    /// An untouched fresh reply is a no-op; an edited non-empty composer saves;
    /// an existing draft edited down to empty is deleted. A Forward window
    /// saves its draft early (to hold the copied attachments), so closing it
    /// untouched deletes that draft rather than leaving an orphan behind.
    var closeAction: ComposeCloseAction {
        guard hasUserEdits else {
            if !openedFromStoredDraft, let draftId { return .deleteDraft(draftId) }
            return .none
        }
        if hasContent { return .save }
        if let draftId { return .deleteDraft(draftId) }
        return .none
    }

    /// Build the NewDraft for sending: requires at least one valid To recipient
    /// and no unparseable entries anywhere. Returns the draft or a user-facing
    /// validation message.
    /// `renderMarkdown` is only ever invoked when `markdownEnabled` is true
    /// (default `{ _ in nil }` so every pre-S46 call site — including every
    /// test that never mentions Markdown — keeps working unchanged, since
    /// `markdownEnabled` itself defaults to `false`). The real caller
    /// (`ComposeView.send()`) passes `session.markdownRenderer(core:)`.
    func buildDraftForSend(
        renderMarkdown: (String) -> String? = { _ in nil }
    ) -> (draft: NewDraft?, problem: String?) {
        let to = AddressParser.parse(toText)
        let cc = AddressParser.parse(ccText)
        let bcc = AddressParser.parse(bccText)
        let invalid = to.invalid + cc.invalid + bcc.invalid
        guard invalid.isEmpty else {
            return (nil, "Not a valid address: \(invalid.joined(separator: ", "))")
        }
        guard !to.addresses.isEmpty else {
            return (nil, "Add at least one To recipient.")
        }
        let unique = AddressDedupe.dedupe(
            to: to.addresses, cc: cc.addresses, bcc: bcc.addresses)
        return (
            makeDraft(to: unique.to, cc: unique.cc, bcc: unique.bcc,
                     renderMarkdown: renderMarkdown),
            nil
        )
    }

    // MARK: forgotten-attachment check (HP5a)

    /// Where the compose preview gets the pictures of this draft's embedded images (MM-5b): the
    /// stored bytes, served through the same locked-down `cid:` handler the reading pane uses.
    /// nil when there are none (every other preview is unchanged).
    func previewImageSource(core: CastellumCore) -> InlineImageSource? {
        let parts = attachments.filter { $0.isInline && $0.contentId != nil }
        guard let draftId, !parts.isEmpty else { return nil }
        let descriptors = parts.map {
            AttachmentDescriptor(
                ordinal: $0.ordinal, filename: $0.filename, contentType: $0.contentType, sizeBytes: $0.sizeBytes,
                section: "", contentId: $0.contentId, isInline: true, transferEncoding: nil)
        }
        let cids = Dictionary(uniqueKeysWithValues: parts.map { ($0.ordinal, $0.contentId ?? "") })
        return InlineImageSource(attachments: descriptors) { ordinal in
            guard let cid = cids[ordinal] else { return nil }
            return core.draftInlineImage(draftId: draftId, contentId: cid)?.data
        }
    }

    // MARK: snippets

    /// What a snippet's placeholders see: the first To address, the current Subject, and the
    /// From identity in use (a chosen alias, else the account's own address and name).
    func snippetContext(
        selfEmail: String, selfName: String?, now: Date = Date(),
        locale: Locale = .current, timeZone: TimeZone = .current
    ) -> TemplateContext {
        let first = AddressParser.parse(toText).addresses.first
        return TemplateContext(
            senderName: nil, senderEmail: nil, sentAt: nil,
            subject: subject.isEmpty ? nil : subject,
            recipientName: first?.name, recipientEmail: first?.email,
            myName: fromIdentity?.name ?? selfName, myEmail: fromIdentity?.email ?? selfEmail,
            now: now, locale: locale, timeZone: timeZone)
    }

    /// The picker's choice: insert `item` with the context this window's account gives it.
    func insertPickedSnippet(_ item: SnippetItem, model: AppModel) {
        insertSnippet(item, context: snippetContext(
            selfEmail: model.selfEmail(for: accountId),
            selfName: model.accounts.first { $0.id == accountId }?.displayName))
    }

    /// Insert `item` at the caret (replacing the selection), with the caret left at its
    /// `${cursor}`. Goes through the body's text view so Cmd-Z removes it in one step;
    /// falls back to editing `body` when the text view cannot be found (a headless test).
    func insertSnippet(_ item: SnippetItem, context: TemplateContext) {
        let rendered = TemplateFormatter.render(item.text, context: context, mode: .multiLine)
        if !bodyHandle.insert(rendered) {
            body = SnippetInsertion.apply(
                rendered, to: body, selection: NSRange(location: NSNotFound, length: 0)).body
        }
        applySnippetSubject(item, context: context)
    }

    /// A snippet's subject fills an EMPTY Subject field and never overwrites one the user
    /// (or a reply's `Re:`) already has. Rendered single-line, so no CR or LF can reach the
    /// header even from a hand-edited library or a hostile value.
    func applySnippetSubject(_ item: SnippetItem, context: TemplateContext) {
        guard subject.trimmingCharacters(in: .whitespaces).isEmpty,
              let format = item.subject, !format.isEmpty else { return }
        let rendered = TemplateFormatter.render(format, context: context, mode: .singleLine).text
        if !rendered.isEmpty { subject = rendered }
    }

    /// The word that suggests an attachment was meant, or nil when the check
    /// is off, a file is already attached, or the user's text has no keyword.
    /// Inline images are attachments here (compose attaches pasted images).
    func forgottenAttachmentWord() -> String? {
        guard preferences.warnAboutForgottenAttachment, attachments.isEmpty else { return nil }
        let scanned = subject == inheritedSubject ? "" : subject
        return ForgottenAttachment.triggerWord(
            subject: scanned, body: body, knownAttribution: generatedAttribution)
    }

    /// The Send action: asks first (publishing `attachmentWarningWord`) when a
    /// forgotten attachment is suspected and the message is otherwise sendable;
    /// otherwise runs `send` straight away (which reports invalid input itself).
    func requestSend(_ send: () -> Void) {
        if let word = forgottenAttachmentWord(), buildDraftForSend().draft != nil {
            attachmentWarningWord = word
        } else {
            send()
        }
    }

    /// "Send Anyway" runs `send`; "Cancel" just dismisses, keeping the draft.
    func resolveAttachmentWarning(sendAnyway: Bool, _ send: () -> Void) {
        attachmentWarningWord = nil
        if sendAnyway { send() }
    }

    /// The renderer for THIS session's chosen theme, for `buildDraftForSend` /
    /// `buildDraftForSave` (captured by value at call time, so a later theme
    /// change can't alter a draft that is already being built).
    ///
    /// An HTML signature replaces its unmodified plain block
    /// in the source rendered here only; the core's compose sanitiser cleans
    /// it with everything else. The text/plain part keeps the plain block.
    func markdownRenderer(core: CastellumCore) -> (String) -> String? {
        let source = markdownSourceTransform()
        let theme = markdownTheme
        // Read `draftId` when the closure RUNS, not when it is built: it is nil until the first
        // save, and an image can only be embedded after that, so a render built early must
        // still see the draft's inline images later (MM-5b).
        return { [self] in core.renderMarkdown(source($0), theme: theme, draftId: draftId) }
    }

    /// `body` -> the Markdown source for the HTML part (see above). Captured
    /// by value, like the theme.
    func markdownSourceTransform() -> (String) -> String {
        let plain = insertedSignature
        let html = insertedSignatureHTML
        return { SignatureHTML.markdownSource(body: $0, plain: plain, html: html) }
    }

    /// Build a lenient NewDraft for silent save-on-close: keeps whatever
    /// parses, never fails (partial drafts are the point of drafts).
    func buildDraftForSave(renderMarkdown: (String) -> String? = { _ in nil }) -> NewDraft {
        makeDraft(to: AddressParser.parse(toText).addresses,
                  cc: AddressParser.parse(ccText).addresses,
                  bcc: AddressParser.parse(bccText).addresses,
                  renderMarkdown: renderMarkdown)
    }

    // MARK: attachments (S33)

    /// Load this draft's existing attachments (a reopened draft). No-op for a
    /// draft that has never been saved (`draftId == nil`).
    func loadAttachments(model: AppModel) {
        guard let draftId else { return }
        let core = model.core
        Task.detached {
            let list = (try? core.listDraftAttachments(draftId: draftId)) ?? []
            await MainActor.run { self.attachments = list }
        }
    }

    /// Attach files from the file panel or a drop. Saves the draft first when
    /// it has never been saved (an attachment row needs a `draft_id`), then
    /// reads and adds each file off the main actor. A file that cannot be read,
    /// or is over 50 MB, is skipped with an inline error; the rest still attach.
    func attach(fileURLs: [URL], model: AppModel) {
        attachmentError = nil
        userTouchedAttachments = true
        // MM-5b (decision M2): with Markdown on, an image is embedded at the caret; anything else,
        // and any image with Markdown off, attaches as before.
        let embedImages = markdownEnabled
        ensureDraftSaved(model: model) { draftId in
            guard let draftId else {
                self.attachmentError = "Couldn't save the draft before attaching."
                return
            }
            let core = model.core
            Task.detached {
                var problems: [String] = []
                var references: [String] = []
                for url in fileURLs {
                    do {
                        let attachment = try ComposeAttachmentBuilder.make(fileURL: url)
                        if embedImages, ComposeInlineImages.looksLikeImage(url) {
                            do {
                                // HEIC / TIFF / BMP become PNG first; the file itself is only attached
                                // (unchanged) if the core still refuses it.
                                let prepared = ComposeInlineImages.preparedForEmbedding(
                                    Data(base64Encoded: attachment.dataBase64) ?? Data(), filename: attachment.filename)
                                let result = try core.embedDraftImage(
                                    draftId: draftId,
                                    image: NewInlineImage(
                                        filename: prepared.filename, alt: nil,
                                        dataBase64: prepared.data.base64EncodedString()))
                                references.append(result.markdown)
                                await MainActor.run { self.attachments = result.attachments }
                                continue
                            } catch {
                                // The core refused (not a raster it can embed, or over 10 MB): attach it normally.
                                problems.append(ComposeInlineImages.fallbackNote(
                                    filename: attachment.filename, reason: "\(error)"))
                            }
                        }
                        let list = try core.addDraftAttachment(draftId: draftId, attachment: attachment)
                        await MainActor.run { self.attachments = list }
                    } catch let error as ComposeAttachmentError {
                        problems.append(error.message)
                    } catch {
                        problems.append("Couldn't attach \(url.lastPathComponent): \(error)")
                    }
                }
                let summary = problems.joined(separator: " ")
                await MainActor.run {
                    self.attachmentError = summary.isEmpty ? nil : summary
                    self.insertEmbeddedReferences(references)
                }
            }
        }
    }

    /// Put the Markdown references of freshly embedded images into the body at the caret (never
    /// inside a signature or quote), one per line.
    func insertEmbeddedReferences(_ references: [String]) {
        guard !references.isEmpty else { return }
        let rendered = RenderedTemplate(text: references.joined(separator: "\n"), caretOffset: nil)
        if !bodyHandle.insert(rendered) {
            body = SnippetInsertion.apply(
                rendered, to: body, selection: NSRange(location: NSNotFound, length: 0)).body
        }
    }

    /// "Insert in Body" on an attachment row (MM-5b slice 3): make it an inline picture and put its
    /// reference at the caret. The core refuses what is not a PNG, JPEG, GIF or WebP within 10 MB;
    /// the row then stays a regular attachment and a one-line note says why.
    func insertAttachmentInBody(id: Int64, model: AppModel) {
        guard let draftId, let row = attachments.first(where: { $0.id == id }), !row.isInline else { return }
        attachmentError = nil
        userTouchedAttachments = true
        let core = model.core
        Task.detached {
            do {
                let result = try core.setDraftAttachmentInline(draftId: draftId, attachmentId: id, inline: true)
                await MainActor.run {
                    self.attachments = result.attachments
                    if let markdown = result.markdown { self.insertEmbeddedReferences([markdown]) }
                }
            } catch {
                await MainActor.run {
                    self.attachmentError = ComposeInlineImages.cannotInsertNote(filename: row.filename, reason: "\(error)")
                }
            }
        }
    }

    /// "Keep as Attachment Only": the picture stops being inline, and its reference leaves the body.
    func keepAttachmentOnly(id: Int64, model: AppModel) {
        guard let draftId, let row = attachments.first(where: { $0.id == id }), row.isInline else { return }
        userTouchedAttachments = true
        if let cid = row.contentId, ComposeInlineImages.references(cid, in: body) {
            body = ComposeInlineImages.removingReferences(to: cid, from: body)
        }
        let core = model.core
        Task.detached {
            do {
                let result = try core.setDraftAttachmentInline(draftId: draftId, attachmentId: id, inline: false)
                await MainActor.run { self.attachments = result.attachments }
            } catch {
                await MainActor.run { self.attachmentError = String(describing: error) }
            }
        }
    }

    /// Embed raw image bytes (a paste, or a generated image from the UI harness) as an inline
    /// image at the caret. Falls back to a regular attachment, with a note, when the core refuses.
    func embedImage(data: Data, filename: String, model: AppModel) {
        attachmentError = nil
        userTouchedAttachments = true
        ensureDraftSaved(model: model) { draftId in
            guard let draftId else {
                self.attachmentError = "Couldn't save the draft before attaching."
                return
            }
            let core = model.core
            Task.detached {
                do {
                    let result = try core.embedDraftImage(
                        draftId: draftId,
                        image: NewInlineImage(filename: filename, alt: nil, dataBase64: data.base64EncodedString()))
                    await MainActor.run {
                        self.attachments = result.attachments
                        self.insertEmbeddedReferences([result.markdown])
                    }
                } catch {
                    let note = ComposeInlineImages.fallbackNote(filename: filename, reason: "\(error)")
                    do {
                        let list = try core.addDraftAttachment(
                            draftId: draftId,
                            attachment: NewDraftAttachment(
                                filename: filename, contentType: "application/octet-stream",
                                isInline: false, contentId: nil, dataBase64: data.base64EncodedString()))
                        await MainActor.run { self.attachments = list; self.attachmentError = note }
                    } catch {
                        await MainActor.run { self.attachmentError = String(describing: error) }
                    }
                }
            }
        }
    }

    /// Paste of raw image data (a screenshot, an image copied from a browser,
    /// with no backing file): always a regular attachment, never inline
    /// (compose is plain text, so there is no `cid:` to reference it from).
    func attachPastedImage(data: Data, contentType: String, model: AppModel) {
        if markdownEnabled {   // MM-5b (M2): embed at the caret
            let ext = UTType(mimeType: contentType)?.preferredFilenameExtension ?? "png"
            embedImage(data: data, filename: "Pasted Image \(Int(Date().timeIntervalSince1970)).\(ext)", model: model)
            return
        }
        attachmentError = nil
        userTouchedAttachments = true
        let attachment: NewDraftAttachment
        do {
            attachment = try ComposeAttachmentBuilder.make(pastedImage: data, contentType: contentType)
        } catch let error as ComposeAttachmentError {
            attachmentError = error.message
            return
        } catch {
            attachmentError = String(describing: error)
            return
        }
        ensureDraftSaved(model: model) { draftId in
            guard let draftId else {
                self.attachmentError = "Couldn't save the draft before attaching."
                return
            }
            let core = model.core
            Task.detached {
                do {
                    let list = try core.addDraftAttachment(draftId: draftId, attachment: attachment)
                    await MainActor.run { self.attachments = list }
                } catch {
                    await MainActor.run { self.attachmentError = String(describing: error) }
                }
            }
        }
    }

    func removeAttachment(id: Int64, model: AppModel) {
        guard let draftId else { return }
        userTouchedAttachments = true
        // MM-5b: removing an embedded image also removes its reference from the body (the
        // change re-renders the preview and is autosaved like any other edit).
        if let removed = attachments.first(where: { $0.id == id }), removed.isInline,
           let cid = removed.contentId, ComposeInlineImages.references(cid, in: body) {
            body = ComposeInlineImages.removingReferences(to: cid, from: body)
        }
        let core = model.core
        Task.detached {
            do {
                let list = try core.removeDraftAttachment(draftId: draftId, attachmentId: id)
                await MainActor.run { self.attachments = list }
            } catch {
                await MainActor.run { self.attachmentError = String(describing: error) }
            }
        }
    }

    /// One-shot: for a Forward window, saves the draft, then copies the
    /// original message's attachments in (downloading any not yet cached).
    /// Safe to call repeatedly: only the first call does anything.
    func copyForwardedAttachmentsIfNeeded(model: AppModel) {
        guard let sourceMessageId = forwardSourceMessageId, !didCopyForwardedAttachments else { return }
        didCopyForwardedAttachments = true
        guard forwardSourceAttachmentCount > 0 else { return }
        ensureDraftSaved(model: model) { draftId in
            guard let draftId, let configJSON = model.storedConfig(for: self.accountId) else {
                self.attachmentError = "Couldn't copy the original's attachments."
                return
            }
            let core = model.core
            let expected = self.forwardSourceAttachmentCount
            Task.detached {
                let list = try? core.forwardAttachments(
                    configJSON: configJSON, messageId: sourceMessageId, draftId: draftId)
                await MainActor.run {
                    if let list { self.attachments = list }
                    if (list?.count ?? 0) < expected {
                        self.attachmentError = "Some of the original's attachments couldn't be copied."
                    }
                }
            }
        }
    }

    /// Calls `completion` immediately when the draft already has an id;
    /// otherwise saves a draft first (the same path close-on-save uses) and
    /// calls `completion` with the new id (`nil` on failure).
    /// `fileprivate` (not `private`): S36's `ComposeView.scheduleServerAutosave`
    /// calls this directly on `session` from a sibling type in this file.
    fileprivate func ensureDraftSaved(model: AppModel, completion: @escaping @MainActor (Int64?) -> Void) {
        // With Markdown on, the first save (triggered by an attachment) must
        // carry the rendered HTML too, or a reopened draft loses Markdown mode.
        let renderer = markdownRenderer(core: model.core)
        ensureDraftSaved(
            save: { [unowned self] done in model.saveDraft(self.buildDraftForSave(renderMarkdown: renderer), completion: done) },
            completion: completion)
    }

    /// Completions waiting on the one in-flight INSERT of this session's
    /// draft row; nil when no creation is in flight (bug hunt 2026-10-12,
    /// #3444). Without it, every keystroke before the first save returned
    /// started its own INSERT and left orphan rows in Drafts.
    private var pendingDraftCreation: [@MainActor (Int64?) -> Void]?

    /// True while the first save of a new draft has not returned yet.
    var isCreatingDraft: Bool { pendingDraftCreation != nil }

    /// The guard itself, with the save injected so tests can hold it open.
    /// `save` must call its argument exactly once with the new id (nil on
    /// failure). At most one `save` runs per session until it reports; every
    /// call made meanwhile is answered with that same id.
    func ensureDraftSaved(
        save: @MainActor (@escaping @MainActor (Int64?) -> Void) -> Void,
        completion: @escaping @MainActor (Int64?) -> Void
    ) {
        if let draftId { completion(draftId); return }
        if pendingDraftCreation != nil {
            pendingDraftCreation?.append(completion)
            return
        }
        pendingDraftCreation = [completion]
        save { [weak self] newId in
            guard let self else { return }
            if let newId { self.draftId = newId }
            let waiting = self.pendingDraftCreation ?? []
            self.pendingDraftCreation = nil
            for done in waiting { done(newId) }
        }
    }

    /// Runs `body` now, or, while the first save of a new draft is still in
    /// flight, right after it reports, so `body` sees the row's id and
    /// updates that row instead of inserting a second one (close, send).
    func afterPendingDraftCreation(_ body: @escaping @MainActor () -> Void) {
        if pendingDraftCreation != nil {
            pendingDraftCreation?.append { _ in body() }
        } else {
            body()
        }
    }

    private func makeDraft(to: [DraftAddress], cc: [DraftAddress], bcc: [DraftAddress],
                           renderMarkdown: (String) -> String? = { _ in nil }) -> NewDraft {
        NewDraft(id: draftId, accountId: accountId, to: to, cc: cc, bcc: bcc,
                 subject: subject, bodyText: body,
                 bodyHtml: markdownEnabled ? renderMarkdown(body) : nil,
                 fromEmail: fromIdentity?.email, fromName: fromIdentity?.name,
                 inReplyTo: inReplyTo, referencesHeader: referencesHeader)
    }
}

struct ComposeView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var session: ComposeSession
    /// Non-nil whenever AI is configured, for both reply and blank-compose
    /// windows — nil only when AI isn't configured. "Draft with AI" is
    /// working either way: a reply drafts against the original message
    /// (`session.originalMessageDbId`); a blank message drafts from a
    /// user-typed instruction gathered via a prompt sheet (see
    /// `AiDraftMode`/`AiDraftSection` below).
    var aiCoordinator: AiCoordinator? = nil

    @ObservedObject private var prefs = Preferences.shared
    @State private var previewHtml = ""

    private var fieldsDisabled: Bool { session.isSending || session.queuedInOutbox }

    private var aiDraftMode: AiDraftMode {
        if let originalMsgId = session.originalMessageDbId {
            return .reply(originalMessageId: originalMsgId)
        }
        return .newMessage(sessionId: session.composeSessionId)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.accounts.count > 1 {
                Picker("From", selection: $session.accountId) {
                    ForEach(model.accounts) { account in
                        Text(account.fromHeader).tag(account.id)
                    }
                }
                .disabled(fieldsDisabled)
            }
            Form {
                addressRow(
                    String(localized: "To"), text: $session.toText,
                    placeholder: String(localized: "name@example.com, …")
                )
                addressRow(String(localized: "Cc"), text: $session.ccText)
                addressRow(String(localized: "Bcc"), text: $session.bccText)
                TextField("Subject", text: $session.subject)
                    .textFieldStyle(.roundedBorder)
            }
            .disabled(fieldsDisabled)
            if let accountIdentities = model.identities[session.accountId], !accountIdentities.isEmpty {
                Picker("Send As", selection: identityPickerSelection) {
                    Text(model.selfEmail(for: session.accountId)).tag(Int64?.none)
                    ForEach(accountIdentities) { identity in
                        Text(identity.displayName.map { "\($0) <\(identity.email)>" } ?? identity.email)
                            .tag(Int64?.some(identity.id))
                    }
                }
                .disabled(fieldsDisabled)
            }

            HStack {
                Toggle("Markdown", isOn: $session.markdownEnabled)
                    .disabled(fieldsDisabled)
                    .help(String(localized: "Send as multipart/alternative: a rendered HTML copy alongside the Markdown source."))
                if session.markdownEnabled {
                    Picker("Theme", selection: $session.markdownTheme) {
                        ForEach(MarkdownThemeChoice.allCases) { Text($0.label).tag($0) }
                    }
                    .fixedSize()
                    .disabled(fieldsDisabled)
                    .help(String(localized: "How the HTML copy looks to the recipient. The plain-text part stays your Markdown source."))
                }
                Spacer()
                Button { session.showSnippetPicker = true } label: { Image(systemName: "text.insert") }
                    .buttonStyle(.borderless)
                    .disabled(fieldsDisabled)
                    .help(String(localized: "Insert Snippet… (⌥⌘I)"))
                    .accessibilityLabel(String(localized: "Insert Snippet…"))
                    .accessibilityIdentifier("compose.insertSnippet")
            }
            HStack(alignment: .top, spacing: 8) {
                TextEditor(text: $session.body)
                    .font(.system(.body, design: session.markdownEnabled ? .monospaced : .default))
                    .markdownSourceTyping(markdown: session.markdownEnabled)
                    .composeBodyHandle(session.bodyHandle)
                    .snippetTabExpansion(session: session, model: model, prefs: prefs)
                    // 80 pt floor (was 180): the AI draft panel below adds ~170 pt, which at the
                    // 560x460 opening size pushed To and Send out of view (RP14).
                    .frame(minHeight: 80, maxHeight: .infinity)
                    .disabled(fieldsDisabled)
                if session.markdownEnabled {
                    EmailBodyWebView(
                        sanitizedHtml: previewHtml,
                        allowRemoteImages: false,
                        onUnrecoverableFailure: {},
                        inlineImages: session.previewImageSource(core: model.core)
                    )
                    .frame(minHeight: 80, maxHeight: .infinity)
                    .background(Color.primary.opacity(0.03))
                }
            }
            .task(id: session.markdownEnabled
                    ? "\(session.markdownTheme.rawValue)|\(session.attachments.map(\.id))|\(session.body)" : "") {
                // Debounced live render: cancelled and restarted by SwiftUI
                // whenever the task id (body text, only while Markdown is
                // on) changes — no manual Combine/Timer bookkeeping needed.
                guard session.markdownEnabled else { return }
                try? await Task.sleep(nanoseconds: 250_000_000)
                guard !Task.isCancelled else { return }
                previewHtml = session.markdownRenderer(core: model.core)(session.body) ?? ""
            }

            ComposeAttachmentsView(model: model, session: session)

            // AI draft section: works for both a reply (drafts against the
            // original message) and a blank message (drafts from a
            // user-typed instruction, via a small prompt sheet).
            if prefs.aiEnabled, let coordinator = aiCoordinator {
                AiDraftSection(
                    mode: aiDraftMode,
                    subject: $session.subject,
                    recipients: $session.toText,
                    composeBody: $session.body
                )
                .environmentObject(coordinator)
            }

            if let error = session.sendError {
                // Queued-in-Outbox captions are pre-composed (with the SMTP
                // endpoint + classified reason) by SendFailurePresentation at
                // the point of failure below; the non-queued `.failed` case
                // passes its reason straight through, same as before.
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Image(systemName: session.queuedInOutbox
                          ? "tray.and.arrow.up" : "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                        .accessibilityHidden(true)
                    Text(error)
                        .font(.caption)
                        .textSelection(.enabled)
                    Spacer()
                }
                .accessibilityElement(children: .combine)
                if session.showsSmtpAuthDisabledHelp {
                    // The caption above already carries the headline and
                    // what the setting is; this adds the Outbox note's link.
                    HStack(spacing: 8) {
                        Text(SmtpAuthDisabledGuidance.outboxNote)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Link(SmtpAuthDisabledGuidance.linkTitle,
                             destination: SmtpAuthDisabledGuidance.helpURL)
                            .font(.caption)
                        Spacer()
                    }
                    .accessibilityIdentifier("compose.smtpAuthDisabledHelp")
                }
            }

            if let notice = session.signatureNotice {
                Text(notice)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            HStack {
                signaturePicker
                if session.queuedInOutbox {
                    Text("This message is queued in the Outbox.")
                        .font(.caption).foregroundStyle(.secondary)
                } else if let draftId = session.draftId,
                          let state = model.serverDraftSaveState[draftId] {
                    // S36: server-mirror status, mirrors UndoSendBanner's minimal styling.
                    switch state {
                    case .idle:
                        EmptyView()
                    case .saving:
                        Text("Saving to server…").font(.caption).foregroundStyle(.secondary)
                    case .saved:
                        Text("Saved to server").font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if session.isSending { ProgressView().controlSize(.small) }
                Button("Send Later") { session.showSendLaterPicker = true }
                    .disabled(fieldsDisabled || !session.hasContent)
                    .help("Choose a time to send this message")
                    .accessibilityIdentifier("compose.sendLater")
                    .popover(isPresented: $session.showSendLaterPicker) {
                        SendLaterPicker(
                            onPick: { date in
                                session.showSendLaterPicker = false
                                requestSend(at: date)
                            },
                            onCancel: { session.showSendLaterPicker = false })
                    }
                Button("Send") { requestSend(at: nil) }
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(fieldsDisabled || !session.hasContent)
                    .help("Send (⌘⏎)")
                    .accessibilityIdentifier("compose.send")
            }
        }
        .padding(16)
        .frame(minWidth: 480)
        .composeAttachmentInput(model: model, session: session)
        .sheet(isPresented: $session.showSnippetPicker) {
            SnippetPickerView(
                snippets: prefs.templateLibrary.snippets(for: session.accountId),
                onPick: { item in
                    session.showSnippetPicker = false
                    // After the sheet is gone the text view is first responder again.
                    DispatchQueue.main.async { session.insertPickedSnippet(item, model: model) }
                },
                onManage: {
                    session.showSnippetPicker = false
                    SnippetEditorWindowController.show(prefs: prefs, model: model)
                },
                onCancel: { session.showSnippetPicker = false })
        }
        // A Forward window copies the original's attachments in once, as soon
        // as it appears (no-op for every other kind of compose window).
        .task { session.copyForwardedAttachmentsIfNeeded(model: model) }
        .onAppear {
            model.reloadIdentities(session.accountId)
            model.reloadAutocompleteContacts()
            applyAutomaticIdentity()
        }
        .onChange(of: session.accountId) { newValue in
            model.reloadIdentities(newValue)
        }
        // Identities load asynchronously after the window opens: choose the
        // starting From (reply-to-the-address-written-to, else the account
        // default) as soon as they are there — once, and never over a choice
        // the user made (MM-6).
        .onChange(of: model.identities[session.accountId]) { _ in applyAutomaticIdentity() }
        // S36: autosave to the server Drafts mailbox on the same edit signal
        // that already drives the local save (subject/body); recipients are
        // read fresh at save time from `session` itself, so a To/Cc/Bcc edit
        // made just before the debounce fires is captured too even though it
        // isn't its own trigger here.
        .onChange(of: session.sendLaterCommand) { preset in
            guard let preset else { return }
            session.sendLaterCommand = nil
            guard session.hasContent, !fieldsDisabled else { return }
            let date = preset.sendDate()
            requestSend(at: date)
        }
        .onChange(of: session.subject) { _ in scheduleServerAutosave() }
        .onChange(of: session.body) { _ in scheduleServerAutosave() }
        // A recipient-only edit must reach the server copy too (#3444). Only
        // once the row exists: recipients alone never create a draft here,
        // the close path decides that.
        .onChange(of: session.toText) { _ in scheduleServerAutosaveIfDraftExists() }
        .onChange(of: session.ccText) { _ in scheduleServerAutosaveIfDraftExists() }
        .onChange(of: session.bccText) { _ in scheduleServerAutosaveIfDraftExists() }
        // Missing-credentials is actionable (re-authenticate in Settings), so
        // it gets its own alert instead of the dead-end inline `sendError`
        // caption.
        .alert(
            "Can't Send This Message",
            isPresented: Binding(
                get: { session.missingCredentialsAccountId != nil },
                set: { if !$0 { session.missingCredentialsAccountId = nil } }
            ),
            presenting: session.missingCredentialsAccountId
        ) { accountId in
            Button("Open Accounts…") {
                session.missingCredentialsAccountId = nil
                model.openAccountsSettings(accountId: accountId)
            }
            Button("Cancel", role: .cancel) {
                session.missingCredentialsAccountId = nil
            }
        } message: { accountId in
            Text(missingCredentialsMessage(accountId: accountId))
        }
        // HP5a: mentions an attachment, has none. Cancel is the default
        // button so a stray Return keeps the draft open.
        .alert(
            "No attachment",
            isPresented: Binding(
                get: { session.attachmentWarningWord != nil },
                set: { if !$0 { session.attachmentWarningWord = nil } }
            ),
            presenting: session.attachmentWarningWord
        ) { _ in
            Button("Send Anyway") {
                session.resolveAttachmentWarning(sendAnyway: true) { send(at: session.pendingSendAt) }
            }
            Button("Cancel", role: .cancel) {
                session.resolveAttachmentWarning(sendAnyway: false) { send(at: session.pendingSendAt) }
            }
            .keyboardShortcut(.defaultAction)
        } message: { word in
            Text(attachmentWarningMessage(word: word))
        }
    }

    /// Footer signature picker: None + the From account's
    /// saved signatures, and a button to edit them. Hidden while the account
    /// has none (nothing to pick; Settings > Accounts adds the first).
    @ViewBuilder
    private var signaturePicker: some View {
        let items = prefs.signatureLibrary.items(for: session.accountId)
        if !items.isEmpty {
            HStack(spacing: 4) {
                Image(systemName: "signature")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Picker("Signature", selection: Binding(
                    get: { session.signatureId },
                    set: { session.chooseSignature($0) })) {
                    Text("No signature").tag(String?.none)
                    ForEach(items) { item in
                        Text(item.name.isEmpty ? String(localized: "Untitled") : item.name)
                            .tag(String?.some(item.id))
                    }
                }
                .labelsHidden()
                .fixedSize()
                .help(String(localized: "Signature for this message"))
                .accessibilityLabel(String(localized: "Signature"))
                .accessibilityIdentifier("compose.signature")
                Button {
                    SignatureEditorWindowController.show(prefs: prefs, model: model, accountId: session.accountId)
                } label: {
                    Image(systemName: "pencil")
                }
                .buttonStyle(.borderless)
                .help(String(localized: "Edit Signatures…"))
                .accessibilityLabel(String(localized: "Edit Signatures…"))
            }
            .disabled(fieldsDisabled)
        }
    }

    private func attachmentWarningMessage(word: String) -> String {
        String(localized: "Your message mentions “\(word)”, but nothing is attached.")
    }

    private func missingCredentialsMessage(accountId: String) -> String {
        String(localized: "No stored credentials for \(model.selfEmail(for: accountId)) — re-authenticate the account in Settings to store SMTP settings again.")
    }

    /// A labelled `AddressAutocompleteField` row for `Form` (mirrors how
    /// `TextField` lays out inside a `Form`'s implicit label column).
    private func addressRow(_ label: String, text: Binding<String>, placeholder: String = "") -> some View {
        HStack {
            Text(label).frame(width: 44, alignment: .trailing)
            AddressAutocompleteField(
                text: text,
                placeholder: placeholder,
                completions: { partial in
                    AddressCompleter.suggestions(
                        for: partial,
                        localContacts: model.autocompleteContacts,
                        systemContacts: SystemContactsSource.shared.entries,
                        sentRecipients: model.autocompleteSentRecipients
                    ).map(\.completionText)
                },
                onFirstInteraction: { SystemContactsSource.shared.prepareIfNeeded() }
            )
            .textFieldStyle(.roundedBorder)
        }
    }

    /// Bridges `session.fromIdentity` (an email/name pair, so `ComposeSession`
    /// never looks up an `Identity` by id) to the `Picker`'s `Int64?` tag space.
    private var identityPickerSelection: Binding<Int64?> {
        Binding(
            get: {
                guard let fromIdentity = session.fromIdentity else { return nil }
                return model.identities[session.accountId]?
                    .first { $0.email == fromIdentity.email }?.id
            },
            set: { newId in
                guard let newId,
                      let identity = model.identities[session.accountId]?.first(where: { $0.id == newId })
                else {
                    session.userChoseFrom(nil)
                    scheduleServerAutosaveIfDraftExists()
                    return
                }
                session.userChoseFrom(ComposeIdentity(email: identity.email, name: identity.displayName))
                scheduleServerAutosaveIfDraftExists()
            }
        )
    }

    /// S36: ensure the local draft row exists (creating it on the very
    /// first edit, exactly like `attachPastedImage`'s use of the same
    /// helper), then (re)start the debounced server-mirror save.
    private func scheduleServerAutosave() {
        session.ensureDraftSaved(model: model) { draftId in
            guard let draftId else { return }
            model.scheduleServerDraftSave(session: session, draftId: draftId)
        }
    }

    /// The From picker and the To/Cc/Bcc fields: mirror the change when the
    /// draft already has a row (or is getting one), never create one.
    private func scheduleServerAutosaveIfDraftExists() {
        guard session.draftId != nil || session.isCreatingDraft else { return }
        scheduleServerAutosave()
    }

    private func applyAutomaticIdentity() {
        guard let identities = model.identities[session.accountId] else { return }
        session.autoSelectIdentity(
            identities: identities, accountEmail: model.selfEmail(for: session.accountId))
    }

    /// Send now (`date == nil`) or schedule for `date`, asking about a
    /// forgotten attachment first in both cases.
    private func requestSend(at date: Date?) {
        session.pendingSendAt = date
        session.requestSend { send(at: date) }
    }

    private func send(at sendAt: Date?) {
        // The first save of a new draft is still in flight: send once it has
        // reported, so this updates that row instead of inserting a second,
        // orphan one (#3444). A second press meanwhile is dropped below.
        if session.isCreatingDraft {
            session.afterPendingDraftCreation { send(at: sendAt) }
            return
        }
        guard !session.isSending else { return }
        session.sendError = nil
        let (built, problem) = session.buildDraftForSend(
            renderMarkdown: session.markdownRenderer(core: model.core)
        )
        guard let draft = built else {
            session.sendError = problem
            return
        }
        session.isSending = true
        // M3 signpost: Send pressed -> draft saved and HANDED OFF to the send
        // path (scheduleDraft / deliverDraftWithUndo). Ended at the hand-off,
        // never in the completion: with undo-send Off that completion waits
        // for SMTP, which must stay out of the interval (PerfSignpost.swift).
        var sendSpan = PerfSignpost.begin(.send)
        model.saveDraft(draft) { id in
            guard let id else {
                PerfSignpost.end(&sendSpan, count: -1)
                session.isSending = false
                session.sendError = "Could not save the draft."
                return
            }
            session.draftId = id
            PerfSignpost.end(&sendSpan)
            // S36: the send itself will clean up any existing server
            // mirror; a debounced autosave that fires after this would just
            // be a benign no-op (the draft has left `draft` status), but
            // there is no reason to let it race the send.
            model.cancelServerDraftSave(draftId: id)
            let handle: @MainActor (AppModel.ComposeSendOutcome) -> Void = { outcome in
                session.isSending = false
                switch outcome {
                case .sent, .queued:
                    // `.queued` (S25 undo-send window) closes the compose
                    // window exactly like `.sent` — the main window's banner
                    // takes over from here. Undo (the banner's button or ⌘Z)
                    // reopens THIS draft, unsent, in a new compose window
                    // (`AppModel.undoSend` -> `reopenDraftForEditing`); if it
                    // loses the race — the sender already claimed it — a
                    // short non-modal notice explains that instead, nothing
                    // reopens.
                    session.suppressSaveOnClose = true
                    session.requestClose?()
                case .scheduled:
                    // HP1 Send Later: held in the durable Outbox until its
                    // time; the compose window closes like a real send. The
                    // Outbox lists it with Cancel / Send Now / Reschedule.
                    session.suppressSaveOnClose = true
                    session.requestClose?()
                case .leftInOutbox(let reason):
                    // Durably queued: content is safe, but no longer editable.
                    session.queuedInOutbox = true
                    session.suppressSaveOnClose = true
                    let presentation = SendFailurePresentation.composePresentation(
                        rawReason: reason,
                        endpoint: model.smtpEndpoint(for: draft.accountId)
                    )
                    session.sendError = presentation.caption
                    session.showsSmtpAuthDisabledHelp = presentation.showsSmtpAuthDisabledHelp
                case .failed(let reason):
                    session.sendError = reason
                    // WS1 (#2964): when this failure is the read-only refusal
                    // (the model has just raised its dialog for this account),
                    // "Enable Writes" re-runs THIS Send — rebuilt from the
                    // window as it is then, not from the draft as it was — so
                    // the fix is one click, on this window. A no-op for any
                    // other failure: no refusal is pending to attach to.
                    model.registerWriteSafetyRetry(accountId: draft.accountId) {
                        send(at: sendAt)
                    }
                case .missingCredentials(let accountId):
                    session.missingCredentialsAccountId = accountId
                }
            }
            if let sendAt {
                model.scheduleDraft(
                    id: id, accountId: draft.accountId, sendAt: sendAt, completion: handle)
            } else {
                model.deliverDraftWithUndo(
                    id: id, accountId: draft.accountId, completion: handle)
            }
        }
    }
}

// MARK: - AI Draft Section

/// Compact AI compose bar shown below the body editor. Manages an
/// `AiCoordinator` job whose feature/owner id come from `mode`: `.reply`
/// drafts against an original message; `.newMessage` drafts a blank compose
/// window from a user-typed instruction gathered via a small prompt sheet.
private struct AiDraftSection: View {
    let mode: AiDraftMode
    /// Live compose-window fields, needed only by `.newMessage` to give the
    /// prompt builder addressing context — a reply already has this from the
    /// original message it looked up server-side.
    @Binding var subject: String
    @Binding var recipients: String
    @Binding var composeBody: String

    @EnvironmentObject private var coordinator: AiCoordinator
    @ObservedObject private var prefs = Preferences.shared
    /// Inline guidance for a reply draft (optional).
    @State private var instruction = ""
    /// "What should this email say?" text for a new-message draft (required).
    @State private var messagePrompt = ""
    @State private var showingPromptSheet = false

    /// Live state: look up by ownerId + feature so that after `clear()` the
    /// section snaps back to idle automatically (no local job-id state needed).
    private var state: AiJobViewState? {
        coordinator.jobState(forMessageId: mode.ownerId, feature: mode.feature)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Divider()

            if let s = state {
                switch s.status {
                case "queued", "leased", "running", "streaming":
                    runningView(state: s)
                case "succeeded":
                    succeededView(state: s)
                case "cancelled":
                    cancelledView(state: s)
                default:   // "failed"
                    failedView(state: s)
                }
            } else {
                idleView
            }
        }
        .padding(.vertical, 4)
        .sheet(isPresented: $showingPromptSheet) {
            newMessagePromptSheet
        }
    }

    // MARK: - Sub-views

    @ViewBuilder
    private var idleView: some View {
        switch mode {
        case .reply:
            HStack(spacing: 8) {
                TextField(
                    String(localized: "Optional: guidance, e.g. 'accept and propose Thursday'"),
                    text: $instruction
                )
                .textFieldStyle(.roundedBorder)
                .font(.callout)
                Button("Draft with AI") { startReplyDraft() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            }
        case .newMessage:
            HStack(spacing: 8) {
                Spacer()
                Button("Draft with AI") {
                    messagePrompt = ""
                    showingPromptSheet = true
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
            }
        }
    }

    /// "What should this email say?" sheet for a blank compose window
    ///. Cancel discards the text; Draft enqueues the job and
    /// the section switches to the same running/succeeded views a reply uses.
    private var newMessagePromptSheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("What should this email say?")
                .font(.headline)
            TextEditor(text: $messagePrompt)
                .font(.body)
                .frame(minWidth: 320, minHeight: 120)
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .stroke(Color.secondary.opacity(0.3))
                )
            HStack {
                Spacer()
                Button("Cancel") { showingPromptSheet = false }
                Button("Draft") {
                    showingPromptSheet = false
                    startNewMessageDraft()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(AiDraftInstruction.normalised(messagePrompt) == nil)
            }
        }
        .padding(20)
        .frame(minWidth: 360)
    }

    private func runningView(state: AiJobViewState) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Drafting…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Cancel") { coordinator.cancel(jobId: state.id) }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
            if !state.text.isEmpty {
                suggestionBox(text: state.text)
            }
        }
    }

    private func succeededView(state: AiJobViewState) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("AI draft ready", systemImage: "sparkles")
                .font(.caption2)
                .foregroundStyle(.tertiary)
            suggestionBox(text: state.text)
            HStack(spacing: 8) {
                Button("Insert") { insert(text: state.text, jobId: state.id) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                Button("Discard") { discard(jobId: state.id) }
                    .buttonStyle(.borderless)
                    .controlSize(.small)
                Spacer()
            }
        }
    }

    private func cancelledView(state: AiJobViewState) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("Stopped early", systemImage: "stop.circle")
                .font(.caption)
                .foregroundStyle(.secondary)
            if !state.text.isEmpty {
                suggestionBox(text: state.text)
            }
            HStack(spacing: 8) {
                if !state.text.isEmpty {
                    Button("Insert anyway") { insert(text: state.text, jobId: state.id) }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                }
                Button("Discard") { discard(jobId: state.id) }
                    .buttonStyle(.borderless)
                    .controlSize(.small)
                Spacer()
            }
        }
    }

    private func failedView(state: AiJobViewState) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Label(
                    AiEventReducer.errorMessage(code: state.errorCode, modelName: prefs.aiModel),
                    systemImage: "exclamationmark.triangle"
                )
                .font(.caption)
                .foregroundStyle(.red)
                Spacer()
                Button("Retry") { discard(jobId: state.id) }
                    .buttonStyle(.borderless)
                    .font(.caption)
            }
            if state.errorCode == "model_missing" {
                Text("Run `ollama pull \(prefs.aiModel)` on the endpoint to install the model.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// Read-only suggestion box with a faint background and text selection.
    private func suggestionBox(text: String) -> some View {
        ScrollView {
            Text(text)
                .font(.callout)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(6)
        }
        .frame(maxHeight: 90)
        .background(Color.primary.opacity(0.05))
        .clipShape(RoundedRectangle(cornerRadius: 4))
    }

    // MARK: - Actions

    private func startReplyDraft() {
        guard case .reply(let originalMessageId) = mode else { return }
        // Trim before sending, not just before the is-empty test: the untrimmed
        // string was what reached the prompt builder, so leading/trailing
        // whitespace and stray newlines went into the model's context verbatim.
        coordinator.draftReply(
            messageId: originalMessageId,
            instruction: AiDraftInstruction.normalised(instruction)
        )
    }

    private func startNewMessageDraft() {
        guard case .newMessage(let sessionId) = mode,
              let instr = AiDraftInstruction.normalised(messagePrompt)
        else { return }
        coordinator.draftMessage(
            sessionId: sessionId,
            instruction: instr,
            subject: subject,
            recipients: recipients,
            existingBody: AiDraftInstruction.normalised(composeBody)
        )
    }

    private func insert(text: String, jobId: Int64) {
        composeBody = ReplyPrefill.insertAboveQuote(suggestion: text, in: composeBody)
        coordinator.clear(jobId: jobId)
    }

    private func discard(jobId: Int64) {
        coordinator.clear(jobId: jobId)
    }
}
