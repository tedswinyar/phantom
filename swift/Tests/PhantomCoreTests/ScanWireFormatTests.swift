// Scan-domain wire-format tests: decode the SHARED fixtures (tests/fixtures/
// at the repo root) FROM RAW BYTES — the same bytes the Rust tests and the
// OPE conformance harness use. If Swift and Rust ever disagree about the
// scan wire format, these fail first.

import XCTest
@testable import PhantomCore

final class ScanWireFormatTests: XCTestCase {

    // MARK: - Scan (+ progress)

    func testDecodesRunningScanFixture() throws {
        let scan = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-running.json"))
        XCTAssertEqual(scan.id.uuidString.lowercased(), "0b54b774-19a1-4373-a423-77aa93e40e5b")
        XCTAssertEqual(scan.rootPath, "/Users/ghost")
        XCTAssertEqual(scan.status, .running)
        XCTAssertFalse(scan.isTerminal)
        XCTAssertNil(scan.finishedAt)
        // A just-started scan has no totals yet.
        XCTAssertEqual(scan.totalDiskSize, 0)
        XCTAssertEqual(scan.totalLogicalSize, 0)
        XCTAssertEqual(scan.fileCount, 0)
        XCTAssertEqual(scan.dirCount, 0)
        XCTAssertEqual(scan.errorCount, 0)
        // Live progress: object while running, with the exact counter shape.
        let progress = try XCTUnwrap(scan.progress, "running scan must carry progress")
        XCTAssertEqual(progress.filesSeen, 1337)
        XCTAssertEqual(progress.bytesSeen, 987_654_321)
        XCTAssertEqual(progress.currentPath, "/Users/ghost/Library/Caches/deep/file.bin")
    }

    func testDecodesCompleteScanFixture() throws {
        let scan = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        XCTAssertEqual(scan.status, .complete)
        XCTAssertTrue(scan.isTerminal)
        XCTAssertNil(scan.progress, "terminal scan carries progress: null")
        XCTAssertNotNil(scan.finishedAt)
        // Disk and logical totals differ in the fixture ON PURPOSE, so a
        // swapped mapping cannot pass.
        XCTAssertEqual(scan.totalDiskSize, 123_456_789)
        XCTAssertEqual(scan.totalLogicalSize, 223_456_789)
        XCTAssertEqual(scan.fileCount, 4200)
        XCTAssertEqual(scan.dirCount, 310)
        XCTAssertEqual(scan.errorCount, 2)
    }

    func testScanRejectsUnknownStatus() throws {
        let raw = try String(
            data: try WireFormatTests.fixture("scan-complete.json"), encoding: .utf8)
        let mutated = Data(try XCTUnwrap(raw).replacingOccurrences(
            of: "\"complete\"", with: "\"exploded\"").utf8)
        XCTAssertThrowsError(try Wire.decoder().decode(Scan.self, from: mutated))
    }

    func testScanAcceptsUppercaseUUIDAndEncodesLowercase() throws {
        let raw = try String(
            data: try WireFormatTests.fixture("scan-complete.json"), encoding: .utf8)
        let mutated = Data(try XCTUnwrap(raw).replacingOccurrences(
            of: "e7ae86e2-308b-444c-8a3d-cd21467ab442",
            with: "E7AE86E2-308B-444C-8A3D-CD21467AB442").utf8)
        let scan = try Wire.decoder().decode(Scan.self, from: mutated)
        let obj = try encodeToObject(scan)
        XCTAssertEqual(obj["id"] as? String, "e7ae86e2-308b-444c-8a3d-cd21467ab442")
    }

    func testScanEncodesNullableFieldsAsPresentNulls() throws {
        // Running: finishedAt null. Terminal: progress null. Both must be
        // PRESENT as null — the synthesized encoder would omit them.
        let running = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-running.json"))
        let runningObj = try encodeToObject(running)
        let finishedAt = try XCTUnwrap(runningObj["finishedAt"], "finishedAt must be PRESENT as null")
        XCTAssertTrue(finishedAt is NSNull)

        let complete = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        let completeObj = try encodeToObject(complete)
        let progress = try XCTUnwrap(completeObj["progress"], "progress must be PRESENT as null")
        XCTAssertTrue(progress is NSNull)
    }

