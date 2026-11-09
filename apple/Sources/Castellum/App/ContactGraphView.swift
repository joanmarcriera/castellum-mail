// Castellum - People network graph view (slice 8c).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Static Fruchterman–Reingold layout (300 iters, deterministic seed from node
// ids). Computed off-main in Task.detached; stored positions drive a Canvas
// that draws edges first (width/opacity scaled by weight) then nodes (radius
// ∝ log(message_count+1)) + labels for the top-20 by size. Tap hit-tests
// nearest node within radius+4pt and calls onSelectContact(id).

import SwiftUI

// MARK: - Layout engine (pure, testable)

extension ContactGraphView {
    /// Fruchterman–Reingold force-directed layout.
    ///
    /// - Parameters:
    ///   - nodes: Graph nodes carrying their id.
    ///   - edges: Graph edges (a,b,weight).
    ///   - size:  Canvas size to lay out within.
    /// - Returns: Map from node id to 2D position (in canvas coordinates).
    ///
    /// The seed is deterministic: derived from a sorted hash of node ids so
    /// the same graph data always produces the same layout.
    static func layout(
        nodes: [GraphNode],
        edges: [GraphEdge],
        size: CGSize
    ) -> [Int64: CGPoint] {
        guard !nodes.isEmpty else { return [:] }

        let w = Double(max(size.width, 100))
        let h = Double(max(size.height, 100))
        let n = Double(nodes.count)
        // Fruchterman-Reingold spring constant.
        let k = sqrt((w * h) / max(n, 1))
        let iterations = 300
        let padding: Double = 40

        // Deterministic initial positions: seed from sorted node ids.
        var seed: UInt64 = nodes.map(\.id).sorted().reduce(UInt64(1783083704)) { acc, id in
            acc &* 6364136223846793005 &+ UInt64(bitPattern: id) &+ 1442695040888963407
        }
        func nextRand() -> Double {
            seed = seed &* 6364136223846793005 &+ 1442695040888963407
            return Double(seed >> 11) / Double(1 << 53)
        }

        // Index nodes for O(1) edge lookup.
        let idxById: [Int64: Int] = Dictionary(
            uniqueKeysWithValues: nodes.enumerated().map { ($1.id, $0) }
        )

        var px = nodes.map { _ in nextRand() * (w - 2 * padding) + padding }
        var py = nodes.map { _ in nextRand() * (h - 2 * padding) + padding }
        var dx = [Double](repeating: 0, count: nodes.count)
        var dy = [Double](repeating: 0, count: nodes.count)

        // Cooling schedule: temperature decreases linearly.
        let tInit = 0.1 * max(w, h)

        for iter in 0 ..< iterations {
            let t = tInit * (1.0 - Double(iter) / Double(iterations))

            // Repulsion: every pair.
            for i in 0 ..< nodes.count {
                dx[i] = 0; dy[i] = 0
                for j in 0 ..< nodes.count where i != j {
                    let ddx = px[i] - px[j]
                    let ddy = py[i] - py[j]
                    let dist = max(sqrt(ddx * ddx + ddy * ddy), 0.01)
                    let f = (k * k) / dist
                    dx[i] += (ddx / dist) * f
                    dy[i] += (ddy / dist) * f
                }
            }

            // Attraction: edges pull nodes together.
            for e in edges {
                guard let i = idxById[e.a], let j = idxById[e.b] else { continue }
                let ddx = px[j] - px[i]
                let ddy = py[j] - py[i]
                let dist = max(sqrt(ddx * ddx + ddy * ddy), 0.01)
                let f = (dist * dist) / k * (1.0 + e.weight)
                let fx = (ddx / dist) * f
                let fy = (ddy / dist) * f
                dx[i] += fx; dy[i] += fy
                dx[j] -= fx; dy[j] -= fy
            }

            // Apply displacement, capped to temperature; clamp to bounds.
            for i in 0 ..< nodes.count {
                let dispLen = max(sqrt(dx[i] * dx[i] + dy[i] * dy[i]), 0.01)
                let cap = min(dispLen, t) / dispLen
                px[i] = (px[i] + dx[i] * cap).clamped(to: padding ... (w - padding))
                py[i] = (py[i] + dy[i] * cap).clamped(to: padding ... (h - padding))
            }
        }

        var result: [Int64: CGPoint] = [:]
        for (i, node) in nodes.enumerated() {
            result[node.id] = CGPoint(x: px[i], y: py[i])
        }
        return result
    }
}

