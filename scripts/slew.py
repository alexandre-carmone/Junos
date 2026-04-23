#!/usr/bin/env python3
"""Send a goto command to Junos via WebSocket."""

import asyncio
import json
import sys

import websockets

WS_URL = "ws://127.0.0.1:4565/ws"
TARGET_RA_H = 6.0
TARGET_DEC_DEG = 45.0


async def main():
    ra = float(sys.argv[1]) if len(sys.argv) > 1 else TARGET_RA_H
    dec = float(sys.argv[2]) if len(sys.argv) > 2 else TARGET_DEC_DEG

    async with websockets.connect(WS_URL) as ws:
        # Drain initial dump, find mount device
        mount = None
        deadline = asyncio.get_event_loop().time() + 5
        while asyncio.get_event_loop().time() < deadline:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=2)
                evt = json.loads(msg)
                if evt.get("type") == "DefNumber" and evt.get("name") == "EQUATORIAL_EOD_COORD":
                    mount = evt["device"]
                    break
            except asyncio.TimeoutError:
                break

        if not mount:
            print("No mount found")
            return

        print(f"Mount: {mount}")

        # Send goto command
        await ws.send(json.dumps({
            "type": "MountGoto",
            "device": mount,
            "ra_h": ra,
            "dec_deg": dec,
        }))
        print(f"Goto sent: RA={ra}h DEC={dec}deg")

        # Wait for slew complete
        while True:
            msg = await asyncio.wait_for(ws.recv(), timeout=120)
            evt = json.loads(msg)
            if (evt.get("type") == "SetNumber"
                    and evt.get("name") == "EQUATORIAL_EOD_COORD"
                    and evt.get("state") == "Ok"):
                elems = {e["name"]: e["value"] for e in evt["elements"]}
                print(f"Slew complete: RA={elems.get('RA', '?')}h DEC={elems.get('DEC', '?')}deg")
                break


if __name__ == "__main__":
    asyncio.run(main())
