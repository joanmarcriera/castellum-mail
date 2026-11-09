// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import AppKit
import XCTest
@testable import Castellum

@MainActor
final class ProjectUXSupportTests: XCTestCase {
    func testNewProjectDraftRequiresANonWhitespaceName() {
        XCTAssertFalse(NewProjectDraft(name: " \n ").canCreate)

        let draft = NewProjectDraft(name: "  Client launch  ", notes: "Scope")
        XCTAssertTrue(draft.canCreate)
        XCTAssertEqual(draft.trimmedName, "Client launch")
    }

    func testAssignmentKeyboardNavigationIncludesCreateWhenThereAreNoProjects() {
        let choices = ProjectAssignmentKeyboardNavigation.choices(projectIds: [])
        XCTAssertEqual(choices, [.createProject])
        XCTAssertEqual(
            ProjectAssignmentKeyboardNavigation.move(
                from: nil, direction: .down, choices: choices
            ),
            .createProject
        )
    }

    func testAssignmentKeyboardNavigationMovesBetweenProjectsAndCreate() {
        let choices = ProjectAssignmentKeyboardNavigation.choices(projectIds: ["a", "b"])
        XCTAssertEqual(choices, [.project("a"), .project("b"), .createProject])
        XCTAssertEqual(
            ProjectAssignmentKeyboardNavigation.move(
                from: .project("a"), direction: .down, choices: choices
            ),
            .project("b")
        )
        XCTAssertEqual(
            ProjectAssignmentKeyboardNavigation.move(
                from: .project("b"), direction: .down, choices: choices
            ),
            .createProject
        )
        XCTAssertEqual(
            ProjectAssignmentKeyboardNavigation.move(
                from: .createProject, direction: .up, choices: choices
            ),
            .project("b")
        )
    }

    func testProjectsWindowHasUsefulDefaultAndMinimumSizes() {
        XCTAssertGreaterThanOrEqual(ProjectsBoardWindowController.defaultContentSize.width, 900)
        XCTAssertGreaterThanOrEqual(ProjectsBoardWindowController.defaultContentSize.height, 600)
        XCTAssertGreaterThanOrEqual(ProjectsBoardWindowController.minimumContentSize.width, 800)
        XCTAssertGreaterThanOrEqual(ProjectsBoardWindowController.minimumContentSize.height, 500)
    }

    func testLinkPeopleSuggestionsExcludePeopleAlreadyInTheProject() {
        let linked = Contact(id: 1, email: "linked@example.test", derivedName: "Linked",
                             displayName: nil, notes: "", isHidden: false,
                             isSelf: false, isUserCreated: false,
                             firstSeenAt: 1, lastSeenAt: 2,
                             inboundCount: 2, outboundCount: 0)
        let available = Contact(id: 2, email: "available@example.test", derivedName: "Available",
                                displayName: nil, notes: "", isHidden: false,
                                isSelf: false, isUserCreated: false,
                                firstSeenAt: 1, lastSeenAt: 2,
                                inboundCount: 1, outboundCount: 0)

        let suggestions = LinkPeopleSuggestions.filtered(
            contacts: [linked, available],
            query: "",
            excluding: [linked.id]
        )

        XCTAssertEqual(suggestions.map(\.id), [available.id])
    }
}
