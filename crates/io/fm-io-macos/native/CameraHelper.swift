import AVFoundation
import AudioToolbox
import CoreMedia
import CoreVideo
import Darwin
import Dispatch
import Foundation

private let discoveryMagic = Data([0x46, 0x4d, 0x43, 0x41, 0x4d, 0x44, 0x32, 0x00])
private let captureMagic = Data([0x46, 0x4d, 0x43, 0x41, 0x4d, 0x46, 0x33, 0x00])
private let metadataLength: UInt32 = 58
private let audioDiscoveryMagic = Data([0x46, 0x4d, 0x41, 0x55, 0x44, 0x44, 0x31, 0x00])
private let audioCaptureMagic = Data([0x46, 0x4d, 0x41, 0x55, 0x44, 0x46, 0x31, 0x00])
private let audioMetadataLength: UInt32 = 41
private let maximumDeviceCount = 64
private let maximumFormatsPerDevice = 256
private let maximumStringBytes = 4096
private let maximumDiscoveryBytes = 256 * 1024
private let maximumRecordLength: UInt32 = 64 * 1024 * 1024
private let maximumFrameWidth: UInt32 = 3840
private let maximumFrameHeight: UInt32 = 2160
private let maximumFramesPerSecond = 60
private let maximumAudioSampleRate: UInt32 = 192_000
private let maximumAudioChannels: UInt32 = 2
private let maximumAudioSamplesPerBlock = 16_384
private let maximumAudioBlockBytes: UInt32 = 16_384 * 2 * 4
private let sessionUnavailableExitCode: Int32 = 20

private enum HelperError: Error, CustomStringConvertible {
    case message(String)

    var description: String {
        switch self {
        case .message(let message): message
        }
    }
}

private struct Dimensions: Hashable {
    let width: UInt32
    let height: UInt32
}

private struct EmittedFormat: Hashable {
    let dimensions: Dimensions
    let frameRate: RationalFrameRate
}

private struct RationalFrameRate: Hashable {
    let numerator: UInt32
    let denominator: UInt32

    var duration: CMTime {
        CMTime(value: Int64(denominator), timescale: CMTimeScale(numerator))
    }

    var description: String {
        "\(numerator)/\(denominator)"
    }
}

private struct EmittedAudioFormat: Hashable {
    let sampleRate: UInt32
    let channels: UInt8
}

private enum EncodedColorPrimaries: UInt8 {
    case bt709 = 1
    case displayP3 = 2
    case bt2020 = 3
}

private enum EncodedTransferFunction: UInt8 {
    case sRGB = 1
    case bt709 = 2
}

private func diagnostic(_ message: String) {
    fputs("fm-camera-helper: \(message)\n", stderr)
}

private func writeStdout(_ data: Data) throws {
    do {
        try FileHandle.standardOutput.write(contentsOf: data)
    } catch {
        throw HelperError.message("failed to write stdout: \(error)")
    }
}

private extension Data {
    mutating func appendLittleEndian<T: FixedWidthInteger>(_ value: T) {
        var littleEndian = value.littleEndian
        Swift.withUnsafeBytes(of: &littleEndian) { bytes in
            append(contentsOf: bytes)
        }
    }

    mutating func appendLengthPrefixed(_ value: String, name: String) throws {
        guard value.utf8.count <= maximumStringBytes else {
            throw HelperError.message("\(name) exceeds \(maximumStringBytes) UTF-8 bytes")
        }
        let bytes = Data(value.utf8)
        guard let length = UInt32(exactly: bytes.count) else {
            throw HelperError.message("\(name) exceeds the protocol length limit")
        }
        appendLittleEndian(length)
        append(bytes)
    }
}

private func permissionByte(_ status: AVAuthorizationStatus) -> UInt8 {
    switch status {
    case .authorized: return 0
    case .notDetermined: return 1
    case .denied: return 2
    case .restricted: return 3
    @unknown default:
        diagnostic("unknown AVFoundation authorization status; reporting restricted")
        return 3
    }
}

private func greatestCommonDivisor(_ left: UInt32, _ right: UInt32) -> UInt32 {
    var left = left
    var right = right
    while right != 0 {
        let remainder = left % right
        left = right
        right = remainder
    }
    return left
}

