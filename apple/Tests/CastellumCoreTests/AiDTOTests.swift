// Castellum - Tests for AI DTO decoding (CoreModels AI types).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AiDTOTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    // MARK: - AiEnqueueResult

    func testAiEnqueueResultDecodes() throws {
        let r = try decode(AiEnqueueResult.self, #"{"job_id":42}"#)
        XCTAssertEqual(r.jobId, 42)
    }

    // MARK: - AiPollResult

    func testAiPollResultDecodes() throws {
        let json = """
        {
          "job": {"id":7,"status":"running","feature":"summarize_message","attempts":1,"error_code":null},
          "events": [
            {"id":1,"kind":"started","payload":""},
            {"id":2,"kind":"delta","payload":"Hello "},
            {"id":3,"kind":"delta","payload":"world"}
          ]
        }
        """
        let r = try decode(AiPollResult.self, json)
        XCTAssertEqual(r.job.id, 7)
        XCTAssertEqual(r.job.status, "running")
        XCTAssertEqual(r.job.feature, "summarize_message")
        XCTAssertEqual(r.job.attempts, 1)
        XCTAssertNil(r.job.errorCode)
        XCTAssertEqual(r.events.count, 3)
        XCTAssertEqual(r.events[0].kind, "started")
        XCTAssertEqual(r.events[1].kind, "delta")
        XCTAssertEqual(r.events[2].payload, "world")
    }

    func testAiPollResultNullable() throws {
        // The core returns JSON null for an unknown job id.
        let nil_ = try decode(AiPollResult?.self, "null")
        XCTAssertNil(nil_)
    }

    func testAiPollResultWithError() throws {
        let json = """
        {
          "job": {"id":3,"status":"failed","feature":"summarize_message","attempts":3,
                  "error_code":"model_missing"},
          "events": []
        }
        """
        let r = try decode(AiPollResult.self, json)
        XCTAssertEqual(r.job.status, "failed")
        XCTAssertEqual(r.job.errorCode, "model_missing")
        XCTAssertTrue(r.events.isEmpty)
    }

    // MARK: - AiModelInfo

    func testAiModelInfoListDecodes() throws {
        let json = #"[{"name":"llama3.1:8b","size_bytes":4661211136},{"name":"phi4","size_bytes":null}]"#
        let models = try decode([AiModelInfo].self, json)
        XCTAssertEqual(models.count, 2)
        XCTAssertEqual(models[0].name, "llama3.1:8b")
        XCTAssertEqual(models[0].sizeBytes, 4_661_211_136)
        XCTAssertNil(models[1].sizeBytes)
        XCTAssertEqual(models[1].id, "phi4")
    }

    // MARK: - AiOutput

    func testAiOutputDecodes() throws {
        let json = """
        {"text":"This is the summary.","complete":true,"model":"llama3.1:8b",
         "tokens_in":512,"tokens_out":128,"job_id":5}
        """
        let o = try decode(AiOutput.self, json)
        XCTAssertEqual(o.text, "This is the summary.")
        XCTAssertTrue(o.complete)
        XCTAssertEqual(o.model, "llama3.1:8b")
        XCTAssertEqual(o.tokensIn, 512)
        XCTAssertEqual(o.tokensOut, 128)
        XCTAssertEqual(o.jobId, 5)
    }

    func testAiOutputNullable() throws {
        let nil_ = try decode(AiOutput?.self, "null")
        XCTAssertNil(nil_)
    }

    func testAiOutputPartial() throws {
        let json = #"{"text":"partial…","complete":false,"model":"llama3.1:8b","tokens_in":100,"tokens_out":30,"job_id":2}"#
        let o = try decode(AiOutput.self, json)
        XCTAssertFalse(o.complete)
        XCTAssertEqual(o.text, "partial…")
    }

    // MARK: - AiRunReport

    func testAiRunReportDecodes() throws {
        let json = #"{"ran":3,"succeeded":2,"failed":0,"cancelled":1}"#
        let r = try decode(AiRunReport.self, json)
        XCTAssertEqual(r.ran, 3)
        XCTAssertEqual(r.succeeded, 2)
        XCTAssertEqual(r.failed, 0)
        XCTAssertEqual(r.cancelled, 1)
    }

    func testAiRunReportAllZero() throws {
        let r = try decode(AiRunReport.self, #"{"ran":0,"succeeded":0,"failed":0,"cancelled":0}"#)
        XCTAssertEqual(r.ran, 0)
    }
}
