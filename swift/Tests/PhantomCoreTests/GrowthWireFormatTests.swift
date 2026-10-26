// Growth wire-format tests (v1.1 Phase 4): decode the SHARED fixture
// (tests/fixtures/growth.json) FROM RAW BYTES — the same bytes the Rust
// growth tests round-trip — and reproduce its key sets on encode.

import XCTest
@testable import PhantomCore

final class GrowthWireFormatTests: XCTestCase {

    private func raw() throws -> Data { try WireFormatTests.fixture("growth.json") }

    private func decodeFixture() throws -> Growth {
        try Wire.decoder().decode(Growth.self, from: try raw())
    }

    func testDecodesFixtureFromRawBytes() throws {
        let g = try decodeFixture()
        XCTAssertEqual(g.rootPath, "/Users/ghost")
        XCTAssertEqual(g.groupBy, "topLevelDir")
        XCTAssertEqual(g.points.count, 3)
        XCTAssertEqual(g.totals, [100_000_000_000, 130_000_000_000, 160_000_000_000], "oldest first")
        XCTAssertEqual(g.points.map(\.totalPrivateSize), [nil, 120_000_000_000, 150_000_000_000])
        XCTAssertEqual(g.points[0].scanId, UUID(uuidString: "11111111-1111-4111-8111-111111111111"))
        XCTAssertEqual(g.series.map(\.key), ["Code", "Library", "Movies", "other"])
        XCTAssertEqual(g.series[2].values, [0, 2_000_000_000, 5_000_000_000])
        let f = try XCTUnwrap(g.forecast)
        XCTAssertEqual(f.method, "linear")
        XCTAssertEqual(f.pointsUsed, 3)
        XCTAssertEqual(f.spanDays, 6.0)
        XCTAssertEqual(f.bytesPerDay, 10_000_000_000)
        XCTAssertEqual(f.daysUntilFull, 60.0)
        XCTAssertTrue(f.isGrowing)
        XCTAssertEqual(f.projectedFullAt, WireDate.decode("2026-11-06T09:00:00.000000Z"))
        XCTAssertTrue(f.caveat.contains("assumes"))
    }

    func testEncodeReproducesTheFixtureKeySetsAtEveryDepth() throws {
        let g = try decodeFixture()
        let data = try Wire.encoder().encode(g)
        let obj = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let rawObj = try XCTUnwrap(try JSONSerialization.jsonObject(with: try raw()) as? [String: Any])
        XCTAssertEqual(Set(obj.keys), Set(rawObj.keys))
        let points = try XCTUnwrap(obj["points"] as? [[String: Any]])
        let rawPoints = try XCTUnwrap(rawObj["points"] as? [[String: Any]])
        XCTAssertEqual(Set(points[0].keys), Set(rawPoints[0].keys))
        XCTAssertTrue(points[0]["totalPrivateSize"] is NSNull, "present-as-null, not absent")
        XCTAssertEqual(points[0]["scanId"] as? String, "11111111-1111-4111-8111-111111111111", "uuid lowercase out")
        let forecast = try XCTUnwrap(obj["forecast"] as? [String: Any])
        let rawForecast = try XCTUnwrap(rawObj["forecast"] as? [String: Any])
        XCTAssertEqual(Set(forecast.keys), Set(rawForecast.keys))
        XCTAssertEqual(forecast["projectedFullAt"] as? String, "2026-11-06T09:00:00.000000Z", "6-digit Z form out")
    }

    func testNullForecastAndNullSeriesValuesDecode() throws {
        var obj = try XCTUnwrap(try JSONSerialization.jsonObject(with: try raw()) as? [String: Any])
        obj["forecast"] = NSNull()
        obj["series"] = [["key": "cache", "values": [NSNull(), 5, 7]]]
        let data = try JSONSerialization.data(withJSONObject: obj)
        let g = try Wire.decoder().decode(Growth.self, from: data)
        XCTAssertNil(g.forecast)
        XCTAssertEqual(g.series, [GrowthLine(key: "cache", values: [nil, 5, 7])])
        // And back out: the null stays a null, the forecast stays present-as-null.
        let out = try XCTUnwrap(try JSONSerialization.jsonObject(with: try Wire.encoder().encode(g)) as? [String: Any])
        XCTAssertTrue(out["forecast"] is NSNull)
        let line = try XCTUnwrap((out["series"] as? [[String: Any]])?.first)
        XCTAssertTrue((line["values"] as? [Any])?.first is NSNull)
    }
}
