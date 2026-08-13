#!/usr/bin/env python3
"""Judge the --3d tree borders against the 2D render, by pixels, not by eye.

A tree in the diorama is the game's own metatile artwork stood upright. Its
geometry is foreshortened, so the SHAPE cannot be compared with the 2D frame --
but its COLOURS can, and they are the whole complaint: the tilt-shift pass used
to blur neighbouring canopies into one another (you could see trees through
trees) and lift the saturation of every canopy pixel off the game's palette
(the washed-out pale green). Both show up as one number: a pixel whose colour
is not exactly a colour the 2D render of the same scene contains.

The tool runs the SAME scripted input twice, once with --3d and once without,
at the viewpoint Joseph plays from (north-west Pallet Town, standing left of
his house), and reports for each region of the frame how many pixels are off
the 2D palette. It also reads the renderer's own per-tree TREE trace lines,
which say how many pixels each tree unit painted and how many of them some
later pass changed.

Usage: python3 tools/treediff.py [--rom /tmp/rom.gba] [--binary ...]
"""
import argparse
import os
import subprocess
import sys
import tempfile

# Loads Joseph's save, dismisses the recap, then walks to the north-west corner
# of Pallet Town: down out of the doorway row, west along the town, north to the
# tree border. Frame 2020 is him standing left of his house with the west tree
# border down the left of the frame and the town's north tree row behind the
# fence -- his screenshot, cell for cell.
PREFIX = ("420-424:start,480-484:a,540-544:a,600-604:a,660-664:a,930-934:a,"
          "1230-1234:a,1290-1294:a,1350-1354:a,1410-1414:a")
NW_PALLET = "1520-1600:down,1620-1900:left,1920-2000:up"
NW_FRAME = 2020

# Rectangles of the 800x500 diorama frame that are tree border and nothing else
# (measured off the dump, and stable because the viewpoint is fixed).
REGIONS = {"west border": (295, 375, 60, 400), "north row": (390, 760, 50, 170)}


def readppm(path):
    tok = open(path).read().split()
    assert tok[0] == "P3", f"{path} is not an ASCII PPM"
    w, h = int(tok[1]), int(tok[2])
    return w, h, [int(v) for v in tok[4:4 + w * h * 3]]


def dump(binary, rom, three_d, frame, out, env_extra=None):
    """Run the scripted input and dump `frame`, returning the trace output."""
    env = dict(os.environ)
    env.update(GBA_INPUT=PREFIX + "," + NW_PALLET, GBA_FRAMES=str(frame),
               GBA_DUMP_FROM=str(frame - 2), GBA_DUMP_EVERY="2",
               GBA_DUMP_DIR=out, GBA_3D_TRACE="1")
    env.setdefault("GBA_3D_WALL", "1")
    if env_extra:
        env.update(env_extra)
    args = [binary, None, "--headless"] + (["--3d"] if three_d else [])
    # A headless exit rewrites <rom>.sav, so every run gets a private copy.
    with tempfile.TemporaryDirectory() as td:
        r = os.path.join(td, "rom.gba")
        subprocess.run(["cp", rom, r], check=True)
        if os.path.exists(rom + ".sav"):
            subprocess.run(["cp", rom + ".sav", r + ".sav"], check=True)
        args[1] = r
        p = subprocess.run(args, env=env, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, text=True)
    return p.stdout


def offpalette(path2d, path3d):
    """Per region: pixels whose colour the 2D render never draws."""
    _, _, flat = readppm(path2d)
    pal = {(flat[i], flat[i + 1], flat[i + 2]) for i in range(0, len(flat), 3)}
    w, _, dio = readppm(path3d)
    out = {}
    for name, (x0, x1, y0, y1) in REGIONS.items():
        total = off = 0
        for y in range(y0, y1):
            for x in range(x0, x1):
                i = (y * w + x) * 3
                total += 1
                off += (dio[i], dio[i + 1], dio[i + 2]) not in pal
        out[name] = (total, off)
    return out


def trees(trace):
    """The renderer's own account: (map cell, pixels painted, pixels changed)."""
    out = []
    for line in trace.splitlines():
        f = line.split()
        if f[:1] == ["TREE"]:
            out.append(((int(f[1]), int(f[2])), int(f[4]), int(f[6])))
    # The headless runner renders its last frame twice; keep the last set.
    seen, tail = set(), []
    for t in reversed(out):
        if t[0] in seen:
            break
        seen.add(t[0])
        tail.append(t)
    return list(reversed(tail))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rom", default="/tmp/rom.gba")
    ap.add_argument("--binary", default="./target/release/gba")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory() as td:
        d2, d3 = os.path.join(td, "flat"), os.path.join(td, "diorama")
        dump(args.binary, args.rom, False, NW_FRAME, d2)
        trace = dump(args.binary, args.rom, True, NW_FRAME, d3)
        name = f"frame{NW_FRAME:05d}.ppm"
        regions = offpalette(os.path.join(d2, name), os.path.join(d3, name))

    bad = False
    print(f"north-west Pallet Town, frame {NW_FRAME}")
    for region, (total, off) in regions.items():
        print(f"  {region:<14} {off:6d} / {total:6d} pixels off the 2D palette "
              f"({100 * off / total:.1f}%)")
    drawn = [t for t in trees(trace) if t[1] > 0]
    changed = [t for t in drawn if t[2]]
    for cell, painted, off in drawn:
        flag = "  <-- CHANGED" if off else ""
        print(f"  tree at map {cell[0]:>3},{cell[1]:>3}: {painted:6d} pixels, "
              f"{off:6d} not the colour the artwork painted{flag}")
    if not drawn:
        print("FAIL: no tree was drawn, so this proves nothing")
        bad = True
    if changed:
        print(f"FAIL: {len(changed)} of {len(drawn)} trees had pixels changed "
              "after they were painted")
        bad = True
    if not bad:
        print(f"PASS: {len(drawn)} trees, every pixel still exactly the "
              "colour the game's artwork put there")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
