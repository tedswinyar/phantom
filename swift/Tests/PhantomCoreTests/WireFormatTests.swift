// Wire-format tests decode the SHARED fixtures (tests/fixtures/ at the repo
// root) — the same bytes the Rust tests and the OPE conformance harness use.
// If Swift and Rust ever disagree about the wire format, these fail first.

import XCTest
@testable import PhantomCore

final class WireFormatTests: XCTestCase {
    /// Locate the repo-root fixtures dir relative to this source file.
    static func fixture(_ name: String) throws -> Data {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // → PhantomCoreTests/
            .deletingLastPathComponent() // → Tests/
            .deletingLastPathComponent() // → swift/
            .deletingLastPathComponent() // → repo root
            .appending(path: "tests/fixtures/\(name)")
        return try Data(contentsOf: url)
    }

    // L3: the decode regex requires the colon in the offset (±HH:MM), matching
    // the wire contract. Colon-less offsets are rejected consistently at the
    // regex gate rather than depending on ICU parser leniency.
    func testColonlessOffsetIsRejected() {
        XCTAssertNil(WireDate.decode("2026-03-17T14:30:00+0100"), "colon-less offset is out of contract")
        XCTAssertNotNil(WireDate.decode("2026-03-17T14:30:00+01:00"), "colon offset is in contract")
    }

    // L4: the reviewer predicted a 3-digit year for pre-year-1000 dates. It
    // does not reproduce — `yyyy` zero-pads to a minimum of four digits — so
    // there is no lower-bound saturation guard. This pins the actual behavior:
    // an ancient instant still emits a 4-digit year that satisfies the `\d{4}`
    // contract and round-trips.
    func testEncodePadsAncientYearsToFourDigits() throws {
        let ancient = Date(timeIntervalSince1970: -70_000_000_000) // ~year 250
        let encoded = WireDate.encode(ancient)
        XCTAssertNotNil(
            try? #/^\d{4}-/#.prefixMatch(in: encoded),
            "year must be exactly 4 digits, got \(encoded)"
        )
        // Round-trips through our own decoder.
        XCTAssertEqual(WireDate.decode(encoded).map(WireDate.encode), encoded)
    }

    func testEncodeSaturatesAboveYear9999() {
        // Far past year 9999 → clamp to the maximum canonical instant.
        let farFuture = Date(timeIntervalSince1970: 400_000_000_000)
        XCTAssertEqual(WireDate.encode(farFuture), "9999-12-31T23:59:59.999999Z")
    }

    // The generous-decode table (interop guide rule 5): every known
    // producer's format must parse.
    func testDecodesAllKnownDatetimeVariants() {
        for variant in [
            "2026-03-17T14:30:00Z",
            "2026-03-17T14:30:00.123Z",
            "2026-03-17T14:30:00.123456Z",
            "2026-03-17T14:30:00.123456+00:00",
            "2026-03-17T14:30:00+01:00",
        ] {
            XCTAssertNotNil(WireDate.decode(variant), "must accept \(variant)")
        }
    }

    func testRejectsGarbageDatetimes() {
        for junk in ["", "yesterday", "2026-03-17", "14:30:00"] {
            XCTAssertNil(WireDate.decode(junk), "must reject \(junk)")
        }
    }

    // The SHARED generous-decode fixture — the Rust decode tests iterate the
    // same file. Every `input` must decode and re-encode to exactly
    // `canonical`; a Rust/Swift divergence on any producer variant fails here.
    func testSharedDatetimeVariantsDecodeToCanonical() throws {
        struct Variant: Decodable { let input: String; let canonical: String }
        struct Fixture: Decodable { let accept: [Variant]; let reject: [String] }
        let f = try JSONDecoder().decode(Fixture.self, from: Self.fixture("datetime-variants.json"))
        for row in f.accept {
            let d = try XCTUnwrap(WireDate.decode(row.input), "must accept \(row.input)")
            XCTAssertEqual(WireDate.encode(d), row.canonical, "canonical mismatch for \(row.input)")
        }
        for bad in f.reject {
            XCTAssertNil(WireDate.decode(bad), "must reject \(bad)")
        }
    }

    func testCanonicalEncodeRoundTripsThroughRustFormat() throws {
        // Encode then re-decode: the canonical 6-digit format must survive
        // our own decoder (and, by the shared fixtures, Rust's).
        let now = try XCTUnwrap(WireDate.decode("2026-08-19T10:00:00.250000Z"))
        let encoded = WireDate.encode(now)
        XCTAssertEqual(encoded, "2026-08-19T10:00:00.250000Z")
        XCTAssertEqual(WireDate.decode(encoded), now)
    }

    func testErrorShapeParsesFromRawBytes() {
        let raw = Data(#"{"error": "title must not be empty"}"#.utf8)
        XCTAssertEqual(APIClient.errorMessage(from: raw), "title must not be empty")
        XCTAssertEqual(APIClient.errorMessage(from: Data("not json".utf8)), "unknown server error")
    }

    // H4: an empty or junk PHANTOM_API_URL must not crash the app on
    // launch. resolveBaseURL falls back to the default instead of force-
    // unwrapping nil.
    func testResolveBaseURLFailsSoftOnBadInput() {
        XCTAssertEqual(APIClient.resolveBaseURL(nil), APIClient.defaultBaseURL)
        XCTAssertEqual(APIClient.resolveBaseURL(""), APIClient.defaultBaseURL)
        XCTAssertEqual(APIClient.resolveBaseURL("   "), APIClient.defaultBaseURL)
        // Scheme-less / host-less values are not usable HTTP bases → default.
        XCTAssertEqual(APIClient.resolveBaseURL("localhost:8768"), APIClient.defaultBaseURL)
        // A valid override is honored.
        XCTAssertEqual(
            APIClient.resolveBaseURL("http://127.0.0.1:9000"),
            URL(string: "http://127.0.0.1:9000")
        )
    }

    func testAnnouncementLineParses() {
        let url = APIServerManager.parseAnnouncement(
            "phantom-api listening on http://127.0.0.1:63294\n"
        )
        XCTAssertEqual(url?.port, 63294)
        XCTAssertNil(APIServerManager.parseAnnouncement("some other log line"))
        XCTAssertNil(APIServerManager.parseAnnouncement(""))
    }
}