private func validatedFrameRate(numerator: UInt32, denominator: UInt32) throws -> RationalFrameRate {
    guard numerator > 0, denominator > 0 else {
        throw HelperError.message("frame-rate numerator and denominator must be positive")
    }
    guard greatestCommonDivisor(numerator, denominator) == 1 else {
        throw HelperError.message("frame rate \(numerator)/\(denominator) is not normalized")
    }
    guard numerator <= UInt32(Int32.max) else {
        throw HelperError.message("frame-rate numerator exceeds the CoreMedia timescale limit")
    }
    guard UInt64(numerator) <= UInt64(maximumFramesPerSecond) * UInt64(denominator) else {
        throw HelperError.message("frame rate must not exceed \(maximumFramesPerSecond) fps")
    }
    return RationalFrameRate(numerator: numerator, denominator: denominator)
}

private let prioritizedFrameRates: [RationalFrameRate] = [
    RationalFrameRate(numerator: 24_000, denominator: 1_001),
    RationalFrameRate(numerator: 24, denominator: 1),
    RationalFrameRate(numerator: 25, denominator: 1),
    RationalFrameRate(numerator: 30_000, denominator: 1_001),
    RationalFrameRate(numerator: 30, denominator: 1),
    RationalFrameRate(numerator: 50, denominator: 1),
    RationalFrameRate(numerator: 60_000, denominator: 1_001),
    RationalFrameRate(numerator: 60, denominator: 1),
]

private let candidateFrameRates: [RationalFrameRate] = {
    var rates = Set(prioritizedFrameRates)
    for integerRate in 1...maximumFramesPerSecond {
        rates.insert(RationalFrameRate(numerator: UInt32(integerRate), denominator: 1))
    }
    return Array(rates)
}()

private func supports(_ frameRate: RationalFrameRate, in range: AVFrameRateRange) -> Bool {
    let duration = frameRate.duration
    return validPositiveDuration(duration)
        && validPositiveDuration(range.minFrameDuration)
        && validPositiveDuration(range.maxFrameDuration)
        && CMTimeCompare(duration, range.minFrameDuration) >= 0
        && CMTimeCompare(duration, range.maxFrameDuration) <= 0
}

private func supportedFrameRates(in ranges: [AVFrameRateRange]) -> [RationalFrameRate] {
    candidateFrameRates.filter { candidate in
        ranges.contains(where: { supports(candidate, in: $0) })
    }
}

private func ratePriority(_ frameRate: RationalFrameRate) -> Int {
    prioritizedFrameRates.firstIndex(of: frameRate) ?? prioritizedFrameRates.count
}

private func rateLessThan(_ left: RationalFrameRate, _ right: RationalFrameRate) -> Bool {
    UInt64(left.numerator) * UInt64(right.denominator)
        < UInt64(right.numerator) * UInt64(left.denominator)
}

private func dimensions(of format: AVCaptureDevice.Format) -> Dimensions? {
    let raw = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
    guard raw.width > 0, raw.height > 0,
          let width = UInt32(exactly: raw.width),
          let height = UInt32(exactly: raw.height) else {
        return nil
    }
    return Dimensions(width: width, height: height)
}

private func emittedFormats(for device: AVCaptureDevice) -> [EmittedFormat] {
    var formats: Set<EmittedFormat> = []
    for format in device.formats {
        guard let dimensions = dimensions(of: format),
              dimensions.width <= maximumFrameWidth,
              dimensions.height <= maximumFrameHeight else {
            continue
        }
        for frameRate in supportedFrameRates(in: format.videoSupportedFrameRateRanges) {
            formats.insert(EmittedFormat(dimensions: dimensions, frameRate: frameRate))
        }
    }

    return formats.sorted {
            let leftPriority = ratePriority($0.frameRate)
            let rightPriority = ratePriority($1.frameRate)
            if leftPriority != rightPriority {
                return leftPriority < rightPriority
            }
            if $0.dimensions.width != $1.dimensions.width {
                return $0.dimensions.width < $1.dimensions.width
            }
            if $0.dimensions.height != $1.dimensions.height {
                return $0.dimensions.height < $1.dimensions.height
            }
            return rateLessThan($0.frameRate, $1.frameRate)
        }
        .prefix(maximumFormatsPerDevice)
        .map { $0 }
}

private func videoDevices() -> [AVCaptureDevice] {
    var deviceTypes: [AVCaptureDevice.DeviceType] = [.builtInWideAngleCamera]
    if #available(macOS 14.0, *) {
        deviceTypes.append(.external)
        deviceTypes.append(.continuityCamera)
    } else {
        deviceTypes.append(.externalUnknown)
    }
    let session = AVCaptureDevice.DiscoverySession(
        deviceTypes: deviceTypes,
        mediaType: .video,
        position: .unspecified
    )
    return Array(session.devices
        .sorted { $0.uniqueID < $1.uniqueID }
        .prefix(maximumDeviceCount))
}

