// Castellum - application menus generated from the command registry.
// CommandRegistry is the single source of truth; this file only maps specs
// onto SwiftUI's Commands structure (placement, dividers, key equivalents).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

extension MenuChord {
    /// SwiftUI key equivalent for this chord ("⌫" is the delete key).
    var keyEquivalent: KeyEquivalent {
        key == "⌫" ? .delete : KeyEquivalent(Character(key))
    }

    var eventModifiers: EventModifiers {
        var mods: EventModifiers = []
        if control { mods.insert(.control) }
        if option { mods.insert(.option) }
        if shift { mods.insert(.shift) }
        if command { mods.insert(.command) }
        return mods
    }
}

extension CommandRegistry {
    /// Menu content for one menu, grouped into sections (divider between
    /// sections). Registry order is preserved within each section. Defaults
    /// to the built-in table; production passes the live effective specs
    /// so a user override actually takes effect in the menu bar (S45).
    static func menuSections(
        for menu: MenuName, in specs: [CommandSpec] = all
    ) -> [[CommandSpec]] {
        let members = specs.filter { $0.menu?.name == menu }
        let grouped = Dictionary(grouping: members) { $0.menu!.section }
        return grouped.keys.sorted().map { grouped[$0]! }
    }
}

/// The app's menu bar, driven entirely by the registry. Placement mirrors
/// the previous hand-written blocks: File replaces .newItem, View sits after
/// .toolbar, Help replaces .help, the rest are top-level command menus in
/// declaration order. `specs` is `AppModel.keyBindings.effectiveSpecs` —
/// add or change commands in CommandRegistry, never here (CommandRegistryTests
/// pins the structure, chords, and conflict-freedom; KeyBindingsStoreTests
/// pins the override layer this reads through).
struct RegistryCommands: Commands {
    let specs: [CommandSpec]
    /// Commands shown greyed out right now (e.g. mailbox-wide actions while a
    /// Correspondence view hides the selected mailbox).
    var disabled: Set<CommandID> = []
    /// Radio-style commands that currently apply; shown with a checkmark.
    var checked: Set<CommandID> = []

    var body: some Commands {
        CommandGroup(after: .appInfo) { MenuItems(menu: .app, specs: specs, disabled: disabled, checked: checked) }
        CommandGroup(replacing: .newItem) { MenuItems(menu: .file, specs: specs, disabled: disabled, checked: checked) }
        CommandMenu("Search") { MenuItems(menu: .search, specs: specs, disabled: disabled, checked: checked) }
        CommandMenu("Message") { MenuItems(menu: .message, specs: specs, disabled: disabled, checked: checked) }
        CommandGroup(after: .toolbar) { MenuItems(menu: .view, specs: specs, disabled: disabled, checked: checked) }
        CommandGroup(after: .windowArrangement) { MenuItems(menu: .window, specs: specs, disabled: disabled, checked: checked) }
        CommandMenu("People") { MenuItems(menu: .people, specs: specs, disabled: disabled, checked: checked) }
        CommandMenu("Projects") { MenuItems(menu: .projects, specs: specs, disabled: disabled, checked: checked) }
        CommandGroup(replacing: .help) { MenuItems(menu: .help, specs: specs, disabled: disabled, checked: checked) }
    }
}

/// Commands that are choices between alternatives (View > layout): their menu
/// item carries a checkmark for the one in force.
private let checkable: Set<CommandID> = [
    .layoutWidescreen, .layoutClassic, .headerCompact, .headerExtended, .headerAll, .toggleRemoteImages,
]

/// The buttons and dividers for one registry-driven menu.
private struct MenuItems: View {
    let menu: MenuName
    let specs: [CommandSpec]
    let disabled: Set<CommandID>
    let checked: Set<CommandID>

    var body: some View {
        let sections = CommandRegistry.menuSections(for: menu, in: specs)
        ForEach(Array(sections.enumerated()), id: \.offset) { index, section in
            if index > 0 { Divider() }
            ForEach(section, id: \.id.rawValue) { spec in
                CommandButton(spec: spec, isDisabled: disabled.contains(spec.id), checked: checkable.contains(spec.id) ? checked.contains(spec.id) : nil)
            }
        }
    }
}

private struct CommandButton: View {
    let spec: CommandSpec
    let isDisabled: Bool
    /// Non-nil for a checkable item: whether it is the current choice.
    let checked: Bool?

    private func fire() {
        if let notification = spec.notification {
            NotificationCenter.default.post(name: notification, object: nil)
        }
    }

    var body: some View {
        let item = Group {
            if let checked {
                Toggle(spec.title, isOn: Binding(get: { checked }, set: { _ in fire() }))
            } else {
                Button(spec.title, action: fire)
            }
        }
        .disabled(isDisabled)
        if let chord = spec.menuChord {
            item.keyboardShortcut(chord.keyEquivalent, modifiers: chord.eventModifiers)
        } else {
            item
        }
    }
}

/// Which checkable menu items are ticked, from the current preferences (pure, so it is tested).
enum MenuChecks {
    static func checked(layout: ViewerLayout, headerMode: MessageHeaderMode, loadsRemoteImages: Bool) -> Set<CommandID> {
        var out: Set<CommandID> = [layout == .widescreen ? .layoutWidescreen : .layoutClassic]
        switch headerMode {
        case .compact: out.insert(.headerCompact)
        case .extended: out.insert(.headerExtended)
        case .all: out.insert(.headerAll)
        }
        if loadsRemoteImages { out.insert(.toggleRemoteImages) }
        return out
    }
}
