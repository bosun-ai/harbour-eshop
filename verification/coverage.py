"""Read LLVM 22 raw counters without installing coverage tools.

This is counter coverage, not line/branch coverage. Fail closed on format changes.
Layout: compiler-rt/include/profile/InstrProfData.inc (LLVM release/22.x).
"""
import hashlib
from pathlib import Path
import struct
import sys
import zlib


def unsigned_leb(data, position):
    value = 0
    shift = 0
    while True:
        byte = data[position]
        position += 1
        value |= (byte & 127) << shift
        if not byte & 128:
            return value, position
        shift += 7


def profiles(directory):
    merged = {}
    for path in Path(directory).glob("*.profraw"):
        data = path.read_bytes()
        header = struct.unpack_from("<16Q", data)
        assert header[0] == 0xFF6C70726F667281 and header[1] == 10, "unsupported raw profile"
        records = 128 + header[2]
        counters = records + header[3] * 64 + header[4]
        names_start = counters + header[5] * 8 + header[6] + header[7] + header[8]
        names_end = names_start + header[9]
        position = names_start
        names = {}
        while position < names_end:
            size, position = unsigned_leb(data, position)
            compressed, position = unsigned_leb(data, position)
            payload = data[position:position + (compressed or size)]
            position += compressed or size
            if compressed:
                payload = zlib.decompress(payload)
            assert len(payload) == size
            for name in payload.split(b"\x01"):
                names[int.from_bytes(hashlib.md5(name).digest()[:8], "little")] = name.decode()
        for index in range(header[3]):
            offset = records + index * 64
            name_hash, function_hash, pointer = struct.unpack_from("<3Q", data, offset)
            count = struct.unpack_from("<I", data, offset + 48)[0]
            delta = (pointer - header[10] + index * 64) % (1 << 64)
            assert delta + count * 8 <= header[5] * 8
            values = struct.unpack_from(f"<{count}Q", data, counters + delta)
            name = names[name_hash]
            if "eshop_gateway" not in name:
                continue
            key = (name, function_hash)
            existing = merged.setdefault(key, [0] * count)
            for counter, value in enumerate(values):
                existing[counter] += value
    return merged


def main():
    merged = profiles(sys.argv[1])
    for module in ["config", "dispatch", "legacy", "server"]:
        values = [value for (name, _), counts in merged.items() if module in name and "tests" not in name for value in counts]
        hit = sum(value > 0 for value in values)
        assert hit > 0, f"no exercised counters in {module}"
        print(f"{module}: {hit}/{len(values)} counters exercised ({hit / len(values):.1%})")
    for function in ["select", "strip_hop_headers", "LegacyUpstream"]:
        assert any(function in name and any(counts) for (name, _), counts in merged.items()), function


if __name__ == "__main__":
    main()