private func audioDevices() -> [AVCaptureDevice] {
    Array(AVCaptureDevice.devices(for: .audio)
        .sorted { $0.uniqueID < $1.uniqueID }
        .prefix(maximumDeviceCount))
}

private func emittedAudioFormat(_ format: AVCaptureDevice.Format) -> EmittedAudioFormat? {
    guard CMFormatDescriptionGetMediaType(format.formatDescription) == kCMMediaType_Audio,
          let description = CMAudioFormatDescriptionGetStreamBasicDescription(
              format.formatDescription
          )?.pointee,
          description.mSampleRate.isFinite,
          description.mSampleRate.rounded(.towardZero) == description.mSampleRate,
          let sampleRate = UInt32(exactly: description.mSampleRate),
          sampleRate > 0,
          sampleRate <= maximumAudioSampleRate,
          description.mChannelsPerFrame > 0,
          description.mChannelsPerFrame <= maximumAudioChannels,
          (description.mChannelsPerFrame == 1 || hasStereoLayout(format.formatDescription)),
          let channels = UInt8(exactly: description.mChannelsPerFrame) else {
        return nil
    }
    return EmittedAudioFormat(sampleRate: sampleRate, channels: channels)
}

private func hasStereoLayout(_ description: CMFormatDescription) -> Bool {
    var layoutSize = 0
    guard let layout = CMAudioFormatDescriptionGetChannelLayout(
        description,
        sizeOut: &layoutSize
    ) else {
        return false
    }
    return layout.pointee.mChannelLayoutTag == kAudioChannelLayoutTag_Stereo
}

private func emittedAudioFormats(for device: AVCaptureDevice) -> [EmittedAudioFormat] {
    Array(Set(device.formats.compactMap(emittedAudioFormat))
        .sorted {
            if $0.sampleRate != $1.sampleRate {
                return $0.sampleRate < $1.sampleRate
            }
            return $0.channels < $1.channels
        }
        .prefix(maximumFormatsPerDevice))
}

private func discoveryRecord(for device: AVCaptureDevice) throws -> Data {
    guard !device.uniqueID.isEmpty else {
        throw HelperError.message("camera identifier is empty")
    }
    let formats = emittedFormats(for: device)
    var record = Data()
    try record.appendLengthPrefixed(device.uniqueID, name: "camera identifier")
    try record.appendLengthPrefixed(device.localizedName, name: "camera name")
    record.appendLittleEndian(UInt32(formats.count))
    for format in formats {
        record.appendLittleEndian(format.dimensions.width)
        record.appendLittleEndian(format.dimensions.height)
        record.appendLittleEndian(format.frameRate.numerator)
        record.appendLittleEndian(format.frameRate.denominator)
    }
    return record
}

private func discover() throws {
    let headerLength = discoveryMagic.count + 1 + MemoryLayout<UInt32>.size
    var totalLength = headerLength
    var records: [Data] = []

    for device in videoDevices() {
        do {
            let record = try discoveryRecord(for: device)
            guard record.count <= maximumDiscoveryBytes - totalLength else {
                diagnostic("skipping camera device because discovery output would exceed \(maximumDiscoveryBytes) bytes")
                continue
            }
            records.append(record)
            totalLength += record.count
        } catch {
            diagnostic("skipping camera device: \(error)")
        }
    }

    var output = Data(capacity: totalLength)
    output.append(discoveryMagic)
    output.append(permissionByte(AVCaptureDevice.authorizationStatus(for: .video)))
    output.appendLittleEndian(UInt32(records.count))
    for record in records {
        output.append(record)
    }

    try writeStdout(output)
}

private func audioDiscoveryRecord(for device: AVCaptureDevice) throws -> Data {
    guard !device.uniqueID.isEmpty else {
        throw HelperError.message("microphone identifier is empty")
    }
    let formats = emittedAudioFormats(for: device)
    var record = Data()
    try record.appendLengthPrefixed(device.uniqueID, name: "microphone identifier")
    try record.appendLengthPrefixed(device.localizedName, name: "microphone name")
    record.appendLittleEndian(UInt32(formats.count))
    for format in formats {
        record.appendLittleEndian(format.sampleRate)
        record.append(format.channels)
    }
    return record
}