private extension Double {
    func clamped(to range: ClosedRange<Double>) -> Double {
        Swift.max(range.lowerBound, Swift.min(self, range.upperBound))
    }
}

// MARK: - Main view

struct ContactGraphView: View {
    @ObservedObject var model: AppModel
    /// When non-nil, the graph is filtered to this project's contacts.
    var projectId: String?
    /// Called when the user taps a node; switches the people tab + selects contact.
    var onSelectContact: (Int64) -> Void = { _ in }

    // Controls
    @State private var maxNodes: Int = 100
    @State private var minWeightFraction: Double = 0.0  // fraction of max edge weight

    // Data state
    @State private var graph: ContactGraph = ContactGraph(nodes: [], edges: [])
    @State private var positions: [Int64: CGPoint] = [:]
    @State private var isLoading = false
    @State private var canvasSize: CGSize = CGSize(width: 800, height: 600)

    // Project filter chip
    @State private var projectName: String?

    private let maxNodesOptions = [25, 50, 100, 200]

    private var filteredEdges: [GraphEdge] {
        guard !graph.edges.isEmpty else { return [] }
        let maxW = graph.edges.map(\.weight).max() ?? 1.0
        let threshold = minWeightFraction * maxW
        return graph.edges.filter { $0.weight >= threshold }
    }

    var body: some View {
        VStack(spacing: 0) {
            toolbar
            Divider()
            if isLoading {
                Spacer()
                ProgressView("Computing layout…")
                Spacer()
            } else if graph.nodes.isEmpty {
                Spacer()
                Text("Sync more mail or refresh contacts")
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding()
                Spacer()
            } else {
                // Lone nodes (no edges) still render — the canvas just has
                // nothing to connect them with.
                graphCanvas
            }
        }
        .task(id: canvasTaskKey) {
            await loadGraph()
        }
    }

    // A stable task key so we reload when the project, maxNodes, or the
    // canvas size (bucketed to 50pt so live-resize doesn't thrash the
    // layout task) changes.
    private var canvasTaskKey: String {
        let bw = Int(canvasSize.width / 50)
        let bh = Int(canvasSize.height / 50)
        return "\(projectId ?? "nil")-\(maxNodes)-\(bw)x\(bh)"
    }

    // MARK: - Toolbar