    // phantom-081: the scan diff decodes from the shared fixture, keeping
    // the "both languages consume every fixture" invariant honest. The
    // created dir carries before: null; the deleted one after: null; ids
    // encode lowercase; every delta key is present.
    func testScanDiffDecodesFromRawBytesAndReencodes() throws {
        let d = try Wire.decoder().decode(
            ScanDiff.self, from: try WireFormatTests.fixture("scan-diff.json"))
        XCTAssertEqual(d.rootPath, "/Users/ghost/Code")
        XCTAssertEqual(d.diskDelta, -31_457_280)
        XCTAssertEqual(d.fileCountDelta, -12)
        XCTAssertEqual(d.grown.count, 1)
        XCTAssertNil(d.grown[0].before, "created dir: before is null")
        XCTAssertEqual(d.freed.count, 2)
        XCTAssertNil(d.freed[1].after, "deleted dir: after is null")

        let scanA = try XCTUnwrap(try encodeToObject(d)["scanA"] as? String)
        XCTAssertEqual(scanA, scanA.lowercased(), "uuids encode lowercase")
        XCTAssertNil(d.reversedChronology, "natural-order fixture is unflagged")
        XCTAssertLessThan(d.scanAStartedAt, d.scanBStartedAt, "A older than B")
    }

    // Key-set drift guards for the three types added this session — the same
    // foot-gun protection testScanEncodeCoversEveryStoredField gives Scan:
    // a hand-written encoder that silently omits a new field is caught here.
    func testScanDiffEncodeCoversEveryStoredField() throws {
        let d = try Wire.decoder().decode(
            ScanDiff.self, from: try WireFormatTests.fixture("scan-diff.json"))
        let obj = try encodeToObject(d)
        XCTAssertEqual(Set(obj.keys), [
            "scanA", "scanB", "scanAStartedAt", "scanBStartedAt", "reversedChronology",
            "rootPath", "diskDelta", "logicalDelta", "fileCountDelta", "dirCountDelta",
            "errorCountDelta", "grown", "freed",
        ], "ScanDiff encode(to:) key set drifted — a field was added without an encode line")
        XCTAssertTrue(obj["reversedChronology"] is NSNull, "unflagged order present-as-null")
        // A DiffEntry's key set, at depth.
        let grown = try XCTUnwrap(obj["grown"] as? [[String: Any]])
        XCTAssertEqual(Set(grown[0].keys), ["path", "before", "after", "delta"],
                       "DiffEntry encode(to:) key set drifted")
        XCTAssertTrue(grown[0]["before"] is NSNull, "created dir before: present-as-null")
    }

    func testUnreadablePathEncodeCoversEveryStoredField() throws {
        let complete = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        let sample = try XCTUnwrap(complete.unreadablePaths?.first)
        let obj = try encodeToObject(sample)
        XCTAssertEqual(Set(obj.keys), ["path", "reason"],
                       "UnreadablePath encode key set drifted")
    }

    // phantom-671: the capped unreadable sample decodes from the shared
    // fixture's raw bytes — populated on the complete fixture (coheres with
    // errorCount 2), empty (not nil) on the running one; nil means "not
    // recorded" (pre-v4 rows) and still encodes present-as-null.
    func testUnreadablePathsDecodeAndNullRole() throws {
        let complete = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        XCTAssertEqual(
            complete.unreadablePaths,
            [
                UnreadablePath(path: "/Users/ghost/Code/locked",
                               reason: "Permission denied (os error 13)"),
                UnreadablePath(path: "/Users/ghost/Code/vanished.tmp",
                               reason: "No such file or directory (os error 2)"),
            ])

        let running = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-running.json"))
        XCTAssertEqual(running.unreadablePaths, [], "recorded-and-empty, not nil")

        var notRecorded = complete
        notRecorded.unreadablePaths = nil
        let obj = try encodeToObject(notRecorded)
        let value = try XCTUnwrap(obj["unreadablePaths"], "must be PRESENT as null")
        XCTAssertTrue(value is NSNull)
    }