private func discoverAudio() throws {
    let headerLength = audioDiscoveryMagic.count + 1 + MemoryLayout<UInt32>.size
    var totalLength = headerLength
    var records: [Data] = []
    for device in audioDevices() {
        do {
            let record = try audioDiscoveryRecord(for: device)
            guard record.count <= maximumDiscoveryBytes - totalLength else {
                diagnostic("skipping microphone because discovery output would exceed \(maximumDiscoveryBytes) bytes")
                continue
            }
            records.append(record)
            totalLength += record.count
        } catch {
            diagnostic("skipping microphone: \(error)")
        }
    }

    var output = Data(capacity: totalLength)
    output.append(audioDiscoveryMagic)
    output.append(permissionByte(AVCaptureDevice.authorizationStatus(for: .audio)))
    output.appendLittleEndian(UInt32(records.count))
    for record in records {
        output.append(record)
    }
    try writeStdout(output)
}

private func requestPermission() throws {
    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
        return
    case .denied:
        throw HelperError.message("camera permission is denied; enable it in System Settings > Privacy & Security > Camera")
    case .restricted:
        throw HelperError.message("camera access is restricted by system policy")
    case .notDetermined:
        let semaphore = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .video) { result in
            granted = result
            semaphore.signal()
        }
        semaphore.wait()
        if !granted {
            throw HelperError.message("camera permission was not granted")
        }
    @unknown default:
        throw HelperError.message("camera authorization returned an unknown status")
    }
}

private func requestAudioPermission() throws {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized:
        return
    case .denied:
        throw HelperError.message("microphone permission is denied; enable it in System Settings > Privacy & Security > Microphone")
    case .restricted:
        throw HelperError.message("microphone access is restricted by system policy")
    case .notDetermined:
        let semaphore = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .audio) { result in
            granted = result
            semaphore.signal()
        }
        semaphore.wait()
        if !granted {
            throw HelperError.message("microphone permission was not granted")
        }
    @unknown default:
        throw HelperError.message("microphone authorization returned an unknown status")
    }
}

private func parsePositiveUInt32(_ value: String, name: String) throws -> UInt32 {
    guard let parsed = UInt32(value), parsed > 0 else {
        throw HelperError.message("invalid \(name) '\(value)'; expected a positive integer")
    }
    return parsed
}

private func selectedFormat(
    for device: AVCaptureDevice,
    dimensions requested: Dimensions,
    frameRate: RationalFrameRate
) throws -> AVCaptureDevice.Format {
    guard emittedFormats(for: device).contains(where: {
        $0.dimensions == requested && $0.frameRate == frameRate
    }) else {
        throw HelperError.message(
            "requested format \(requested.width)x\(requested.height) at \(frameRate.description) fps was not emitted for device '\(device.uniqueID)'"
        )
    }

    guard let format = device.formats.first(where: { format in
        guard dimensions(of: format) == requested else { return false }
        return format.videoSupportedFrameRateRanges.contains {
            supports(frameRate, in: $0)
        }
    }) else {
        throw HelperError.message("the emitted camera format is no longer available")
    }
    return format
}

private func validTimestamp(_ time: CMTime) -> Bool {
    time.isValid && time.isNumeric && !time.isIndefinite && time.timescale > 0
}

private func validPositiveDuration(_ time: CMTime) -> Bool {
    validTimestamp(time) && time.value > 0
}

private func requiredStringAttachment(
    _ pixelBuffer: CVPixelBuffer,
    key: CFString,
    name: String
) throws -> String {
    guard let attachment = CVBufferCopyAttachment(pixelBuffer, key, nil),
          let value = attachment as? String else {
        throw HelperError.message("camera frame has no valid \(name) attachment")
    }
    return value
}

private func encodedColorMetadata(
    _ pixelBuffer: CVPixelBuffer
) throws -> (EncodedColorPrimaries, EncodedTransferFunction) {
    let primaries = try requiredStringAttachment(
        pixelBuffer,
        key: kCVImageBufferColorPrimariesKey,
        name: "color primaries"
    )
    let encodedPrimaries: EncodedColorPrimaries
    if primaries == kCVImageBufferColorPrimaries_ITU_R_709_2 as String {
        encodedPrimaries = .bt709
    } else if primaries == kCVImageBufferColorPrimaries_P3_D65 as String {
        encodedPrimaries = .displayP3
    } else if primaries == kCVImageBufferColorPrimaries_ITU_R_2020 as String {
        encodedPrimaries = .bt2020
    } else {
        throw HelperError.message("camera frame uses unsupported color primaries '\(primaries)'")
    }

    let transfer = try requiredStringAttachment(
        pixelBuffer,
        key: kCVImageBufferTransferFunctionKey,
        name: "transfer function"
    )
    if transfer == kCVImageBufferTransferFunction_sRGB as String {
        return (encodedPrimaries, .sRGB)
    }
    if transfer == kCVImageBufferTransferFunction_ITU_R_709_2 as String
        || transfer == kCVImageBufferTransferFunction_ITU_R_2020 as String {
        return (encodedPrimaries, .bt709)
    }
    throw HelperError.message("camera frame uses unsupported transfer function '\(transfer)'")
}

