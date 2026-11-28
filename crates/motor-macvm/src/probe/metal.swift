import Darwin
import Metal

func fail(_ reason: String) -> Never {
    print("metal: check failed: \(reason)")
    exit(1)
}

func checkMetal() {
    guard let device = MTLCreateSystemDefaultDevice() else {
        print("metal: no Metal device")
        return
    }
    print("metal: device: \(device.name)")

    let descriptor = MTLTextureDescriptor.texture2DDescriptor(
        pixelFormat: .rgba8Unorm, width: 1, height: 1, mipmapped: false
    )
    descriptor.storageMode = .private
    descriptor.usage = .renderTarget
    let rowBytes = 256
    guard let texture = device.makeTexture(descriptor: descriptor),
          let readback = device.makeBuffer(length: rowBytes, options: .storageModeShared),
          let queue = device.makeCommandQueue(), let command = queue.makeCommandBuffer() else {
        fail("could not allocate Metal resources")
    }

    let pass = MTLRenderPassDescriptor()
    let color = pass.colorAttachments[0]!
    color.texture = texture
    color.loadAction = .clear
    color.storeAction = .store
    color.clearColor = MTLClearColorMake(1, 0, 1, 1)
    guard let render = command.makeRenderCommandEncoder(descriptor: pass) else {
        fail("could not create a render encoder")
    }
    render.endEncoding()
    guard let blit = command.makeBlitCommandEncoder() else {
        fail("could not create a blit encoder")
    }
    blit.copy(
        from: texture, sourceSlice: 0, sourceLevel: 0,
        sourceOrigin: MTLOrigin(x: 0, y: 0, z: 0), sourceSize: MTLSize(width: 1, height: 1, depth: 1),
        to: readback, destinationOffset: 0, destinationBytesPerRow: rowBytes, destinationBytesPerImage: rowBytes
    )
    blit.endEncoding()
    command.commit()
    command.waitUntilCompleted()
    guard command.status == .completed else {
        fail("GPU command: \(String(describing: command.error))")
    }

    let bytes = readback.contents().assumingMemoryBound(to: UInt8.self)
    let pixel = (0..<4).map { bytes[$0] }
    print("metal: RGBA pixel: \(pixel)")
    guard pixel == [255, 0, 255, 255] else {
        fail("wrong pixel; expected [255, 0, 255, 255]")
    }
}

checkMetal()
