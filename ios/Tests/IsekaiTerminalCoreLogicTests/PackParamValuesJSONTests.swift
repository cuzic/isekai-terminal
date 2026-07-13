import XCTest
@testable import IsekaiTerminalCoreLogic

/// Android版`PackParamValuesJsonTest.kt`と同じ観点の検証。
final class PackParamValuesJSONTests: XCTestCase {

    func testRoundTripsASingleCtrlCharParam() {
        let values: [String: KeyStep] = ["prefix": .ctrlChar("b")]
        XCTAssertEqual(PackParamValuesJSON.decode(PackParamValuesJSON.encode(values)), values)
    }

    func testRoundTripsMultipleParams() {
        let values: [String: KeyStep] = [
            "prefix": .ctrlChar("a"),
            "secondary": .special(.functionKey(5)),
        ]
        XCTAssertEqual(PackParamValuesJSON.decode(PackParamValuesJSON.encode(values)), values)
    }

    func testEmptyMapEncodesAndDecodesToEmptyMap() {
        XCTAssertEqual(PackParamValuesJSON.decode(PackParamValuesJSON.encode([:])), [:])
    }

    func testBlankStringDecodesToEmptyMap() {
        XCTAssertEqual(PackParamValuesJSON.decode(""), [:])
    }

    func testMalformedJsonDecodesToEmptyMapInsteadOfThrowing() {
        XCTAssertEqual(PackParamValuesJSON.decode("{not valid"), [:])
    }

    func testUnresolvableValueForOneKeyIsSkippedOthersSurvive() {
        let json = #"{"prefix":{"type":"ctrlChar","char":"b"},"broken":{"type":"special","key":"totallyUnknown"}}"#
        XCTAssertEqual(PackParamValuesJSON.decode(json), ["prefix": .ctrlChar("b")])
    }
}