private final class CaptureFailureObservers {
    private var tokens: [NSObjectProtocol] = []

    init(session: AVCaptureSession, device: AVCaptureDevice) {
        let notifications = NotificationCenter.default
        tokens.append(notifications.addObserver(
            forName: AVCaptureSession.runtimeErrorNotification,
            object: session,
            queue: nil
        ) { notification in
            if let error = notification.userInfo?[AVCaptureSessionErrorKey] as? NSError {
                diagnostic("capture session runtime error: \(error.localizedDescription)")
            } else {
                diagnostic("capture session runtime error")
            }
            Darwin.exit(EXIT_FAILURE)
        })
        tokens.append(notifications.addObserver(
            forName: AVCaptureSession.wasInterruptedNotification,
            object: session,
            queue: nil
        ) { _ in
            diagnostic("capture session was interrupted")
            Darwin.exit(sessionUnavailableExitCode)
        })
        tokens.append(notifications.addObserver(
            forName: AVCaptureDevice.wasDisconnectedNotification,
            object: device,
            queue: nil
        ) { _ in
            diagnostic("capture device was disconnected")
            Darwin.exit(sessionUnavailableExitCode)
        })
    }

    deinit {
        for token in tokens {
            NotificationCenter.default.removeObserver(token)
        }
    }
}

private final class CaptureDelegate: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private let expectedDimensions: Dimensions
    private let configuredFrameDuration: CMTime
    private var sequence: UInt64 = 0
    private var nativeDroppedTotal: UInt64 = 0

    init(expectedDimensions: Dimensions, configuredFrameDuration: CMTime) throws {
        guard validPositiveDuration(configuredFrameDuration) else {
            throw HelperError.message("configured frame duration is invalid")
        }
        self.expectedDimensions = expectedDimensions
        self.configuredFrameDuration = configuredFrameDuration
    }

    func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        do {
            try write(sampleBuffer)
        } catch {
            diagnostic("capture failed: \(error)")
            Darwin.exit(EXIT_FAILURE)
        }
    }

    func captureOutput(
        _ output: AVCaptureOutput,
        didDrop sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        let (next, overflow) = nativeDroppedTotal.addingReportingOverflow(1)
        nativeDroppedTotal = overflow ? UInt64.max : next
    }

    private func write(_ sampleBuffer: CMSampleBuffer) throws {
        let presentation = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        guard validTimestamp(presentation) else {
            throw HelperError.message("received an invalid or indefinite presentation timestamp")
        }
        let sampleDuration = CMSampleBufferGetDuration(sampleBuffer)
        let duration = validPositiveDuration(sampleDuration) ? sampleDuration : configuredFrameDuration
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else {
            throw HelperError.message("received a video sample without a pixel buffer")
        }
        guard CVPixelBufferGetPixelFormatType(pixelBuffer) == kCVPixelFormatType_32BGRA else {
            throw HelperError.message("received a non-BGRA pixel buffer")
        }
        let colorMetadata = try encodedColorMetadata(pixelBuffer)

        let lockResult = CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
        guard lockResult == kCVReturnSuccess else {
            throw HelperError.message("failed to lock pixel buffer (CoreVideo error \(lockResult))")
        }
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }

        let widthValue = CVPixelBufferGetWidth(pixelBuffer)
        let heightValue = CVPixelBufferGetHeight(pixelBuffer)
        let bytesPerRowValue = CVPixelBufferGetBytesPerRow(pixelBuffer)
        guard let width = UInt32(exactly: widthValue),
              let height = UInt32(exactly: heightValue),
              let bytesPerRow = UInt32(exactly: bytesPerRowValue),
              width > 0, height > 0, bytesPerRow > 0 else {
            throw HelperError.message("received malformed pixel buffer dimensions")
        }
        guard Dimensions(width: width, height: height) == expectedDimensions else {
            throw HelperError.message(
                "received \(width)x\(height), expected \(expectedDimensions.width)x\(expectedDimensions.height)"
            )
        }
        let (minimumRowBytes, rowSizeOverflow) = widthValue.multipliedReportingOverflow(by: 4)
        guard !rowSizeOverflow, bytesPerRowValue >= minimumRowBytes else {
            throw HelperError.message("pixel buffer row stride is smaller than its BGRA width")
        }

        let (payloadSize, payloadOverflow) = bytesPerRowValue.multipliedReportingOverflow(by: heightValue)
        guard !payloadOverflow,
              let payloadLength = UInt32(exactly: payloadSize),
              payloadLength <= maximumRecordLength - metadataLength,
              let baseAddress = CVPixelBufferGetBaseAddress(pixelBuffer) else {
            throw HelperError.message("pixel buffer size is invalid or exceeds the capture protocol limit")
        }
        let pixels = baseAddress.assumingMemoryBound(to: UInt8.self)
        for rowIndex in 0..<heightValue {
            let row = pixels.advanced(by: rowIndex * bytesPerRowValue)
            for columnIndex in 0..<widthValue where row[columnIndex * 4 + 3] != 255 {
                throw HelperError.message("camera BGRA frame contains nonopaque active pixels")
            }
        }

        let recordLength = metadataLength + payloadLength
        var record = Data(capacity: MemoryLayout<UInt32>.size + Int(recordLength))
        record.appendLittleEndian(recordLength)
        record.appendLittleEndian(sequence)
        record.appendLittleEndian(nativeDroppedTotal)
        record.appendLittleEndian(presentation.value)
        record.appendLittleEndian(presentation.timescale)
        record.appendLittleEndian(duration.value)
        record.appendLittleEndian(duration.timescale)
        record.appendLittleEndian(width)
        record.appendLittleEndian(height)
        record.appendLittleEndian(bytesPerRow)
        record.appendLittleEndian(payloadLength)
        record.append(colorMetadata.0.rawValue)
        record.append(colorMetadata.1.rawValue)
        record.append(pixels, count: payloadSize)
        try writeStdout(record)

        guard sequence < UInt64.max else {
            throw HelperError.message("capture sequence overflow")
        }
        sequence += 1
    }
}