    // Foot-gun guard: the hand-written encode(to:) is a
    // hard-coded field list. If a field is added to Scan without a matching
    // encode line, the emitted key set drifts and this goes red.
    func testScanEncodeCoversEveryStoredField() throws {
        let scan = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-running.json"))
        let obj = try encodeToObject(scan)
        let expected: Set<String> = [
            "id", "rootPath", "status", "startedAt", "finishedAt",
            "totalDiskSize", "totalLogicalSize",
            "fileCount", "dirCount", "errorCount", "unreadablePaths",
            "progress", "totalPrivateSize", "totalSharedSize", "failureReason",
        ]
        XCTAssertEqual(Set(obj.keys), expected,
                       "encode(to:) key set drifted — a Scan field was added without a matching encode line")
        // Nested progress keys are camelCase at depth too.
        let progress = try XCTUnwrap(obj["progress"] as? [String: Any])
        XCTAssertEqual(Set(progress.keys), ["filesSeen", "bytesSeen", "currentPath"])
    }

    /// phantom-aoa: a scan the server was running when it stopped comes
    /// back failed with a reason the UI shows verbatim. Pre-1.1 bodies have
    /// no key at all and must still decode.
    func testInterruptedScanCarriesItsReasonAndOldBodiesDecodeWithout() throws {
        let scan = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-interrupted.json"))
        XCTAssertEqual(scan.status, .failed)
        XCTAssertTrue(scan.isTerminal)
        let reason = try XCTUnwrap(scan.failureReason)
        XCTAssertTrue(reason.hasPrefix("interrupted:"), reason)

        var obj = try JSONSerialization.jsonObject(
            with: try WireFormatTests.fixture("scan-complete.json")) as! [String: Any]
        obj.removeValue(forKey: "failureReason")
        let old = try Wire.decoder().decode(
            Scan.self, from: try JSONSerialization.data(withJSONObject: obj))
        XCTAssertNil(old.failureReason, "a pre-1.1 body decodes with a nil reason")
        XCTAssertNil(scan.progress)
    }

    func testScanTimestampsRoundTripCanonically() throws {
        let scan = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        let obj = try encodeToObject(scan)
        XCTAssertEqual(obj["startedAt"] as? String, "2026-03-17T14:30:00.123456Z")
        XCTAssertEqual(obj["finishedAt"] as? String, "2026-03-17T14:31:05.654321Z")
    }

    // MARK: - ScanEntry

    func testDecodesFileEntryFixture() throws {
        let entry = try Wire.decoder().decode(
            ScanEntry.self, from: try WireFormatTests.fixture("entry.json"))
        XCTAssertEqual(entry.path, "/Users/ghost/Code/phantom/Cargo.lock")
        XCTAssertEqual(entry.parentPath, "/Users/ghost/Code/phantom")
        XCTAssertEqual(entry.name, "Cargo.lock")
        XCTAssertFalse(entry.isDir)
        XCTAssertEqual(entry.diskSize, 49152)
        XCTAssertEqual(entry.logicalSize, 47811)
        // diskSize is THE size: the headline number is st_blocks × 512, never
        // the logical size (which the fixture keeps different on purpose).
        XCTAssertEqual(entry.displaySize, 49152)
        XCTAssertNotNil(entry.modifiedAt)
        XCTAssertEqual(entry.fileType, "lock")
        XCTAssertNil(entry.category)
        XCTAssertEqual(entry.nlink, 1)
        XCTAssertEqual(entry.dev, 16_777_233)
        XCTAssertEqual(entry.ino, 42_424_242)
        // File rows never carry descendant counts.
        XCTAssertNil(entry.fileCount)
        XCTAssertNil(entry.dirCount)
        // The fixture is a pure APFS clone: full allocation, nothing private,
        // a clone-group id, the two clone flags.
        XCTAssertEqual(entry.privateSize, 0)
        XCTAssertEqual(entry.sharedSize, 49152)
        XCTAssertEqual(entry.cloneId, 42_424_242)
        XCTAssertEqual(entry.flags, ["mayShareBlocks", "sharesAllBlocks"])
        XCTAssertTrue(entry.hasFlag("sharesAllBlocks"))
        XCTAssertFalse(entry.hasFlag("dataless"))
    }

    // The v1.1 scan totals ride the same fixtures: real values on the
    // complete scan, zeros while running.
    func testDecodesShareTotalsFromScanFixtures() throws {
        let complete = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-complete.json"))
        XCTAssertEqual(complete.totalPrivateSize, 100_000_000)
        XCTAssertEqual(complete.totalSharedSize, 23_456_789)
        let running = try Wire.decoder().decode(
            Scan.self, from: try WireFormatTests.fixture("scan-running.json"))
        XCTAssertEqual(running.totalPrivateSize, 0)
        XCTAssertEqual(running.totalSharedSize, 0)
        // Pre-v5 rows: null totals decode as nil and encode present-as-null.
        var notRecorded = complete
        notRecorded.totalPrivateSize = nil
        notRecorded.totalSharedSize = nil
        let obj = try encodeToObject(notRecorded)
        for key in ["totalPrivateSize", "totalSharedSize"] {
            let value = try XCTUnwrap(obj[key], "\(key) must be PRESENT as null")
            XCTAssertTrue(value is NSNull, "\(key) must be null, got \(value)")
        }
    }

