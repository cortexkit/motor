#!/usr/bin/env python3
"""Disposable probe bridge only. Production admission belongs to motor-host."""
import asyncio
import os


async def relay(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    finally:
        if writer.can_write_eof():
            writer.write_eof()


async def connection(reader, writer):
    upstream = None
    try:
        incoming, upstream = await asyncio.open_unix_connection("/var/lib/motor-boat/state/boat/runner.sock")
        await asyncio.gather(relay(reader, upstream), relay(incoming, writer))
    except (OSError, asyncio.CancelledError):
        pass
    finally:
        for stream in (upstream, writer):
            if stream:
                stream.close()
                await stream.wait_closed()


async def main():
    for name in list(os.environ):
        if name == 'ASCII_TOKEN' or name.startswith('BOAT_'):
            del os.environ[name]
    server = await asyncio.start_server(connection, "127.0.0.1", 19443)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