private final class AudioCaptureDelegate: NSObject, AVCaptureAudioDataOutputSampleBufferDelegate {
    private let expectedFormat: EmittedAudioFormat
    private var sequence: UInt64 = 0

    init(expectedFormat: EmittedAudioFormat) {
        self.expectedFormat = expectedFormat
    }

    func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        do {
            try write(sampleBuffer)
        } catch {
            diagnostic("audio capture failed: \(error)")
            Darwin.exit(EXIT_FAILURE)
        }
    }

    private func write(_ sampleBuffer: CMSampleBuffer) throws {
        let presentation = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        guard validTimestamp(presentation) else {
            throw HelperError.message("received audio with an invalid presentation timestamp")
        }
        guard let formatDescription = CMSampleBufferGetFormatDescription(sampleBuffer),
              CMFormatDescriptionGetMediaType(formatDescription) == kCMMediaType_Audio,
              let description = CMAudioFormatDescriptionGetStreamBasicDescription(
                  formatDescription
              )?.pointee else {
            throw HelperError.message("received audio without a valid format description")
        }
        let requiredFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
        let forbiddenFlags = kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsNonInterleaved
        guard description.mFormatID == kAudioFormatLinearPCM,
              description.mFormatFlags & requiredFlags == requiredFlags,
              description.mFormatFlags & forbiddenFlags == 0,
              description.mBitsPerChannel == 32,
              description.mSampleRate == Double(expectedFormat.sampleRate),
              description.mChannelsPerFrame == UInt32(expectedFormat.channels),
              description.mBytesPerFrame == UInt32(expectedFormat.channels) * 4 else {
            throw HelperError.message("AVFoundation returned an unexpected audio representation")
        }

        let sampleCount = CMSampleBufferGetNumSamples(sampleBuffer)
        guard sampleCount > 0, sampleCount <= maximumAudioSamplesPerBlock,
              let sampleCount32 = UInt32(exactly: sampleCount) else {
            throw HelperError.message("audio sample count is outside the capture protocol bound")
        }
        let (payloadLength, overflow) = sampleCount32.multipliedReportingOverflow(
            by: UInt32(expectedFormat.channels) * 4
        )
        guard !overflow, payloadLength <= maximumAudioBlockBytes,
              let dataBuffer = CMSampleBufferGetDataBuffer(sampleBuffer),
              CMBlockBufferGetDataLength(dataBuffer) == Int(payloadLength) else {
            throw HelperError.message("audio payload length is invalid")
        }
        var payload = Data(count: Int(payloadLength))
        let copyStatus = payload.withUnsafeMutableBytes { destination in
            CMBlockBufferCopyDataBytes(
                dataBuffer,
                atOffset: 0,
                dataLength: Int(payloadLength),
                destination: destination.baseAddress!
            )
        }
        guard copyStatus == kCMBlockBufferNoErr else {
            throw HelperError.message("failed to copy audio payload (CoreMedia error \(copyStatus))")
        }

        let recordLength = audioMetadataLength + payloadLength
        var record = Data(capacity: MemoryLayout<UInt32>.size + Int(recordLength))
        record.appendLittleEndian(recordLength)
        record.appendLittleEndian(sequence)
        record.appendLittleEndian(UInt64(0))
        record.appendLittleEndian(presentation.value)
        record.appendLittleEndian(presentation.timescale)
        record.appendLittleEndian(expectedFormat.sampleRate)
        record.append(expectedFormat.channels)
        record.appendLittleEndian(sampleCount32)
        record.appendLittleEndian(payloadLength)
        record.append(payload)
        try writeStdout(record)

        guard sequence < UInt64.max else {
            throw HelperError.message("audio capture sequence overflow")
        }
        sequence += 1
    }
}