    private var toolbar: some View {
        HStack(spacing: 12) {
            // Project filter chip
            if let name = projectName {
                HStack(spacing: 4) {
                    Text("Project: \(name)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button {
                        // Caller should clear projectId; here we just announce
                        // via onSelectContact(-1) as a sentinel for "clear filter".
                        // In practice the caller wires this differently.
                        // For the standalone embed in ContactsView, projectId is
                        // driven by external state, so just send the sentinel.
                        onSelectContact(-1)
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                            .foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .help("Clear project filter")
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .background(Capsule().fill(Color(nsColor: .controlBackgroundColor)))
                .overlay(Capsule().stroke(Color(nsColor: .separatorColor), lineWidth: 0.5))
            }

            Spacer()

            // Min-weight slider
            if !graph.edges.isEmpty {
                HStack(spacing: 6) {
                    Text("Min weight:")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Slider(value: $minWeightFraction, in: 0 ... 1)
                        .frame(width: 100)
                        .help("Filter out low-weight edges")
                }
            }

            // Top-N stepper
            HStack(spacing: 4) {
                Text("Top:")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Stepper("\(maxNodes)", onIncrement: {
                    if let idx = maxNodesOptions.firstIndex(of: maxNodes), idx + 1 < maxNodesOptions.count {
                        maxNodes = maxNodesOptions[idx + 1]
                    }
                }, onDecrement: {
                    if let idx = maxNodesOptions.firstIndex(of: maxNodes), idx > 0 {
                        maxNodes = maxNodesOptions[idx - 1]
                    }
                })
                .help("Number of contacts to show")
            }

            if isLoading {
                ProgressView().controlSize(.small)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    // MARK: - Canvas

    private var graphCanvas: some View {
        GeometryReader { geo in
            Canvas { ctx, size in
                // Update canvas size for hit testing.
                let edges = filteredEdges
                guard !positions.isEmpty else { return }

                // Compute normalisation range for edge weights.
                let weights = edges.map(\.weight)
                let minW = weights.min() ?? 0
                let maxW = weights.max() ?? 1

                func normWeight(_ w: Double) -> Double {
                    guard maxW > minW else { return 0.5 }
                    return (w - minW) / (maxW - minW)
                }

                // 1. Draw edges first.
                for edge in edges {
                    guard let pa = positions[edge.a], let pb = positions[edge.b] else { continue }
                    let nw = normWeight(edge.weight)
                    let lineWidth = 0.5 + nw * 2.5  // [0.5, 3.0]
                    let opacity = 0.2 + nw * 0.5     // [0.2, 0.7]
                    var path = Path()
                    path.move(to: pa)
                    path.addLine(to: pb)
                    ctx.stroke(path,
                               with: .color(.secondary.opacity(opacity)),
                               lineWidth: lineWidth)
                }

                // 2. Determine which node ids to label (top-20 by messageCount).
                let labelSet: Set<Int64> = Set(
                    graph.nodes
                        .sorted { $0.messageCount > $1.messageCount }
                        .prefix(20)
                        .map(\.id)
                )

                // 3. Draw nodes.
                for node in graph.nodes {
                    guard let pt = positions[node.id] else { continue }
                    let r = nodeRadius(node.messageCount)
                    let rect = CGRect(x: pt.x - r, y: pt.y - r, width: r * 2, height: r * 2)
                    ctx.fill(Path(ellipseIn: rect), with: .color(.accentColor.opacity(0.8)))
                    ctx.stroke(Path(ellipseIn: rect), with: .color(.white.opacity(0.6)), lineWidth: 1)

                    // Label (only for top-20 by message count).
                    if labelSet.contains(node.id) {
                        let label = Text(node.label)
                            .font(.system(size: 10))
                            .foregroundColor(.primary)
                        let resolved = ctx.resolve(label)
                        let lw = resolved.measure(in: size).width
                        ctx.draw(resolved,
                                 at: CGPoint(x: pt.x - lw / 2, y: pt.y + r + 3))
                    }
                }
            }
            .gesture(
                SpatialTapGesture()
                    .onEnded { value in
                        hitTest(at: value.location)
                    }
            )
            .onChange(of: geo.size) { newSize in
                if canvasSize != newSize {
                    canvasSize = newSize
                }
            }
            .onAppear {
                canvasSize = geo.size
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - Helpers

    private func nodeRadius(_ count: Int64) -> CGFloat {
        let r = log(Double(count) + 1) * 6.0
        return CGFloat(r.clamped(to: 6 ... 28))
    }

    private func hitTest(at point: CGPoint) {
        var nearest: Int64?
        var nearestDist = Double.infinity
        for node in graph.nodes {
            guard let pt = positions[node.id] else { continue }
            let r = nodeRadius(node.messageCount)
            let dx = Double(point.x - pt.x)
            let dy = Double(point.y - pt.y)
            let dist = sqrt(dx * dx + dy * dy)
            if dist <= Double(r) + 4, dist < nearestDist {
                nearestDist = dist
                nearest = node.id
            }
        }
        if let id = nearest {
            onSelectContact(id)
        }
    }

    // MARK: - Data loading

    @MainActor
    private func loadGraph() async {
        isLoading = true
        defer { isLoading = false }

        // Resolve project name for the filter chip.
        if let pid = projectId {
            projectName = model.projects.first(where: { $0.id == pid })?.name
        } else {
            projectName = nil
        }

        let size = canvasSize
        let pid = projectId
        let n = Int64(maxNodes)

        // Fetch graph data off-actor via AppModel's load helper.
        let loaded: ContactGraph = await withCheckedContinuation { continuation in
            model.loadContactGraph(projectId: pid, maxNodes: n) { g in
                continuation.resume(returning: g)
            }
        }

        guard !Task.isCancelled else { return }

        // Layout runs off-main (pure, deterministic — safe on any thread).
        let pts = await Task.detached(priority: .userInitiated) {
            Self.layout(nodes: loaded.nodes, edges: loaded.edges, size: size)
        }.value

        graph = loaded
        positions = pts
    }
}
