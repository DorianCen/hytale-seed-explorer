#!/usr/bin/env python3
"""T03 color-path proof probe — NOT part of the explorer build.

Reproduces the documented per-block/fluid color formulas (the server's
`crates/hytale-server/src/map_image.rs`) against a user-supplied extracted
Hytale asset pack, printing the JSON fields it found and the resulting colors.

Usage:
    python3 tools/block_color_probe.py [pack-root]

    pack-root defaults to ../hytale-server-rust/Assets/extracted

It reads only JSON from the user's own pack; nothing is written. The numbers
it prints are recorded in docs/assets-and-palette.md §6.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# Sample biome tint for the BiomeTintUp=100 grass case: the Plains biome's
# "Tint" from Zone1_Tier1/Tile.Plains_Smooth.json (runtime value in the real
# renderer; fixed here so the probe is deterministic).
SAMPLE_BIOME_TINT = (0x63, 0x97, 0x26)  # "#639726"


def parse_color(value) -> tuple[int, int, int] | None:
    """map_image.rs `tint_color`/`color_value` rules: string or first array
    element, optional '#', 3 digits expanded, 6 digits direct, 8 digits take
    the last 6."""
    if value is None:
        return None
    if isinstance(value, list):
        if not value:
            return None
        value = value[0]
    if not isinstance(value, str):
        return None
    hex_digits = value.lstrip("#")
    if len(hex_digits) == 3:
        hex_digits = "".join(ch * 2 for ch in hex_digits)
    if len(hex_digits) not in (6, 8):
        return None
    hex_digits = hex_digits[-6:]
    try:
        return tuple(int(hex_digits[i : i + 2], 16) for i in (0, 2, 4))  # type: ignore[return-value]
    except ValueError:
        return None


def block_color(block_json: dict, biome_tint: tuple[int, int, int]):
    """map_image.rs `block_color` (map_image.rs:220-249).

    self = TintUp or Tint or white; m = BiomeTintUp/100 (default 0);
    c = self + (biome - self) * m; if m < 1: c = c * ParticleColor / 255.
    """
    self_tint = parse_color(block_json.get("TintUp")) or parse_color(block_json.get("Tint"))
    if self_tint is None:
        self_tint = (255, 255, 255)
    multiplier = block_json.get("BiomeTintUp")
    multiplier = (multiplier if isinstance(multiplier, (int, float)) else 0) / 100.0
    out = []
    for self_c, biome_c in zip(self_tint, biome_tint):
        out.append(int(self_c + (biome_c - self_c) * multiplier))
    fields_used = ["TintUp/Tint"] if self_tint != (255, 255, 255) else []
    if multiplier < 1.0:
        particle = parse_color(block_json.get("ParticleColor"))
        if particle:
            out = [c * p // 255 for c, p in zip(out, particle)]
            fields_used.append("ParticleColor")
    if isinstance(block_json.get("BiomeTintUp"), (int, float)):
        fields_used.append(f"BiomeTintUp={multiplier:.2f}")
    return tuple(out), fields_used


def fluid_tint(pack: Path, fluid_name: str, environment: str):
    """map_image.rs `blend_fluid` tint (map_image.rs:251-292): white *
    env WaterTint / 255 * fluid ParticleColor / 255; depth attenuation
    1/(depth+1) applies against the base color at render time."""
    tint = [255, 255, 255]
    env_path = next(pack.glob(f"Server/Environments/**/{environment}.json"), None)
    water_tint = None
    if env_path:
        env = json.loads(env_path.read_text())
        water_tint = parse_color(env.get("WaterTint"))
        if water_tint:
            tint = [t * w // 255 for t, w in zip(tint, water_tint)]
    fluid_path = next(pack.glob(f"Server/Item/Block/Fluids/{fluid_name}.json"), None)
    particle = None
    if fluid_path:
        fluid = json.loads(fluid_path.read_text())
        particle = parse_color(fluid.get("ParticleColor"))
        if particle:
            tint = [t * p // 255 for t, p in zip(tint, particle)]
    return tuple(tint), water_tint, particle, env_path


def main() -> int:
    pack = Path(sys.argv[1] if len(sys.argv) > 1 else "../hytale-server-rust/Assets/extracted")
    if not pack.is_dir():
        print(f"error: {pack} is not an extracted pack root", file=sys.stderr)
        return 1

    samples = ["Soil_Grass", "Soil_Sand", "Rock_Stone", "Soil_Snow", "Soil_Leaves"]
    print(f"pack: {pack}")
    print("block colors (map_image::block_color rules, biome tint #639726):")
    for name in samples:
        path = next(pack.glob(f"Server/Item/Items/**/{name}.json"), None)
        if path is None:
            print(f"  {name:20} MISSING")
            continue
        item = json.loads(path.read_text())
        block_json = item.get("BlockType", {})
        color, fields = block_color(block_json, SAMPLE_BIOME_TINT)
        print(f"  {name:20} rgb{color}  fields=[{', '.join(fields) or 'none -> white'}]")

    print("fluid tint (map_image::blend_fluid tint part):")
    tint, water_tint, particle, env_path = fluid_tint(pack, "Water_Source", "Env_Zone0")
    print(f"  Water_Source + Env_Zone0 -> tint rgb{tint}  WaterTint={water_tint} ParticleColor={particle}  env={env_path}")
    for depth in (0, 1, 3):
        base = (255, 255, 255)
        blended = tuple(
            int(t + (b - t) / (depth + 1.0)) for t, b in zip(tint, base)
        )
        print(f"    depth {depth}: rgb{blended}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
