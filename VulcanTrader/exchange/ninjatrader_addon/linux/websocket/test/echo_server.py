"""Local WebSocket test server for the websocket.dll replacement.

Echoes every message back with the same type. Pings the client every 0.5 s
(the connection dies if the DLL doesn't answer with PONGs). Commands:
  SLOW         -> wait 3 s (several pings happen) then reply "SLOW-OK"
  CLOSEME      -> server-initiated close, code 4000 reason "server bye"
  FRAGMENTED   -> reply with a 3-fragment text message "part1part2part3"
"""
import asyncio
import sys

from websockets.asyncio.server import serve


async def handler(ws):
    async for msg in ws:
        if msg == "SLOW":
            await asyncio.sleep(3)
            await ws.send("SLOW-OK")
        elif msg == "CLOSEME":
            await ws.close(4000, "server bye")
            return
        elif msg == "FRAGMENTED":
            await ws.send(iter(["part1", "part2", "part3"]))
        else:
            await ws.send(msg)


async def main(port):
    async with serve(handler, "127.0.0.1", port, ping_interval=0.5, ping_timeout=2,
                     max_size=4 * 1024 * 1024):
        print(f"listening on {port}", flush=True)
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1])))