    // Pre-v5 entry rows carry the four share fields as null; they must
    // decode as nil and re-encode present-as-null — and an unknown flag
    // string from a newer server must not fail the decode.
    func testEntryShareFieldsNullAndUnknownFlagsDecode() throws {
        var raw = String(decoding: try WireFormatTests.fixture("entry.json"), as: UTF8.self)
        raw = raw.replacingOccurrences(of: "\"privateSize\": 0", with: "\"privateSize\": null")
        raw = raw.replacingOccurrences(of: "\"sharedSize\": 49152", with: "\"sharedSize\": null")
        raw = raw.replacingOccurrences(of: "\"cloneId\": 42424242", with: "\"cloneId\": null")
        raw = raw.replacingOccurrences(of: "\"flags\": [\"mayShareBlocks\", \"sharesAllBlocks\"]", with: "\"flags\": null")
        XCTAssertTrue(raw.contains("\"flags\": null"), "precondition: fixture rewritten")
        let entry = try Wire.decoder().decode(ScanEntry.self, from: Data(raw.utf8))
        XCTAssertNil(entry.privateSize)
        XCTAssertNil(entry.sharedSize)
        XCTAssertNil(entry.cloneId)
        XCTAssertNil(entry.flags)
        let obj = try encodeToObject(entry)
        for key in ["privateSize", "sharedSize", "cloneId", "flags"] {
            let value = try XCTUnwrap(obj[key], "\(key) must be PRESENT as null, not absent")
            XCTAssertTrue(value is NSNull, "\(key) must be null, got \(value)")
        }

        let future = String(decoding: try WireFormatTests.fixture("entry.json"), as: UTF8.self)
            .replacingOccurrences(of: "\"sharesAllBlocks\"", with: "\"sharesAllBlocks\", \"fromTheFuture\"")
        let tolerant = try Wire.decoder().decode(ScanEntry.self, from: Data(future.utf8))
        XCTAssertTrue(tolerant.hasFlag("fromTheFuture"), "unknown flags are kept raw, never fatal")
    }

    // The all-nullables-present-as-null decode case — for the fields that
    // are nullable on DIR rows. fileCount/dirCount invert: null on file rows
    // (entry.json pins that), real values here. The fixture's counts cohere
    // with scan-complete.json (4200 files; 310 dirs INCLUDING the root) to
    // pin the excluding-self rule.
    func testDecodesDirEntryWithAllNullablesNull() throws {
        let entry = try Wire.decoder().decode(
            ScanEntry.self, from: try WireFormatTests.fixture("entry-dir.json"))
        XCTAssertEqual(entry.path, "/Users/ghost/Code/phantom")
        XCTAssertTrue(entry.isDir)
        XCTAssertNil(entry.parentPath, "the scan root has no parent")
        XCTAssertNil(entry.modifiedAt)
        XCTAssertNil(entry.fileType)
        XCTAssertNil(entry.category)
        XCTAssertEqual(entry.nlink, 12)
        XCTAssertEqual(entry.fileCount, 4200)
        XCTAssertEqual(entry.dirCount, 309)
        // The dir row pins the populated split and the never-a-clone case.
        XCTAssertEqual(entry.privateSize, 100_000_000)
        XCTAssertEqual(entry.sharedSize, 23_456_789)
        XCTAssertNil(entry.cloneId)
        XCTAssertEqual(entry.flags, [])
    }