private func selectedAudioFormat(
    for device: AVCaptureDevice,
    requested: EmittedAudioFormat
) throws -> AVCaptureDevice.Format {
    guard emittedAudioFormats(for: device).contains(requested),
          let format = device.formats.first(where: { emittedAudioFormat($0) == requested }) else {
        throw HelperError.message(
            "requested audio format \(requested.sampleRate) Hz/\(requested.channels) channels was not emitted for device '\(device.uniqueID)'"
        )
    }
    return format
}

private func captureAudio(
    deviceID: String,
    sampleRate: UInt32,
    channels: UInt32
) throws -> Never {
    guard AVCaptureDevice.authorizationStatus(for: .audio) == .authorized else {
        throw HelperError.message("microphone access is not authorized; run request-audio-permission first")
    }
    guard let device = audioDevices().first(where: { $0.uniqueID == deviceID }) else {
        throw HelperError.message("no microphone has unique ID '\(deviceID)'")
    }
    guard sampleRate <= maximumAudioSampleRate,
          channels > 0,
          channels <= maximumAudioChannels,
          let channelCount = UInt8(exactly: channels) else {
        throw HelperError.message("unsupported requested audio format")
    }
    let requested = EmittedAudioFormat(sampleRate: sampleRate, channels: channelCount)
    let format = try selectedAudioFormat(for: device, requested: requested)
    let session = AVCaptureSession()
    let input = try AVCaptureDeviceInput(device: device)
    let output = AVCaptureAudioDataOutput()
    output.audioSettings = [
        AVFormatIDKey: kAudioFormatLinearPCM,
        AVLinearPCMBitDepthKey: 32,
        AVLinearPCMIsFloatKey: true,
        AVLinearPCMIsBigEndianKey: false,
        AVLinearPCMIsNonInterleaved: false,
        AVSampleRateKey: sampleRate,
        AVNumberOfChannelsKey: channels,
    ]
    let delegate = AudioCaptureDelegate(expectedFormat: requested)
    let queue = DispatchQueue(label: "fm-camera-helper.audio-capture")

    session.beginConfiguration()
    guard session.canAddInput(input) else {
        session.commitConfiguration()
        throw HelperError.message("AVFoundation refused the selected microphone input")
    }
    session.addInput(input)
    guard session.canAddOutput(output) else {
        session.commitConfiguration()
        throw HelperError.message("AVFoundation refused F32 audio output")
    }
    session.addOutput(output)
    do {
        try device.lockForConfiguration()
        device.activeFormat = format
        device.unlockForConfiguration()
    } catch {
        session.commitConfiguration()
        throw HelperError.message("failed to configure the microphone format: \(error)")
    }
    session.commitConfiguration()

    let failureObservers = CaptureFailureObservers(session: session, device: device)
    queue.suspend()
    output.setSampleBufferDelegate(delegate, queue: queue)
    session.startRunning()
    guard session.isRunning else {
        throw HelperError.message("AVFoundation did not start the audio capture session")
    }
    try writeStdout(audioCaptureMagic)
    queue.resume()
    withExtendedLifetime((session, input, output, delegate, queue, failureObservers)) {
        dispatchMain()
    }
}

