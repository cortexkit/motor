import Foundation
import Virtualization
import Darwin

// The installer needs the hardware model advertised by this exact restore image.
let arguments = CommandLine.arguments
if arguments.count != 3 {
    fatalError("usage: prepare-image <restore.ipsw> <new-image-directory>")
}
let restore = URL(fileURLWithPath: arguments[1])
let directory = URL(fileURLWithPath: arguments[2], isDirectory: true)
try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
var finished = false
var failure: Error?
VZMacOSRestoreImage.load(from: restore) { result in
    do {
        let image = try result.get()
        guard let configuration = image.mostFeaturefulSupportedConfiguration,
              configuration.hardwareModel.isSupported else {
            throw NSError(domain: "MotorImage", code: 1,
                          userInfo: [NSLocalizedDescriptionKey: "restore image has no supported hardware model"])
        }
        let hardware = configuration.hardwareModel
        try hardware.dataRepresentation.write(to: directory.appendingPathComponent("hardware-model"))
        try VZMacMachineIdentifier().dataRepresentation.write(to: directory.appendingPathComponent("machine-identifier"))
        _ = try VZMacAuxiliaryStorage(creatingStorageAt: directory.appendingPathComponent("auxiliary-storage"),
                                     hardwareModel: hardware, options: [])
        let disk = directory.appendingPathComponent("disk.img").path
        let fd = open(disk, O_CREAT | O_EXCL | O_RDWR, 0o600)
        guard fd >= 0 else { throw POSIXError(.EIO) }
        defer { close(fd) }
        guard ftruncate(fd, 96 * 1024 * 1024 * 1024) == 0 else { throw POSIXError(.ENOSPC) }
    } catch {
        failure = error
    }
    finished = true
}
while !finished {
    RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.05))
}
if let failure = failure { throw failure }