    func testEntryEncodesNullableFieldsAsPresentNulls() throws {
        let entry = try Wire.decoder().decode(
            ScanEntry.self, from: try WireFormatTests.fixture("entry-dir.json"))
        let obj = try encodeToObject(entry)
        for key in ["parentPath", "modifiedAt", "fileType", "category", "cloneId"] {
            let value = try XCTUnwrap(obj[key], "\(key) must be PRESENT as null, not absent")
            XCTAssertTrue(value is NSNull, "\(key) must be null, got \(value)")
        }
        // No flags is an empty array, never null (wire rule: arrays that
        // are empty are []).
        XCTAssertEqual(obj["flags"] as? [String], [])
        // The count-null case rides the FILE row.
        let file = try Wire.decoder().decode(
            ScanEntry.self, from: try WireFormatTests.fixture("entry.json"))
        let fileObj = try encodeToObject(file)
        for key in ["fileCount", "dirCount"] {
            let value = try XCTUnwrap(fileObj[key], "\(key) must be PRESENT as null, not absent")
            XCTAssertTrue(value is NSNull, "\(key) must be null on a file row, got \(value)")
        }
    }

    func testEntryEncodeCoversEveryStoredField() throws {
        let entry = try Wire.decoder().decode(
            ScanEntry.self, from: try WireFormatTests.fixture("entry.json"))
        let obj = try encodeToObject(entry)
        let expected: Set<String> = [
            "path", "parentPath", "name", "isDir", "diskSize", "logicalSize",
            "modifiedAt", "fileType", "category", "nlink", "dev", "ino",
            "fileCount", "dirCount", "privateSize", "sharedSize", "cloneId", "flags",
        ]
        XCTAssertEqual(Set(obj.keys), expected,
                       "encode(to:) key set drifted — a ScanEntry field was added without a matching encode line")
    }

    // MARK: - Treemap

    func testDecodesTreemapFixture() throws {
        let layout = try Wire.decoder().decode(
            TreemapLayout.self, from: try WireFormatTests.fixture("treemap.json"))
        XCTAssertEqual(layout.rootPath, "/Users/ghost/Code")
        XCTAssertEqual(layout.totalSize, 4_194_304)
        XCTAssertEqual(layout.rects.count, 4)
        let root = try XCTUnwrap(layout.rects.first)
        XCTAssertEqual(root.depth, 0)
        XCTAssertTrue(root.isDir)
        XCTAssertEqual(root.width, 800.0)
        XCTAssertEqual(root.height, 600.0)
        XCTAssertNil(root.fileType)
        XCTAssertFalse(root.residual, "real rects decode residual: false")
        let file = layout.rects[2]
        XCTAssertEqual(file.name, "big.bin")
        XCTAssertEqual(file.size, 1_048_576)
        XCTAssertFalse(file.isDir)
        XCTAssertEqual(file.fileType, "bin")
        XCTAssertEqual(file.x, 400.0)
        XCTAssertFalse(file.residual)
        // The residual pseudo-tile: parent's path, disambiguated id, leaf.
        let res = try XCTUnwrap(layout.rects.last)
        XCTAssertTrue(res.residual)
        XCTAssertEqual(res.path, "/Users/ghost/Code")
        XCTAssertEqual(res.name, "smaller files")
        XCTAssertEqual(res.size, 1_048_576)
        XCTAssertFalse(res.isDir)
        XCTAssertNil(res.fileType)
        XCTAssertEqual(res.depth, 1)
        XCTAssertNotEqual(res.id, root.id,
                          "residual shares the parent's path but must not share its id")
    }

    func testTreemapRectEncodesNullFileTypeAtDepth() throws {
        // The wire contract applies at EVERY nesting depth: the nested rects'
        // nullable fileType must be present-as-null, and the key set pinned.
        let layout = try Wire.decoder().decode(
            TreemapLayout.self, from: try WireFormatTests.fixture("treemap.json"))
        let data = try Wire.encoder().encode(layout)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(Set(obj.keys), ["rootPath", "totalSize", "rects"])
        let rects = try XCTUnwrap(obj["rects"] as? [[String: Any]])
        let rootRect = try XCTUnwrap(rects.first)
        let expected: Set<String> = [
            "path", "name", "size", "x", "y", "width", "height", "depth", "isDir", "fileType",
            "residual",
        ]
        XCTAssertEqual(Set(rootRect.keys), expected,
                       "encode(to:) key set drifted — a TreemapRect field was added without a matching encode line")
        let fileType = try XCTUnwrap(rootRect["fileType"], "fileType must be PRESENT as null")
        XCTAssertTrue(fileType is NSNull)
        // residual is always present: false on real rects, true on the last.
        XCTAssertEqual(rootRect["residual"] as? Bool, false)
        let lastRect = try XCTUnwrap(rects.last)
        XCTAssertEqual(lastRect["residual"] as? Bool, true)
    }