private func capture(
    deviceID: String,
    width: UInt32,
    height: UInt32,
    frameRate: RationalFrameRate
) throws -> Never {
    guard AVCaptureDevice.authorizationStatus(for: .video) == .authorized else {
        throw HelperError.message("camera access is not authorized; run request-permission first")
    }
    guard let device = videoDevices().first(where: { $0.uniqueID == deviceID }) else {
        throw HelperError.message("no camera has unique ID '\(deviceID)'")
    }

    let requestedDimensions = Dimensions(width: width, height: height)
    let format = try selectedFormat(
        for: device,
        dimensions: requestedDimensions,
        frameRate: frameRate
    )
    let session = AVCaptureSession()
    let input = try AVCaptureDeviceInput(device: device)
    let output = AVCaptureVideoDataOutput()
    let frameDuration = frameRate.duration
    guard validPositiveDuration(frameDuration) else {
        throw HelperError.message("requested frame rate produced an invalid frame duration")
    }
    let delegate = try CaptureDelegate(
        expectedDimensions: requestedDimensions,
        configuredFrameDuration: frameDuration
    )
    let queue = DispatchQueue(label: "fm-camera-helper.capture")

    session.beginConfiguration()
    guard session.canAddInput(input) else {
        session.commitConfiguration()
        throw HelperError.message("AVFoundation refused the selected camera input")
    }
    session.addInput(input)

    output.videoSettings = [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA
    ]
    output.alwaysDiscardsLateVideoFrames = true
    guard session.canAddOutput(output) else {
        session.commitConfiguration()
        throw HelperError.message("AVFoundation refused BGRA video output")
    }
    session.addOutput(output)

    do {
        try device.lockForConfiguration()
        device.activeFormat = format
        device.activeVideoMinFrameDuration = frameDuration
        device.activeVideoMaxFrameDuration = frameDuration
        device.unlockForConfiguration()
    } catch {
        session.commitConfiguration()
        throw HelperError.message("failed to configure the camera format: \(error)")
    }
    session.commitConfiguration()

    let failureObservers = CaptureFailureObservers(session: session, device: device)
    session.startRunning()
    guard session.isRunning else {
        throw HelperError.message("AVFoundation did not start the capture session")
    }
    try writeStdout(captureMagic)
    output.setSampleBufferDelegate(delegate, queue: queue)

    withExtendedLifetime((session, input, output, delegate, queue, failureObservers)) {
        dispatchMain()
    }
}

signal(SIGPIPE, SIG_IGN)

do {
    let arguments = CommandLine.arguments
    guard arguments.count >= 2 else {
        throw HelperError.message("usage: CameraHelper discover | discover-audio | request-permission | request-audio-permission | capture <unique-id> <width> <height> <rate-numerator> <rate-denominator> | capture-audio <unique-id> <sample-rate> <channels>")
    }

    switch arguments[1] {
    case "discover":
        guard arguments.count == 2 else {
            throw HelperError.message("discover takes no arguments")
        }
        try discover()
    case "request-permission":
        guard arguments.count == 2 else {
            throw HelperError.message("request-permission takes no arguments")
        }
        try requestPermission()
    case "discover-audio":
        guard arguments.count == 2 else {
            throw HelperError.message("discover-audio takes no arguments")
        }
        try discoverAudio()
    case "request-audio-permission":
        guard arguments.count == 2 else {
            throw HelperError.message("request-audio-permission takes no arguments")
        }
        try requestAudioPermission()
    case "capture":
        guard arguments.count == 7 else {
            throw HelperError.message("usage: CameraHelper capture <unique-id> <width> <height> <rate-numerator> <rate-denominator>")
        }
        let width = try parsePositiveUInt32(arguments[3], name: "width")
        let height = try parsePositiveUInt32(arguments[4], name: "height")
        let numerator = try parsePositiveUInt32(arguments[5], name: "rate-numerator")
        let denominator = try parsePositiveUInt32(arguments[6], name: "rate-denominator")
        let frameRate = try validatedFrameRate(numerator: numerator, denominator: denominator)
        try capture(deviceID: arguments[2], width: width, height: height, frameRate: frameRate)
    case "capture-audio":
        guard arguments.count == 5 else {
            throw HelperError.message("usage: CameraHelper capture-audio <unique-id> <sample-rate> <channels>")
        }
        let sampleRate = try parsePositiveUInt32(arguments[3], name: "sample-rate")
        let channels = try parsePositiveUInt32(arguments[4], name: "channels")
        try captureAudio(deviceID: arguments[2], sampleRate: sampleRate, channels: channels)
    default:
        throw HelperError.message("unknown command '\(arguments[1])'")
    }
} catch {
    diagnostic(String(describing: error))
    Darwin.exit(EXIT_FAILURE)
}
