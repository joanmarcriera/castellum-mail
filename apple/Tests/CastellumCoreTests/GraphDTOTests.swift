// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Proves the Swift graph DTOs match the Rust core's JSON shapes exactly
/// (snake_case, optionality) and that the layout engine is deterministic.
final class GraphDTOTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    // MARK: - DTO shape tests

    func testGraphNodeDecodes() throws {
        let json = """
        {"id":42,"label":"Alice","message_count":17}
        """
        let node = try decode(GraphNode.self, json)
        XCTAssertEqual(node.id, 42)
        XCTAssertEqual(node.label, "Alice")
        XCTAssertEqual(node.messageCount, 17)
    }

    func testGraphNodeFallsBackToEmailLabel() throws {
        // When the core produces a label = email (no name) we can decode it fine.
        let json = """
        {"id":99,"label":"noname@x.test","message_count":0}
        """
        let node = try decode(GraphNode.self, json)
        XCTAssertEqual(node.label, "noname@x.test")
        XCTAssertEqual(node.messageCount, 0)
    }

    func testGraphEdgeDecodes() throws {
        let json = """
        {"a":10,"b":20,"weight":2.75}
        """
        let edge = try decode(GraphEdge.self, json)
        XCTAssertEqual(edge.a, 10)
        XCTAssertEqual(edge.b, 20)
        XCTAssertEqual(edge.weight, 2.75, accuracy: 1e-9)
    }

    func testContactGraphDecodes() throws {
        let json = """
        {"nodes":[{"id":1,"label":"Alice","message_count":5},
                  {"id":2,"label":"Bob","message_count":3}],
         "edges":[{"a":1,"b":2,"weight":1.0}]}
        """
        let g = try decode(ContactGraph.self, json)
        XCTAssertEqual(g.nodes.count, 2)
        XCTAssertEqual(g.edges.count, 1)
        XCTAssertEqual(g.nodes[0].id, 1)
        XCTAssertEqual(g.nodes[1].id, 2)
        XCTAssertEqual(g.edges[0].a, 1)
        XCTAssertEqual(g.edges[0].b, 2)
        XCTAssertEqual(g.edges[0].weight, 1.0, accuracy: 1e-9)
    }

    func testContactGraphEmptyCollections() throws {
        let json = """
        {"nodes":[],"edges":[]}
        """
        let g = try decode(ContactGraph.self, json)
        XCTAssertTrue(g.nodes.isEmpty)
        XCTAssertTrue(g.edges.isEmpty)
    }

    // MARK: - Layout determinism

    func testLayoutDeterminism() {
        let nodes = [
            GraphNode(id: 1, label: "Alice", messageCount: 10),
            GraphNode(id: 2, label: "Bob",   messageCount: 5),
            GraphNode(id: 3, label: "Carol",  messageCount: 3),
        ]
        let edges = [
            GraphEdge(a: 1, b: 2, weight: 1.0),
            GraphEdge(a: 1, b: 3, weight: 0.5),
        ]
        let size = CGSize(width: 800, height: 600)

        let pos1 = ContactGraphView.layout(nodes: nodes, edges: edges, size: size)
        let pos2 = ContactGraphView.layout(nodes: nodes, edges: edges, size: size)

        XCTAssertEqual(pos1.count, 3)
        XCTAssertEqual(pos2.count, 3)

        for id in nodes.map(\.id) {
            guard let p1 = pos1[id], let p2 = pos2[id] else {
                XCTFail("Missing position for node \(id)")
                continue
            }
            XCTAssertEqual(p1.x, p2.x, accuracy: 0.01,
                           "x of node \(id) must be deterministic")
            XCTAssertEqual(p1.y, p2.y, accuracy: 0.01,
                           "y of node \(id) must be deterministic")
        }
    }

    func testLayoutEmptyGraph() {
        let pts = ContactGraphView.layout(nodes: [], edges: [], size: CGSize(width: 800, height: 600))
        XCTAssertTrue(pts.isEmpty)
    }

    func testLayoutNodePositionsWithinBounds() {
        let nodes = (1 ... 10).map {
            GraphNode(id: Int64($0), label: "N\($0)", messageCount: Int64($0))
        }
        let edges = (1 ..< 10).map { GraphEdge(a: Int64($0), b: Int64($0 + 1), weight: 1.0) }
        let size = CGSize(width: 800, height: 600)
        let pts = ContactGraphView.layout(nodes: nodes, edges: edges, size: size)

        // The layout clamps every position to [padding, dim - padding] with a
        // 40pt padding — assert the actual clamp, not just the canvas bounds.
        let padding: CGFloat = 40
        for (id, pt) in pts {
            XCTAssertGreaterThanOrEqual(pt.x, padding, "Node \(id) x must be >= padding")
            XCTAssertLessThanOrEqual(pt.x, size.width - padding, "Node \(id) x must be <= width - padding")
            XCTAssertGreaterThanOrEqual(pt.y, padding, "Node \(id) y must be >= padding")
            XCTAssertLessThanOrEqual(pt.y, size.height - padding, "Node \(id) y must be <= height - padding")
        }
    }
}
