#!/usr/bin/env python3
"""Measure LLVM v10 raw execution counters without installing coverage tools.

This is counter coverage, not source-line/branch coverage. Format is checked
against LLVM 22 InstrProfData.inc (64-bit little-endian records). Reject other
formats rather than silently claim a result. Only eshop_gateway symbols count.
"""
import argparse
import hashlib
from pathlib import Path
import struct
import zlib


def uleb(data, offset):
    value, shift = 0, 0
    while True:
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, offset
        shift += 7


def counters(directory):
    result = {}
    for path in Path(directory).glob("*.profraw"):
        data = path.read_bytes()
        header = struct.unpack_from("<16Q", data)
        magic, version, binary_ids, records, before, count, after, bitmap, bitmap_pad, names_size, delta = header[:11]
        if magic != 18405209413953942145 or version != 10:
            raise ValueError("requires LLVM raw v10, 64-bit little-endian")
        record_start = 128 + binary_ids
        counter_start = record_start + records * 64 + before
        names_start = counter_start + count * 8 + after + bitmap + bitmap_pad
        names_data = data[names_start:names_start + names_size]
        names = {}
        offset = 0
        while offset < len(names_data):
            size, offset = uleb(names_data, offset)
            compressed_size, offset = uleb(names_data, offset)
            payload_size = compressed_size or size
            payload = names_data[offset:offset + payload_size]
            offset += payload_size
            payload = zlib.decompress(payload) if compressed_size else payload
            assert len(payload) == size
            for name in payload.split(b"\x01"):
                name_hash = int.from_bytes(hashlib.md5(name).digest()[:8], "little")
                names[name_hash] = name.decode()
        for index in range(records):
            name_hash, function_hash, pointer = struct.unpack_from("<3Q", data, record_start + index * 64)
            num_counters = struct.unpack_from("<I", data, record_start + index * 64 + 48)[0]
            name = names.get(name_hash, "")
            if "eshop_gateway" not in name:
                continue
            relative = (pointer - delta + index * 64) & ((1 << 64) - 1)
            assert relative + num_counters * 8 <= count * 8
            values = struct.unpack_from(f"<{num_counters}Q", data, counter_start + relative)
            for counter, value in enumerate(values):
                key = name, function_hash, counter
                result[key] = result.get(key, 0) + value
    assert result, "no gateway counters found"
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--tested", required=True)
    args = parser.parse_args()
    baseline, tested = counters(args.baseline), counters(args.tested)
    before = sum(value > 0 for value in baseline.values())
    after = sum(value > 0 for value in tested.values())
    assert after > before, "tests did not increase executed coverage"
    print(f"PASS: gateway execution-counter coverage increases {before}/{len(baseline)} -> {after}/{len(tested)} ({100 * after / len(tested):.1f}%). Not source-line coverage.")


if __name__ == "__main__":
    main()
