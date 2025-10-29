import Foundation
import AVFoundation
import Darwin

let engine = AVAudioEngine()
let inputNode = engine.inputNode
let inputFormat = inputNode.outputFormat(forBus: 0)

engine.connect(inputNode, to: engine.outputNode, format: inputFormat)

do {
    try engine.start()
    print("🔊 Audio passthrough running (Ctrl+C to stop)")
    RunLoop.main.run()
} catch {
    print("Error: \(error)")
}