    func testTreemapRectResidualIsRequiredOnDecode() throws {
        // The contract has no absent case — a rect without the field is a
        // decode error, not a defaulted false (or worse, true).
        let raw = Data("""
        {"rootPath": "/r", "totalSize": 1, "rects": [
            {"path": "/r", "name": "r", "size": 1, "x": 0.0, "y": 0.0,
             "width": 1.0, "height": 1.0, "depth": 0, "isDir": true,
             "fileType": null}
        ]}
        """.utf8)
        XCTAssertThrowsError(try Wire.decoder().decode(TreemapLayout.self, from: raw))
    }

    // MARK: - FileTypeTotal (shared fixture — the same bytes the Rust tests
    // and the OPE conformance harness consume)

    func testDecodesFileTypeTotalsFixture() throws {
        let totals = try Wire.decoder().decode(
            [FileTypeTotal].self, from: try WireFormatTests.fixture("types.json"))
        // The fixture's order IS the contract order: diskSize descending,
        // name tiebreak (the bin/rs tie), the null-type bucket where its
        // size puts it.
        XCTAssertEqual(totals, [
            FileTypeTotal(fileType: "log", diskSize: 2_097_152, fileCount: 3),
            FileTypeTotal(fileType: "bin", diskSize: 1_048_576, fileCount: 42),
            FileTypeTotal(fileType: "rs", diskSize: 1_048_576, fileCount: 7),
            FileTypeTotal(fileType: nil, diskSize: 4096, fileCount: 1),
        ])
    }

    func testFileTypeTotalEncodesNullTypeAsPresent() throws {
        let total = FileTypeTotal(fileType: nil, diskSize: 4096, fileCount: 3)
        let data = try Wire.encoder().encode(total)
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(Set(obj.keys), ["fileType", "diskSize", "fileCount"])
        let fileType = try XCTUnwrap(obj["fileType"], "fileType must be PRESENT as null")
        XCTAssertTrue(fileType is NSNull)
    }

    // MARK: - Client-side pagination and query construction

    func testNextCursorReadsTheContractHeader() throws {
        let url = try XCTUnwrap(URL(string: "http://127.0.0.1:18770/scans/x/files"))
        let with = try XCTUnwrap(HTTPURLResponse(
            url: url, statusCode: 200, httpVersion: nil,
            headerFields: ["X-Next-Cursor": "100"]))
        XCTAssertEqual(APIClient.nextCursor(from: with), "100")
        let without = try XCTUnwrap(HTTPURLResponse(
            url: url, statusCode: 200, httpVersion: nil, headerFields: [:]))
        XCTAssertNil(APIClient.nextCursor(from: without),
                     "no header means the listing is complete")
    }

    // A drifted query parameter name silently means "unfiltered" server-side
    // (axum ignores unknown query params here), so the names are pinned.
    func testTreemapQueryUsesExactParameterNames() {
        let query = APIClient.treemapQuery(
            root: "/Users/ghost/Code/sub", width: 800, height: 600, maxDepth: 4)
        XCTAssertEqual(query, [
            URLQueryItem(name: "root", value: "/Users/ghost/Code/sub"),
            URLQueryItem(name: "width", value: "800.0"),
            URLQueryItem(name: "height", value: "600.0"),
            URLQueryItem(name: "maxDepth", value: "4"),
        ])
        XCTAssertEqual(APIClient.treemapQuery(root: nil, width: nil, height: nil, maxDepth: nil), [],
                       "absent params are omitted, not sent empty")
    }

    func testFilesQueryUsesExactParameterNames() {
        let query = APIClient.filesQuery(
            fileType: "rs", search: "main", sort: "size", limit: 50, cursor: "100")
        XCTAssertEqual(query, [
            URLQueryItem(name: "fileType", value: "rs"),
            URLQueryItem(name: "search", value: "main"),
            URLQueryItem(name: "sort", value: "size"),
            URLQueryItem(name: "limit", value: "50"),
            URLQueryItem(name: "cursor", value: "100"),
        ])
        XCTAssertEqual(
            APIClient.filesQuery(fileType: nil, search: nil, sort: nil, limit: nil, cursor: nil),
            [], "absent params are omitted, not sent empty")
    }

    // MARK: - Helpers

    private func encodeToObject<T: Encodable>(_ value: T) throws -> [String: Any] {
        let data = try Wire.encoder().encode(value)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
    }
}
