import Foundation
import Vision

let arguments = CommandLine.arguments
let languages = arguments[1].split(separator: ",").map(String.init)
var texts: [String] = []

do {
    for path in arguments.dropFirst(2) {
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        request.usesLanguageCorrection = true
        let supported = try request.supportedRecognitionLanguages()
        let requested = languages.filter { supported.contains($0) }
        if !requested.isEmpty {
            request.recognitionLanguages = requested
        }
        let handler = VNImageRequestHandler(url: URL(fileURLWithPath: path), options: [:])
        try handler.perform([request])
        let lines = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
        texts.append(lines.joined(separator: "\n"))
    }
    let data = try JSONEncoder().encode(texts)
    FileHandle.standardOutput.write(data)
} catch {
    let message = "Vision OCR: \(error.localizedDescription)\n"
    FileHandle.standardError.write(Data(message.utf8))
    exit(1)
}
