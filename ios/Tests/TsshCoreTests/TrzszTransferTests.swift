import XCTest
@testable import TsshCore

/// Phase 1C(#25): `TerminalSessionController.trzszSendChunked`(「1チャンク先読みして
/// isLastを判定する」読み出しループ)の検証。実ファイルI/Oは行わず、`Data`の配列を
/// 順番に返すクロージャで駆動する。
final class TrzszSendChunkedTests: XCTestCase {
    private func makeReader(chunks: [Data]) -> () -> Data {
        var remaining = chunks
        return {
            if remaining.isEmpty { return Data() }
            return remaining.removeFirst()
        }
    }

    func testEmptyFileSendsSingleEmptyLastChunk() {
        var sent: [(Data, Bool)] = []
        let readNext = makeReader(chunks: [Data()])

        TerminalSessionController.trzszSendChunked(readNext: readNext, send: { chunk, isLast in
            sent.append((chunk, isLast))
        })

        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0].0, Data())
        XCTAssertTrue(sent[0].1)
    }

    func testSingleChunkSmallerThanChunkSizeIsMarkedLast() {
        var sent: [(Data, Bool)] = []
        let onlyChunk = Data([1, 2, 3])
        let readNext = makeReader(chunks: [onlyChunk])

        TerminalSessionController.trzszSendChunked(readNext: readNext, send: { chunk, isLast in
            sent.append((chunk, isLast))
        })

        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0].0, onlyChunk)
        XCTAssertTrue(sent[0].1)
    }

    func testMultipleFullChunksOnlyLastIsMarked() {
        var sent: [(Data, Bool)] = []
        let chunk1 = Data(repeating: 0xAA, count: TerminalSessionController.trzszChunkSize)
        let chunk2 = Data(repeating: 0xBB, count: TerminalSessionController.trzszChunkSize)
        let chunk3 = Data([0xCC])
        let readNext = makeReader(chunks: [chunk1, chunk2, chunk3])

        TerminalSessionController.trzszSendChunked(readNext: readNext, send: { chunk, isLast in
            sent.append((chunk, isLast))
        })

        XCTAssertEqual(sent.map(\.0), [chunk1, chunk2, chunk3])
        XCTAssertEqual(sent.map(\.1), [false, false, true])
    }

    func testExactlyOneChunkSizeBoundaryStillDetectsEOFOnNextRead() {
        var sent: [(Data, Bool)] = []
        let exactChunk = Data(repeating: 0x11, count: TerminalSessionController.trzszChunkSize)
        let readNext = makeReader(chunks: [exactChunk])

        TerminalSessionController.trzszSendChunked(readNext: readNext, send: { chunk, isLast in
            sent.append((chunk, isLast))
        })

        // ちょうどchunkSize分だけのデータでも、次の読み出しが空(EOF)になった時点で
        // 直前のチャンクをisLast=trueとして送る。
        XCTAssertEqual(sent.count, 1)
        XCTAssertEqual(sent[0].0, exactChunk)
        XCTAssertTrue(sent[0].1)
    }
}
